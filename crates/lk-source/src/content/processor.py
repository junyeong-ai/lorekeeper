"""Isolated binary document projection. The Rust caller owns IO limits and publication."""

import contextlib
import datetime
import importlib.metadata
import hashlib
import io
import json
import math
import os
import pathlib
import posixpath
import stat
import sys
import warnings
import zipfile

POLICY = "document-projection-v3"
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
    with zipfile.ZipFile(data) as archive:
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
    from openpyxl.cell.read_only import ReadOnlyCell
    from openpyxl.reader.excel import ExcelReader
    from openpyxl.utils.cell import range_boundaries
    from openpyxl.worksheet._reader import WorkSheetParser
    from openpyxl.xml import DEFUSEDXML
    from openpyxl.worksheet.formula import ArrayFormula, DataTableFormula

    if not DEFUSEDXML:
        raise ProjectionError("dependency")

    max_rows, max_columns = 1048576, 16384

    def index(value, maximum):
        value = int(value)
        if not 1 <= value <= maximum:
            raise ProjectionError("invalid-projection")
        return value

    def hidden(value):
        if value not in ("0", "1", "false", "true"):
            raise ProjectionError("invalid-projection")
        return value in ("1", "true")

    class SparseParser(WorkSheetParser):
        # The pinned parser retains formulas and cached-value decoding without
        # materializing blank cells between sparse row/column coordinates.
        def __init__(self, source, sheet, preserve_structure):
            super().__init__(source, sheet._shared_strings,
                             data_only=sheet.parent.data_only, epoch=sheet.parent.epoch,
                             date_formats=sheet.parent._date_formats,
                             timedelta_formats=sheet.parent._timedelta_formats)
            self.preserve_structure = preserve_structure
            self.row_visibility = []
            self.column_visibility = []
            self.previous_row = 0
            self.structure_bytes = 0

        def account(self, record):
            self.structure_bytes += len(json.dumps(record).encode("utf-8"))
            if self.structure_bytes > limit:
                raise ProjectionError("decoded-limit")

        def parse_column_dimensions(self, column):
            start = index(column.get("min"), max_columns)
            end = index(column.get("max"), max_columns)
            if end < start:
                raise ProjectionError("invalid-projection")
            if self.preserve_structure:
                record = {"start": start, "end": end, "hidden": hidden(column.get("hidden", "0"))}
                self.account(record)
                self.column_visibility.append(record)

        def parse_row(self, row):
            number, cells = super().parse_row(row)
            index(number, max_rows)
            if number <= self.previous_row:
                raise ProjectionError("invalid-projection")
            self.previous_row = number
            previous_column = 0
            for cell in cells:
                column = index(cell["column"], max_columns)
                if cell["row"] != number or column <= previous_column:
                    raise ProjectionError("invalid-projection")
                previous_column = column
            if self.preserve_structure:
                visibility = hidden(row.get("hidden", "0"))
                if (self.row_visibility and self.row_visibility[-1]["end"] + 1 == number
                        and self.row_visibility[-1]["hidden"] == visibility):
                    self.row_visibility[-1]["end"] = number
                else:
                    record = {"start": number, "end": number, "hidden": visibility}
                    self.account(record)
                    self.row_visibility.append(record)
            # WorkSheetParser's dimension dictionary is unnecessary for this
            # sparse projection and otherwise grows with every styled row.
            self.row_dimensions.clear()
            return number, cells

        def structure(self):
            ranges = []
            for merged in self.merged_cells.mergeCell if self.merged_cells is not None else ():
                first_column, first_row, last_column, last_row = range_boundaries(merged.ref)
                record = {
                    "start_row": index(first_row, max_rows), "end_row": index(last_row, max_rows),
                    "start_column": index(first_column, max_columns), "end_column": index(last_column, max_columns),
                }
                if first_row > last_row or first_column > last_column:
                    raise ProjectionError("invalid-projection")
                self.account(record)
                ranges.append(record)
            return {"default_row_hidden": bool(getattr(getattr(self, "sheet_format", None), "zeroHeight", False)),
                    "merged_ranges": ranges, "row_visibility": self.row_visibility,
                    "column_visibility": self.column_visibility}

    def load(stream, data_only):
        reader = ExcelReader(stream, read_only=True, data_only=data_only, keep_links=False)
        reader.read()
        try:
            if len(set(reader.valid_files)) != len(reader.valid_files):
                raise ProjectionError("invalid-projection")
            worksheets = []
            for declared in reader.parser.sheets:
                if not declared.id:
                    raise ProjectionError("invalid-projection")
                relation = reader.parser.rels[declared.id]
                if relation.Type.endswith("/chartsheet"):
                    continue
                target = relation.target
                if (not relation.Type.endswith("/worksheet") or relation.TargetMode == "External"
                        or not target or target.startswith("/") or "\\" in target
                        or posixpath.normpath(target) != target or target.startswith("../")
                        or target not in reader.valid_files):
                    raise ProjectionError("invalid-projection")
                worksheets.append((declared.name, target))
            if worksheets != [(sheet.title, sheet._worksheet_path) for sheet in reader.wb.worksheets]:
                raise ProjectionError("invalid-projection")
            return reader.wb
        except Exception:
            reader.wb.close()
            raise

    units = []
    size = 0
    with contextlib.ExitStack() as stack:
        formula_input = stack.enter_context(data.open("rb"))
        formulas = load(formula_input, False)
        stack.callback(formulas.close)
        cached_input = stack.enter_context(data.open("rb"))
        cached = load(cached_input, True)
        stack.callback(cached.close)
        if len(formulas.sheetnames) > max_units:
            raise ProjectionError("unit-limit")
        for sheet in formulas:
            values = cached[sheet.title]
            formula_source = stack.enter_context(sheet._get_source())
            cached_source = stack.enter_context(values._get_source())
            formula_parser = SparseParser(formula_source, sheet, True)
            cached_parser = SparseParser(cached_source, values, False)
            records = []
            for (number, row), (cached_number, cached_row) in zip(formula_parser.parse(), cached_parser.parse(), strict=True):
                if number != cached_number:
                    raise ProjectionError("invalid-projection")
                for cell, stored in zip(row, cached_row, strict=True):
                    if (cell["row"], cell["column"]) != (stored["row"], stored["column"]):
                        raise ProjectionError("invalid-projection")
                    cell = ReadOnlyCell(sheet, **cell)
                    stored = ReadOnlyCell(values, **stored)
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
            formula_source.close()
            cached_source.close()
            structure = formula_parser.structure()
            size += len(json.dumps(structure, ensure_ascii=False).encode("utf-8"))
            if size > limit:
                raise ProjectionError("decoded-limit")
            state = {"visible": "visible", "hidden": "hidden", "veryHidden": "very-hidden"}[sheet.sheet_state]
            projected = unit({"kind": "sheet", "name": sheet.title, "state": state}, "\n".join(records), representation="json-lines")
            projected["sheet_structure"] = structure
            units.append(projected)
    return units, ["Spreadsheet formulas are preserved with stored cached values; they were not recalculated. Merged ranges and declared row/column visibility are preserved as inclusive coordinates; hidden values remain included. Charts, drawings, comments and external linked workbooks are not projected."]


def image_frames(data, options):
    from PIL import Image, ImageOps, ImageSequence

    Image.MAX_IMAGE_PIXELS = max(1, options["max_decoded_bytes"] // 4)
    remaining = options["max_decoded_bytes"]
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", Image.DecompressionBombWarning)
            with Image.open(data) as image:
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
                    yield number, encoded.getvalue(), source.width * source.height * 4
    except (Image.DecompressionBombWarning, Image.DecompressionBombError):
        raise ProjectionError("decoded-limit") from None


def pdf_pages(result):
    pages = result.pages
    expected = result.metadata.get("pages", {}).get("total_count")
    if pages is None or expected is None or len(pages) != expected or [page["page_number"] for page in pages] != list(range(1, expected + 1)):
        raise ProjectionError("invalid-projection")
    return pages


def validate_pdf_raster_budget(result, limit, dpi):
    pages = result.metadata.get("pages", {}).get("pages")
    if not isinstance(pages, list) or len(pages) != len(result.pages):
        raise ProjectionError("invalid-projection")
    scale = dpi / 72
    for number, page in enumerate(pages, 1):
        dimensions = page.get("dimensions")
        if (page.get("number") != number or not isinstance(dimensions, list) or len(dimensions) != 2
                or any(not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0 for value in dimensions)):
            raise ProjectionError("invalid-projection")
        width, height = [math.ceil(value * scale) for value in dimensions]
        # The pinned renderer rescales larger dimensions; refuse that change to
        # the declared DPI rather than silently using a lower resolution.
        if width > 65536 or height > 65536:
            raise ProjectionError("decoded-limit")
        limit -= width * height * 4
        if limit < 0:
            raise ProjectionError("decoded-limit")


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
        return k.OcrConfig(backend="tesseract", language=language,
                           tesseract_config=k.TesseractConfig(language=language, psm=psm, use_cache=False,
                                                           output_format="text", enable_table_detection=False))
    def enable_ocr():
        config.disable_ocr = False
        config.ocr = ocr_config()
        config.output_format = "plain"
    def recognize(location, frame):
        result = k.extract_bytes_sync(frame, "image/png", config)
        if result.mime_type != "image/png":
            raise ProjectionError("invalid-projection")
        return unit(location, result.content, method="ocr", representation="text")
    image = kind in ("png", "jpeg", "tiff", "webp")
    if image and ocr is None:
        raise ProjectionError("ocr-not-configured")
    if image:
        enable_ocr()
        units = []
        size = 0
        for number, frame, _ in image_frames(data, options):
            recognized = recognize({"kind": "image-frame", "number": number}, frame)
            size += len(recognized["text"].encode("utf-8"))
            if size > limit:
                raise ProjectionError("decoded-limit")
            units.append(recognized)
        return units, ["Every decoded image frame is projected in stored order with EXIF orientation applied and transparency composited on white. OCR preserves literal recognized text without dictionary-based line deletion or table reconstruction; layout, diagrams and recognition accuracy require separate inspection."]
    result = k.extract_file_sync(data, MIMES[kind], config)
    if result.mime_type != MIMES[kind]:
        raise ProjectionError("invalid-projection")
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
            dpi = 150
            validate_pdf_raster_budget(result, limit, dpi)
            enable_ocr()
            size = sum(len(part["text"].encode("utf-8")) for part in units)
            remaining_raster = limit
            for number in range(1, len(pages) + 1):
                rendered = k.render_pdf_page(data, number - 1, dpi=dpi)
                if len(rendered) > limit:
                    raise ProjectionError("decoded-limit")
                image_options = {**options, "format": "png", "max_decoded_bytes": remaining_raster, "max_units": 1}
                for _, frame, decoded_bytes in image_frames(io.BytesIO(rendered), image_options):
                    remaining_raster -= decoded_bytes
                    recognized = recognize({"kind": "page", "number": number}, frame)
                    size += len(recognized["text"].encode("utf-8"))
                    if size > limit:
                        raise ProjectionError("decoded-limit")
                    units.append(recognized)
            warnings.append("Every PDF page is rendered at 150 DPI for additional literal OCR text; native text remains separate. OCR does not reconstruct tables or delete lines using dictionaries. Inspect the original for layout and recognition accuracy.")
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
    data = pathlib.Path(options["input_path"])
    metadata = data.stat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > options["max_input_bytes"]:
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
