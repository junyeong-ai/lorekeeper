"""Isolated binary document projection. The Rust caller owns IO limits and publication."""

import contextlib
import datetime
import importlib.metadata
import hashlib
import io
import json
import os
import pathlib
import sys
import warnings
import zipfile

POLICY = "document-projection-v1"
PACKAGES = {"kreuzberg": "4.10.4", "openpyxl": "3.1.5", "defusedxml": "0.7.1", "et-xmlfile": "2.0.0", "pillow": "13.0.0"}
MIMES = {
    "docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "pdf": "application/pdf",
    "png": "image/png",
    "jpeg": "image/jpeg",
    "tiff": "image/tiff",
    "webp": "image/webp",
}


class ProjectionError(Exception):
    def __init__(self, code):
        self.code = code


def unit(location, text, method="native", representation="text"):
    return {"location": location, "method": method, "representation": representation, "text": text}


def model_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(65536):
            digest.update(block)
    return digest.hexdigest()


def json_value(value):
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, (datetime.date, datetime.time, datetime.datetime)):
        return value.isoformat()
    if isinstance(value, datetime.timedelta):
        return {"seconds": value.total_seconds()}
    raise ProjectionError("unsupported-content")


def check_archive(data, limit):
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        members = archive.infolist()
        if len(members) > 10000 or sum(member.file_size for member in members) > limit:
            raise ProjectionError("decoded-limit")
        # Read each member to verify actual expansion and CRC, not just ZIP headers.
        remaining = limit
        for member in members:
            with archive.open(member) as stream:
                while block := stream.read(min(65536, remaining + 1)):
                    remaining -= len(block)
                    if remaining < 0:
                        raise ProjectionError("decoded-limit")


def spreadsheet(data, limit, max_units):
    import openpyxl
    from openpyxl.xml import DEFUSEDXML
    from openpyxl.worksheet.formula import ArrayFormula, DataTableFormula

    if not DEFUSEDXML:
        raise ProjectionError("dependency")
    units = []
    size = 0
    with contextlib.ExitStack() as stack:
        formulas = openpyxl.load_workbook(io.BytesIO(data), read_only=True, data_only=False, keep_links=False)
        stack.callback(formulas.close)
        cached = openpyxl.load_workbook(io.BytesIO(data), read_only=True, data_only=True, keep_links=False)
        stack.callback(cached.close)
        if len(formulas.sheetnames) > max_units:
            raise ProjectionError("unit-limit")
        for sheet in formulas:
            values = cached[sheet.title]
            # Read actual XML rows; producer-written dimension metadata can be wrong.
            sheet.reset_dimensions()
            values.reset_dimensions()
            records = []
            for row, cached_row in zip(sheet.iter_rows(), values.iter_rows(), strict=True):
                for cell, stored in zip(row, cached_row, strict=True):
                    if cell.value is None:
                        continue
                    record = {"sheet": sheet.title, "cell": cell.coordinate, "type": cell.data_type}
                    value = cell.value
                    if cell.data_type == "f":
                        if isinstance(value, ArrayFormula):
                            record["formula"] = {"text": value.text, "range": value.ref}
                        elif isinstance(value, DataTableFormula):
                            record["formula"] = dict(value)
                        elif isinstance(value, str):
                            record["formula"] = value
                        else:
                            raise ProjectionError("unsupported-content")
                        record["cached_value"] = json_value(stored.value)
                    else:
                        record["value"] = json_value(value)
                    if cell.number_format != "General":
                        record["number_format"] = cell.number_format
                    line = json.dumps(record, ensure_ascii=False, allow_nan=False)
                    size += len(line.encode("utf-8")) + 1
                    if size > limit:
                        raise ProjectionError("decoded-limit")
                    records.append(line)
            state = {"visible": "visible", "hidden": "hidden", "veryHidden": "very-hidden"}[sheet.sheet_state]
            units.append(unit({"kind": "sheet", "name": sheet.title, "state": state}, "\n".join(records), representation="json-lines"))
    return units, ["Spreadsheet formulas are preserved with stored cached values; they were not recalculated. Charts, drawings, comments and external linked workbooks are not projected."]


def image_frames(data, options):
    from PIL import Image, ImageOps, ImageSequence

    Image.MAX_IMAGE_PIXELS = max(1, options["max_decoded_bytes"] // 4)
    remaining = options["max_decoded_bytes"]
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", Image.DecompressionBombWarning)
            with Image.open(io.BytesIO(data)) as image:
                expected = {"png": "PNG", "jpeg": "JPEG", "tiff": "TIFF", "webp": "WEBP"}[options["format"]]
                if image.format != expected:
                    raise ProjectionError("format-mismatch")
                for number, source in enumerate(ImageSequence.Iterator(image), 1):
                    if number > options["max_units"]:
                        raise ProjectionError("unit-limit")
                    remaining -= source.width * source.height * 4
                    if remaining < 0:
                        raise ProjectionError("decoded-limit")
                    frame = ImageOps.exif_transpose(source).convert("RGBA")
                    background = Image.new("RGBA", frame.size, "white")
                    background.alpha_composite(frame)
                    encoded = io.BytesIO()
                    background.convert("RGB").save(encoded, format="PNG")
                    yield number, encoded.getvalue()
    except (Image.DecompressionBombWarning, Image.DecompressionBombError):
        raise ProjectionError("decoded-limit") from None


def pdf_pages(result):
    pages = result.pages
    expected = result.metadata.get("pages", {}).get("total_count")
    if pages is None or expected is None or len(pages) != expected or [page["page_number"] for page in pages] != list(range(1, expected + 1)):
        raise ProjectionError("invalid-projection")
    return pages


def extraction(data, options):
    import kreuzberg as k

    kind = options["format"]
    limit = options["max_decoded_bytes"]
    config = k.ExtractionConfig(
        use_cache=False,
        enable_quality_processing=False,
        disable_ocr=True,
        postprocessor=k.PostProcessorConfig(enabled=False),
        pages=k.PageConfig(extract_pages=True),
        output_format="markdown",
        extraction_timeout_secs=options["timeout_seconds"],
    )
    config.security_limits = {
        "max_archive_size": limit,
        "max_content_size": limit,
        "max_compression_ratio": limit,
        "max_files_in_archive": 10000,
        "max_table_cells": 1000000,
    }
    ocr = options.get("ocr")
    def ocr_config():
        language = "+".join(ocr["languages"])
        psm = {"auto": 3, "single-block": 6, "sparse-text": 11}[ocr["layout"]]
        return k.OcrConfig(backend="tesseract", language=language, tesseract_config=k.TesseractConfig(language=language, psm=psm, use_cache=False))
    image = kind in ("png", "jpeg", "tiff", "webp")
    if image and ocr is None:
        raise ProjectionError("ocr-not-configured")
    if image:
        config.disable_ocr = False
        config.ocr = ocr_config()
        units = []
        size = 0
        for number, frame in image_frames(data, options):
            result = k.extract_bytes_sync(frame, "image/png", config)
            size += len(result.content.encode("utf-8"))
            if size > limit:
                raise ProjectionError("decoded-limit")
            units.append(unit({"kind": "image-frame", "number": number}, result.content, method="ocr", representation="markdown"))
        return units, ["Every decoded image frame is projected in stored order with EXIF orientation applied and transparency composited on white. OCR recognizes text only; diagrams and recognition accuracy require separate inspection."]
    result = k.extract_bytes_sync(data, MIMES[kind], config)
    warnings = []
    if result.processing_warnings:
        # Libraries may put source text in diagnostics. Keep the condition, not that text.
        warnings.append("The decoder reported processing warnings; inspect the original before relying on this projection.")
    if kind == "pdf":
        pages = pdf_pages(result)
        if len(pages) * (2 if ocr is not None and ocr["pdf"] else 1) > options["max_units"]:
            raise ProjectionError("unit-limit")
        units = [unit({"kind": "page", "number": page["page_number"]}, page["content"], representation="markdown") for page in pages]
        warnings.append("PDF native text follows the decoder's reading order; diagrams and image content require separate inspection.")
        if ocr is not None and ocr["pdf"]:
            config.disable_ocr = False
            config.force_ocr = True
            config.ocr = ocr_config()
            recognized = k.extract_bytes_sync(data, MIMES[kind], config)
            recognized_pages = pdf_pages(recognized)
            if len(recognized_pages) != len(pages):
                raise ProjectionError("invalid-projection")
            units.extend(unit({"kind": "page", "number": page["page_number"]}, page["content"], method="ocr", representation="markdown") for page in recognized_pages)
            warnings.append("OCR is an additional interpretation, not a replacement for native PDF text.")
    else:
        units = [unit({"kind": "document"}, result.content, representation="markdown")]
        warnings.append("DOCX is projected as a flow document without physical page numbers. Embedded objects, comments and revision history are not projected.")
    return units, warnings


def project(options, data):
    for package, expected in PACKAGES.items():
        if importlib.metadata.version(package) != expected:
            raise ProjectionError("dependency-version")
    if options["format"] not in MIMES:
        raise ProjectionError("unsupported-format")
    uses_ocr = options["format"] in ("png", "jpeg", "tiff", "webp") or (options["format"] == "pdf" and (options.get("ocr") or {}).get("pdf", False))
    ocr = options.get("ocr") if uses_ocr else None
    models = {}
    if ocr is not None:
        directory = pathlib.Path(ocr["tessdata_dir"])
        for language in ocr["languages"]:
            model = directory / f"{language}.traineddata"
            if not model.is_file():
                raise ProjectionError("ocr-data-missing")
            models[language] = model_digest(model)
        os.environ["TESSDATA_PREFIX"] = str(directory)
    limit = options["max_decoded_bytes"]
    if options["format"] in ("docx", "xlsx"):
        check_archive(data, limit)
    if options["format"] == "xlsx":
        units, warnings = spreadsheet(data, limit, options["max_units"])
    else:
        units, warnings = extraction(data, options)
    if sum(len(part["text"].encode("utf-8")) for part in units) > limit:
        raise ProjectionError("decoded-limit")
    if len(units) > options["max_units"]:
        raise ProjectionError("unit-limit")
    if ocr is not None:
        directory = pathlib.Path(ocr["tessdata_dir"])
        try:
            changed = any(model_digest(directory / f"{language}.traineddata") != digest for language, digest in models.items())
        except OSError:
            changed = True
        if changed:
            raise ProjectionError("ocr-data-changed")
    if not any(part["text"].strip() for part in units):
        raise ProjectionError("no-text")
    return {"policy": POLICY, "format": options["format"], "packages": PACKAGES, "ocr": {"languages": ocr["languages"], "layout": ocr["layout"], "models": models} if ocr is not None else None, "units": units, "warnings": warnings}


def main():
    options = json.loads(sys.argv[1])
    if options.get("check"):
        for package, version in PACKAGES.items():
            if importlib.metadata.version(package) != version:
                raise ProjectionError("dependency-version")
        import kreuzberg
        import openpyxl.xml
        from PIL import Image, features
        if not openpyxl.xml.DEFUSEDXML:
            raise ProjectionError("dependency")
        if not all(features.check(name) for name in ("jpg", "zlib", "libtiff", "webp")):
            raise ProjectionError("dependency")
        models = {}
        ocr = options.get("ocr")
        if ocr is not None:
            for language in ocr["languages"]:
                model = pathlib.Path(ocr["tessdata_dir"]) / f"{language}.traineddata"
                if not model.is_file():
                    raise ProjectionError("ocr-data-missing")
                models[language] = model_digest(model)
        return {"status": "ready", "runtime": {"packages": PACKAGES, "formats": list(MIMES), "ocr_models": models}}
    data = sys.stdin.buffer.read(options["max_input_bytes"] + 1)
    if len(data) > options["max_input_bytes"]:
        raise ProjectionError("input-limit")
    return {"status": "projected", "projection": project(options, data)}


if __name__ == "__main__":
    try:
        response = main()
    except ProjectionError as error:
        response = {"status": "failed", "code": error.code}
    except importlib.metadata.PackageNotFoundError:
        response = {"status": "failed", "code": "dependency"}
    except ImportError:
        response = {"status": "failed", "code": "dependency"}
    except Exception:
        # Error messages from parsers can contain private document content.
        response = {"status": "failed", "code": "decoder"}
    sys.stdout.write(json.dumps(response, ensure_ascii=False, allow_nan=False))
