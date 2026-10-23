"""Bounded MLX recognition for requests routed by the owning Rust process."""

PACKAGES = {
    "mlx-vlm": "0.7.10", "mlx": "0.32.4", "transformers": "5.20.2",
    "pillow": "13.0.0", "opencv-python": "5.0.0.93",
}


def checked_text(chunks, remaining):
    text = []
    terminal = None
    try:
        for chunk in chunks:
            if terminal is not None or not isinstance(chunk.text, str):
                raise ProjectionError("incomplete-vision")
            size = len(chunk.text.encode("utf-8"))
            remaining[0] -= size
            if remaining[0] < 0:
                raise ProjectionError("decoded-limit")
            text.append(chunk.text)
            if chunk.finish_reason is not None:
                terminal = chunk.finish_reason
        if terminal != "stop":
            raise ProjectionError("incomplete-vision")
        return "".join(text)
    finally:
        chunks.close()


def load_recognizer(root):
    from mlx_vlm.models.paddleocr_vl import PaddleOCRVLProcessor
    from mlx_vlm.tokenizer_utils import load_tokenizer
    from mlx_vlm.utils import StoppingCriteria, load_model

    try:
        model = load_model(root / "vlm", lazy=False, strict=True, trust_remote_code=False)
        processor = PaddleOCRVLProcessor.from_pretrained(str(root / "vlm"), trust_remote_code=False)
        tokenizer = processor.tokenizer
        processor.detokenizer = load_tokenizer(root / "vlm", return_tokenizer=False)(tokenizer)
        tokenizer.stopping_criteria = StoppingCriteria(model.config.eos_token_id, tokenizer)
    except ProjectionError:
        raise
    except Exception:
        raise ProjectionError("vision-models") from None
    return model, processor, json.loads((root / "vlm/config.json").read_text())


def decode_image(encoded, options):
    from PIL import Image

    if not isinstance(encoded, str) or len(encoded) > options["max_raster_bytes"] * 2 + 1024 * 1024:
        raise ProjectionError("decoded-limit")
    try:
        data = base64.b64decode(encoded, validate=True)
    except (ValueError, TypeError):
        raise ProjectionError("invalid-projection") from None
    with io.BytesIO(data) as source, Image.open(source) as image:
        if image.format != "PNG" or getattr(image, "n_frames", 1) != 1:
            raise ProjectionError("invalid-projection")
        width, height = geometry(*image.size)
        if width * height * 4 > options["max_raster_bytes"]:
            raise ProjectionError("decoded-limit")
        return image.convert("RGB")


def main(options):
    root, runtime, witnesses, output = prepare(options)
    output.emit({"status": "ready", "runtime": runtime})
    if options["operation"] != "project":
        return
    recognizer = None
    remaining = [min(options["max_decoded_bytes"], options["max_output_bytes"])]
    while True:
        request = read_message(options["max_raster_bytes"] * 2 + 1024 * 1024)
        if request is None:
            break
        if set(request) != {"image", "query"} or not isinstance(request["query"], str):
            raise ProjectionError("invalid-projection")
        with decode_image(request["image"], options) as image:
            if recognizer is None:
                recognizer = load_recognizer(root)
            from mlx_vlm import apply_chat_template, stream_generate

            model, processor, config = recognizer
            prompt = apply_chat_template(processor, config, request["query"], num_images=1)
            chunks = stream_generate(model, processor, prompt, image=image, max_tokens=RECIPE["max_tokens"], temperature=RECIPE["temperature"])
            content = checked_text(chunks, remaining)
        if model_versions(root, witnesses) != runtime["models"]:
            raise ProjectionError("vision-models-changed")
        output.emit({"status": "recognized", "content": content, "finish_reason": "stop"})


if __name__ == "__main__":
    run(main)
