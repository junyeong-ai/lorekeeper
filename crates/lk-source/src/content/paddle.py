"""Official Paddle page layout with bounded requests to an owned recognition worker."""

PACKAGES = {
    "paddleocr": "3.7.0", "paddlex": "3.7.2", "paddlepaddle": "3.2.1",
    "pypdfium2": "5.14.0", "pillow": "13.0.0", "opencv-contrib-python": "4.10.0.84",
}


def inventory(path, format_name, options):
    units = []
    if format_name == "pdf":
        import pypdfium2 as pdfium

        scale = RECIPE["dpi"] / 72
        with pdfium.PdfDocument(path) as document:
            if len(document) > options["max_units"]:
                raise ProjectionError("unit-limit")
            for number in range(1, len(document) + 1):
                page = document[number - 1]
                try:
                    width, height = page.get_size()
                    width, height = geometry(math.ceil(width * scale), math.ceil(height * scale))
                finally:
                    page.close()
                units.append({"location": {"kind": "page", "number": number}, "width": width, "height": height})
    else:
        from PIL import Image

        formats = {"png": "PNG", "jpeg": "JPEG", "tiff": "TIFF", "webp": "WEBP"}
        if format_name not in formats:
            raise ProjectionError("unsupported-format")
        with Image.open(path) as image:
            if image.format != formats[format_name]:
                raise ProjectionError("format-mismatch")
            count = getattr(image, "n_frames", 1)
            if count > options["max_units"]:
                raise ProjectionError("unit-limit")
            for number in range(1, count + 1):
                if format_name == "tiff":
                    image.seek(number - 1)
                width, height = image.size
                width, height = geometry(width, height)
                units.append({"location": {"kind": "image-frame", "number": number}, "width": width, "height": height})
    if not units:
        raise ProjectionError("invalid-projection")
    return units


def render(path, format_name, number):
    from PIL import Image, ImageOps

    if format_name == "pdf":
        import pypdfium2 as pdfium

        with pdfium.PdfDocument(path) as document:
            page = document[number - 1]
            try:
                bitmap = page.render(scale=RECIPE["dpi"] / 72)
                try:
                    return bitmap.to_pil().convert("RGB").copy()
                finally:
                    bitmap.close()
            finally:
                page.close()
    with Image.open(path) as image:
        image.seek(number - 1)
        with ImageOps.exif_transpose(image).convert("RGBA") as frame:
            with Image.new("RGBA", frame.size, "white") as background:
                background.alpha_composite(frame)
                return background.convert("RGB")


def build_pipeline(root, options, output):
    import numpy as np
    from PIL import Image
    from paddleocr import PaddleOCRVL
    from paddlex.inference import load_pipeline_config
    from paddlex.inference.common.batch_sampler import DocVLMBatchSampler
    from paddlex.inference.models.bindings import register_predictor_binding_map
    from paddlex.inference.models.doc_vlm.result import DocVLMResult
    from paddlex.inference.models.doc_vlm.utils import format_doc_vlm_result_dict
    from paddlex.inference.models.engines import InferenceEngine
    from paddlex.inference.models.predictors import BasePredictor

    class CheckedMlxEngine(InferenceEngine):
        entities = "lore-mlx-vlm"

        @property
        def name(self):
            return "lore-mlx-vlm"

        @property
        def needs_local_model(self):
            return False

    class CheckedMlxPredictor(BasePredictor):
        def __init__(self, **kwargs):
            super().__init__(**kwargs)
            self.remaining = options["max_decoded_bytes"]

        def _build_batch_sampler(self):
            return DocVLMBatchSampler(self.model_name)

        def _get_result_class(self):
            return DocVLMResult

        def process(self, data, **kwargs):
            predictions = []
            for item in data:
                with Image.fromarray(np.ascontiguousarray(item["image"][:, :, ::-1])) as image:
                    if image.width * image.height * 4 > options["max_raster_bytes"]:
                        raise ProjectionError("decoded-limit")
                    with io.BytesIO() as encoded:
                        image.save(encoded, format="PNG")
                        payload = base64.b64encode(encoded.getvalue()).decode("ascii")
                    output.emit({"status": "recognize", "image": payload, "query": item.get("query", "")}, message_limit=options["max_raster_bytes"] * 2 + 1024 * 1024)
                reply = read_message(options["max_output_bytes"])
                if reply is None or set(reply) != {"content", "finish_reason"} or reply["finish_reason"] != "stop" or not isinstance(reply["content"], str):
                    raise ProjectionError("incomplete-vision")
                self.remaining -= len(reply["content"].encode("utf-8"))
                if self.remaining < 0:
                    raise ProjectionError("decoded-limit")
                predictions.append(reply["content"])
            return format_doc_vlm_result_dict(predictions, data, add_input_path=True)

    register_predictor_binding_map(CheckedMlxPredictor, {"lore-mlx-vlm": ("PaddleOCR-VL-1.6-0.9B",)})
    config = load_pipeline_config("PaddleOCR-VL-1.6")
    config.update({key: RECIPE[key] for key in ["batch_size", "use_queues", "layout_prep_cpu_workers", "use_chart_recognition", "use_seal_recognition", "use_ocr_for_image_block", "format_block_content", "merge_layout_blocks", "markdown_ignore_labels"]})
    config["SubModules"]["VLRecognition"] = {"module_name": "vl_recognition", "model_name": "PaddleOCR-VL-1.6-0.9B", "engine": "lore-mlx-vlm", "batch_size": 1}
    return PaddleOCRVL(
        pipeline_version="v1.6", paddlex_config=config, device="cpu", cpu_threads=8,
        layout_detection_model_dir=str(root / "layout"),
        doc_orientation_classify_model_dir=str(root / "orientation"),
        use_doc_orientation_classify=True, use_doc_unwarping=False,
        use_layout_detection=True, use_queues=False,
    )


def project_unit(pipeline, image, location):
    import numpy as np

    results = list(pipeline.predict_iter(np.ascontiguousarray(np.asarray(image)[:, :, ::-1])))
    if len(results) != 1 or "error" in results[0]:
        raise ProjectionError("invalid-projection")
    result = results[0].json["res"]
    width, height = result["width"], result["height"]
    if type(width) is not int or type(height) is not int or not (0 < width <= 2**32 - 1 and 0 < height <= 2**32 - 1):
        raise ProjectionError("invalid-projection")
    rotation = result.get("doc_preprocessor_res", {}).get("angle", 0)
    if type(rotation) is not int or rotation not in {0, 90, 180, 270}:
        raise ProjectionError("invalid-projection")
    blocks, lines = [], []
    for block in result["parsing_res_list"]:
        label, content, bbox, order = block["block_label"], block["block_content"], block["block_bbox"], block["block_order"]
        if not isinstance(label, str) or not isinstance(content, str) or len(bbox) != 4:
            raise ProjectionError("invalid-projection")
        x0, y0, x1, y1 = bbox
        if not all(type(value) in {int, float} and math.isfinite(value) for value in bbox) or not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height):
            raise ProjectionError("invalid-projection")
        if order is not None and (type(order) is not int or not 0 < order <= 2**32 - 1):
            raise ProjectionError("invalid-projection")
        bbox = [float(value) for value in bbox]
        normalized = {"label": label, "bbox": bbox, "order": order}
        blocks.append(normalized)
        lines.append(canonical({**normalized, "content": content}).decode())
    return {"location": location, "method": "visual", "representation": "json-lines", "text": "\n".join(lines), "visual_structure": {"width": width, "height": height, "rotation": rotation, "blocks": blocks}}


def main(options):
    root, runtime, witnesses, output = prepare(options)
    if options["operation"] != "project":
        output.emit({"status": "ready", "runtime": runtime})
        return
    path = pathlib.Path(options["input_path"])
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ProjectionError("invalid-projection")
    if path.stat().st_size > options["max_input_bytes"]:
        raise ProjectionError("input-limit")
    units = inventory(path, options["format"], options)
    output.emit({"status": "inventory", "runtime": runtime, "units": units})
    selected = read_message(options["max_output_bytes"])
    if selected is None:
        raise ProjectionError("invalid-projection")
    skip = selected.get("skip")
    if set(selected) != {"skip"} or not isinstance(skip, list) or any(type(number) is not int or not 1 <= number <= len(units) for number in skip) or len(skip) != len(set(skip)):
        raise ProjectionError("invalid-projection")
    skip = set(skip)
    pipeline = None
    remaining_decoded = options["max_decoded_bytes"]
    try:
        for item in units:
            number = item["location"]["number"]
            if number in skip:
                continue
            if item["width"] * item["height"] * 4 > options["max_raster_bytes"]:
                raise ProjectionError("decoded-limit")
            if pipeline is None:
                pipeline = build_pipeline(root, options, output)
            image = render(path, options["format"], number)
            try:
                expected = (item["width"], item["height"])
                valid_sizes = {expected} if options["format"] == "pdf" else {expected, expected[::-1]}
                if image.size not in valid_sizes:
                    raise ProjectionError("invalid-projection")
                unit = project_unit(pipeline, image, item["location"])
            finally:
                image.close()
            if model_versions(root, witnesses) != runtime["models"]:
                raise ProjectionError("vision-models-changed")
            remaining_decoded -= len(unit["text"].encode("utf-8"))
            if remaining_decoded < 0:
                raise ProjectionError("decoded-limit")
            output.emit({"status": "unit", "unit": unit})
    finally:
        if pipeline is not None:
            pipeline.close()
    output.emit({"status": "complete"})


if __name__ == "__main__":
    run(main)
