"""Pinned model identity and bounded protocol shared by visual document workers."""

import base64
import contextlib
import hashlib
import importlib.metadata
import io
import json
import math
import os
import pathlib
import platform
import signal
import stat
import sys
import tempfile
import urllib.request

ENGINE = "paddle-ocr-vl"
MODELS = {
    "layout": {
        "files": {
            "inference.json": {
                "bytes": 1196890,
                "sha256": "2b68367c5b312a03de5a6e1642c597c8f95165a7e40cd59c6700cf4a5042f4fd"
            },
            "inference.pdiparams": {
                "bytes": 130806572,
                "sha256": "70bd316b0582769ec968829fd1feb1a6a58b7c941b938327e551b6b12b45c137"
            },
            "inference.yml": {
                "bytes": 1482,
                "sha256": "506fcfac13b3b546ae40d7886b44126420f392adb694e3f8bb6a6286a1f90fdc"
            }
        },
        "repo": "PaddlePaddle/PP-DocLayoutV3",
        "revision": "241f8bdfc77a7c7bee915a5057aaee58c235a8d3"
    },
    "orientation": {
        "files": {
            "inference.json": {
                "bytes": 104441,
                "sha256": "3580978602f309c3554508dd85d4fe09b73a7e0d80d7f9e63258f5b72c390c69"
            },
            "inference.pdiparams": {
                "bytes": 6754166,
                "sha256": "e8d6e7c5d264507e40e58a655779059d616b20d7441ea22047d829eb3931989c"
            },
            "inference.yml": {
                "bytes": 766,
                "sha256": "9e195eb729a8173588cd0e8a852c8b373aa606e79e77b4ac7d8346f5426caf26"
            }
        },
        "repo": "PaddlePaddle/PP-LCNet_x1_0_doc_ori",
        "revision": "d3b95a6dff5fe8a94f2748e12b61cb26818a0df8"
    },
    "vlm": {
        "files": {
            "LICENSE": {
                "bytes": 11376,
                "sha256": "b8c4d7deccd236af023af1c88c4d4e8f0fc2f41914e0fb23a3ec9678fb5a8456"
            },
            "added_tokens.json": {
                "bytes": 25381,
                "sha256": "f59f889088e0fe21c523e7cf121bb6dca3b0bb148cb7159fbb4572c74dfc5644"
            },
            "chat_template.jinja": {
                "bytes": 1474,
                "sha256": "2f27812dab7f333e471884e0c803d807f11953d5453140dfb1aaba234f872bc8"
            },
            "config.json": {
                "bytes": 2059,
                "sha256": "ce7f4565f8b1db78532ad5d1b9ebe55c2139d49bd4cb04778b580a08a598f171"
            },
            "generation_config.json": {
                "bytes": 133,
                "sha256": "a6701d78ab3b4d972307cdec3b69d4c13f46e0d5140514f50ab7d84259324b94"
            },
            "model.safetensors": {
                "bytes": 1917255968,
                "sha256": "85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db"
            },
            "preprocessor_config.json": {
                "bytes": 641,
                "sha256": "111872ab1e8bb7fd040ac5087bfced7ab8f011f02139b088cba294964c3b1d0e"
            },
            "processor_config.json": {
                "bytes": 137,
                "sha256": "1568858960a9760c54431dae693a6152e601ff55cdf6d2eab97a4a99958faea0"
            },
            "special_tokens_map.json": {
                "bytes": 1151,
                "sha256": "d3a125c03103deb2acaf7730791bdbbf196f620e5a2213b664511ff9b4b25bab"
            },
            "tokenizer.json": {
                "bytes": 11189060,
                "sha256": "c8a215a59183d0d0781adc33bacd3ce6162716f7fd568fb30234a74d69803a7d"
            },
            "tokenizer.model": {
                "bytes": 1614363,
                "sha256": "34ef7db83df785924fb83d7b887b6e822a031c56e15cff40aaf9b982988180df"
            },
            "tokenizer_config.json": {
                "bytes": 186947,
                "sha256": "1f979337347cc0cb72a6282d8a23ed183539aa81a87a906f022aee2bab83c7c5"
            }
        },
        "repo": "PaddlePaddle/PaddleOCR-VL-1.6",
        "revision": "c5630abae1d940eafe0697512a0325494b02ab42"
    }
}
RECIPE = {
    "policy": "paddle-ocr-vl-local-v2", "pipeline": "v1.6", "dpi": 150,
    "device": "cpu", "cpu_threads": 8, "batch_size": 1,
    "use_queues": False, "layout_prep_cpu_workers": 0,
    "use_doc_orientation_classify": True, "use_doc_unwarping": False,
    "use_layout_detection": True, "use_chart_recognition": False,
    "use_seal_recognition": False, "use_ocr_for_image_block": False,
    "format_block_content": False, "merge_layout_blocks": True,
    "markdown_ignore_labels": [], "max_tokens": 8192, "temperature": 0.0,
    "image_frame": "exif-transpose-alpha-white",
    "coordinates": "pixels-after-exif-and-document-orientation",
    "cross_page_merge": False, "raster_budget": "per-physical-unit",
    "processor": "mlx_vlm.models.paddleocr_vl.PaddleOCRVLProcessor",
}
PROTOCOL = sys.stdout


class ProjectionError(Exception):
    def __init__(self, code):
        self.code = code


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def digest_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def package_versions():
    if sys.platform != "darwin" or platform.machine() != "arm64":
        raise ProjectionError("unsupported-platform")
    if sys.version_info[:2] != (3, 12):
        raise ProjectionError("dependency-version")
    version = platform.mac_ver()[0].split(".")
    if not version or not version[0].isdigit() or int(version[0]) < 14:
        raise ProjectionError("unsupported-platform")
    try:
        if any(importlib.metadata.version(name) != version for name, version in PACKAGES.items()):
            raise ProjectionError("dependency-version")
    except importlib.metadata.PackageNotFoundError:
        raise ProjectionError("dependency") from None
    versions = {dist.metadata["Name"].lower(): dist.version for dist in importlib.metadata.distributions()}
    providers = {"opencv-python", "opencv-contrib-python", "opencv-python-headless", "opencv-contrib-python-headless"}
    if len({name.replace("_", "-") for name in versions} & providers) != 1:
        raise ProjectionError("dependency-version")
    versions["python"] = platform.python_version()
    return versions


def file_witness(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def model_versions(root, witnesses):
    versions = {}
    if not root.exists() or not stat.S_ISDIR(root.lstat().st_mode):
        raise ProjectionError("vision-models")
    for name, model in MODELS.items():
        directory = root / name
        if not directory.exists() or not stat.S_ISDIR(directory.lstat().st_mode):
            raise ProjectionError("vision-models")
        if {path.name for path in directory.iterdir()} != set(model["files"]):
            raise ProjectionError("vision-models-changed")
        artifacts = {}
        for filename, expected in model["files"].items():
            path = directory / filename
            try:
                info = path.lstat()
                if not stat.S_ISREG(info.st_mode):
                    raise ProjectionError("vision-models")
                if info.st_size != expected["bytes"]:
                    raise ProjectionError("vision-models-changed")
                key = (name, filename)
                if key in witnesses:
                    if file_witness(info) != witnesses[key]:
                        raise ProjectionError("vision-models-changed")
                else:
                    with path.open("rb") as stream:
                        opened = stream.fileno()
                        before = file_witness(os.fstat(opened))
                        if before != file_witness(info):
                            raise ProjectionError("vision-models-changed")
                        digest = hashlib.file_digest(stream, "sha256").hexdigest()
                        if digest != expected["sha256"] or before != file_witness(os.fstat(opened)) or before != file_witness(path.lstat()):
                            raise ProjectionError("vision-models-changed")
                    witnesses[key] = before
            except FileNotFoundError:
                raise ProjectionError("vision-models") from None
            artifacts[filename] = expected["sha256"]
        versions[name] = hashlib.sha256(canonical({"repo": model["repo"], "revision": model["revision"], "artifacts": artifacts})).hexdigest()
    versions["recipe"] = hashlib.sha256(canonical(RECIPE)).hexdigest()
    return versions


def install_models(root):
    for name, model in MODELS.items():
        directory = root / name
        directory.mkdir(parents=True, exist_ok=True)
        for filename, expected in model["files"].items():
            target = directory / filename
            if target.exists() or target.is_symlink():
                if target.is_symlink() or target.stat().st_size != expected["bytes"] or digest_file(target) != expected["sha256"]:
                    raise ProjectionError("vision-models-changed")
                continue
            temporary = None
            try:
                url = f'https://huggingface.co/{model["repo"]}/resolve/{model["revision"]}/{filename}?download=true'
                with tempfile.NamedTemporaryFile(dir=directory, prefix=".download-", delete=False) as out:
                    temporary = pathlib.Path(out.name)
                    digest = hashlib.sha256()
                    size = 0
                    with urllib.request.urlopen(url, timeout=30) as response:
                        while chunk := response.read(1024 * 1024):
                            size += len(chunk)
                            if size > expected["bytes"]:
                                raise ProjectionError("vision-models-changed")
                            out.write(chunk)
                            digest.update(chunk)
                    out.flush()
                    os.fsync(out.fileno())
                if size != expected["bytes"] or digest.hexdigest() != expected["sha256"]:
                    raise ProjectionError("vision-models-changed")
                os.link(temporary, target)
            except ProjectionError:
                raise
            except Exception:
                raise ProjectionError("model-download") from None
            finally:
                if temporary is not None:
                    temporary.unlink(missing_ok=True)


class Output:
    def __init__(self, limit):
        self.remaining = limit

    def emit(self, value, message_limit=None):
        data = canonical(value) + b"\n"
        if message_limit is not None:
            if len(data) > message_limit:
                raise ProjectionError("decoded-limit")
        else:
            if len(data) > self.remaining:
                raise ProjectionError("decoded-limit")
            self.remaining -= len(data)
        PROTOCOL.write(data.decode())
        PROTOCOL.flush()


def geometry(width, height):
    if not (0 < width <= 2**32 - 1 and 0 < height <= 2**32 - 1):
        raise ProjectionError("decoded-limit")
    return int(width), int(height)


def deadline_expired(*_):
    raise ProjectionError("timeout")


def prepare(options):
    for name in ["max_raster_bytes", "max_decoded_bytes", "max_units", "timeout_seconds", "max_output_bytes"]:
        if type(options.get(name)) is not int or options[name] <= 0:
            raise ProjectionError("invalid-projection")
    if type(options.get("max_input_bytes")) is not int or options["max_input_bytes"] < 0:
        raise ProjectionError("invalid-projection")
    if options.get("operation") not in {"install", "check", "project"}:
        raise ProjectionError("invalid-projection")
    signal.signal(signal.SIGALRM, deadline_expired)
    signal.alarm(options["timeout_seconds"])
    root = pathlib.Path(options["models_dir"])
    packages = package_versions()
    if options["operation"] == "install":
        install_models(root)
    os.environ.update({"HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1", "HF_HUB_DISABLE_TELEMETRY": "1", "PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK": "True", "TOKENIZERS_PARALLELISM": "false"})
    witnesses = {}
    runtime = {
        "engine": ENGINE,
        "packages": packages,
        "models": model_versions(root, witnesses),
        "model_revisions": {name: f'{model["repo"]}@{model["revision"]}' for name, model in MODELS.items()},
    }
    if options["operation"] == "project":
        sdk_cache = pathlib.Path(options["scratch_dir"])
        info = sdk_cache.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
            raise ProjectionError("invalid-projection")
        for variable in ["HF_HOME", "HF_HUB_CACHE", "PADDLE_HOME", "PADDLE_PDX_CACHE_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "TORCH_HOME", "MPLCONFIGDIR", "NUMBA_CACHE_DIR", "PADDLE_EXTENSION_DIR", "MODELSCOPE_CACHE"]:
            os.environ[variable] = str(sdk_cache / variable.lower())
    return root, runtime, witnesses, Output(options["max_output_bytes"])


def read_message(limit):
    data = sys.stdin.buffer.readline(limit + 1)
    if len(data) > limit:
        raise ProjectionError("decoded-limit")
    if not data:
        return None
    try:
        value = json.loads(data)
    except (ValueError, UnicodeDecodeError):
        raise ProjectionError("invalid-projection") from None
    if not isinstance(value, dict):
        raise ProjectionError("invalid-projection")
    return value


def run(main):
    try:
        with contextlib.redirect_stdout(sys.stderr):
            main(json.loads(sys.argv[1]))
    except ProjectionError as error:
        PROTOCOL.write(json.dumps({"status": "failed", "code": error.code}) + "\n")
        PROTOCOL.flush()
    except Exception:
        PROTOCOL.write('{"status":"failed","code":"decoder"}\n')
        PROTOCOL.flush()
