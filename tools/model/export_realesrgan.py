"""Deterministically export official Real-ESRGAN RRDBNet weights to ONNX."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ModelSpec:
    preset: str
    blocks: int
    source_name: str
    output_name: str


MODEL_SPECS = {
    "photo": ModelSpec("photo", 23, "RealESRGAN_x4plus.pth", "realesrgan-x4plus-fp32-opset17.onnx"),
    "anime": ModelSpec("anime", 6, "RealESRGAN_x4plus_anime_6B.pth", "realesrgan-x4plus-anime-6b-fp32-opset17.onnx"),
}
OPSET = 17
SEED = 20260826


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_rrdbnet(number_of_blocks: int):
    import torch
    from torch import nn
    from torch.nn import functional as functional

    class ResidualDenseBlock(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.conv1 = nn.Conv2d(64, 32, 3, 1, 1)
            self.conv2 = nn.Conv2d(96, 32, 3, 1, 1)
            self.conv3 = nn.Conv2d(128, 32, 3, 1, 1)
            self.conv4 = nn.Conv2d(160, 32, 3, 1, 1)
            self.conv5 = nn.Conv2d(192, 64, 3, 1, 1)
            self.lrelu = nn.LeakyReLU(negative_slope=0.2, inplace=True)

        def forward(self, value):
            value1 = self.lrelu(self.conv1(value))
            value2 = self.lrelu(self.conv2(torch.cat((value, value1), 1)))
            value3 = self.lrelu(self.conv3(torch.cat((value, value1, value2), 1)))
            value4 = self.lrelu(self.conv4(torch.cat((value, value1, value2, value3), 1)))
            value5 = self.conv5(torch.cat((value, value1, value2, value3, value4), 1))
            return value5 * 0.2 + value

    class RRDB(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.rdb1 = ResidualDenseBlock()
            self.rdb2 = ResidualDenseBlock()
            self.rdb3 = ResidualDenseBlock()

        def forward(self, value):
            output = self.rdb1(value)
            output = self.rdb2(output)
            output = self.rdb3(output)
            return output * 0.2 + value

    class RRDBNet(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.conv_first = nn.Conv2d(3, 64, 3, 1, 1)
            self.body = nn.Sequential(*(RRDB() for _ in range(number_of_blocks)))
            self.conv_body = nn.Conv2d(64, 64, 3, 1, 1)
            self.conv_up1 = nn.Conv2d(64, 64, 3, 1, 1)
            self.conv_up2 = nn.Conv2d(64, 64, 3, 1, 1)
            self.conv_hr = nn.Conv2d(64, 64, 3, 1, 1)
            self.conv_last = nn.Conv2d(64, 3, 3, 1, 1)
            self.lrelu = nn.LeakyReLU(negative_slope=0.2, inplace=True)

        def forward(self, value):
            feature = self.conv_first(value)
            body_feature = self.conv_body(self.body(feature))
            feature = feature + body_feature
            feature = self.lrelu(self.conv_up1(functional.interpolate(feature, scale_factor=2, mode="nearest")))
            feature = self.lrelu(self.conv_up2(functional.interpolate(feature, scale_factor=2, mode="nearest")))
            return self.conv_last(self.lrelu(self.conv_hr(feature)))

    return RRDBNet()


def load_official_weights(model, weights_path: Path) -> None:
    import torch

    checkpoint = torch.load(weights_path, map_location="cpu", weights_only=True)
    if not isinstance(checkpoint, dict):
        raise ValueError("Official checkpoint must be a dictionary")
    parameters = checkpoint.get("params_ema", checkpoint.get("params"))
    if not isinstance(parameters, dict):
        raise ValueError("Official checkpoint has no params_ema or params state dictionary")
    model.load_state_dict(parameters, strict=True)


def normalize_onnx(source: Path, destination: Path) -> None:
    import onnx

    model = onnx.load(source, load_external_data=False)
    model.producer_name = "zoos-upscale"
    model.producer_version = "goal1b-v1"
    model.domain = ""
    model.model_version = 1
    model.doc_string = ""
    del model.metadata_props[:]
    model.graph.doc_string = ""
    for node in model.graph.node:
        node.doc_string = ""
    destination.write_bytes(model.SerializeToString(deterministic=True))


def export_once(model, destination: Path) -> None:
    import torch

    sample = torch.linspace(0.0, 1.0, 3 * 8 * 8, dtype=torch.float32).reshape(1, 3, 8, 8)
    raw_path = destination.with_suffix(".raw.onnx")
    torch.onnx.export(
        model,
        sample,
        raw_path,
        export_params=True,
        opset_version=OPSET,
        do_constant_folding=True,
        input_names=["input"],
        output_names=["output"],
        dynamic_axes={"input": {2: "height", 3: "width"}, "output": {2: "output_height", 3: "output_width"}},
        dynamo=False,
    )
    normalize_onnx(raw_path, destination)
    raw_path.unlink()


def verify_export(model, onnx_path: Path) -> dict[str, float | int | str]:
    import numpy as np
    import onnx
    import onnxruntime
    import torch

    exported = onnx.load(onnx_path, load_external_data=False)
    onnx.checker.check_model(exported, full_check=True)
    if [(item.domain, item.version) for item in exported.opset_import] != [("", OPSET)]:
        raise ValueError("Exported model must use only ONNX opset 17")
    if [item.name for item in exported.graph.input] != ["input"] or [item.name for item in exported.graph.output] != ["output"]:
        raise ValueError("Exported model must expose input -> output")
    input_dimensions = exported.graph.input[0].type.tensor_type.shape.dim
    if [input_dimensions[index].dim_param for index in (2, 3)] != ["height", "width"]:
        raise ValueError("Input height and width must remain dynamic")

    sample = np.linspace(0.0, 1.0, 3 * 7 * 9, dtype=np.float32).reshape(1, 3, 7, 9)
    with torch.inference_mode():
        expected = model(torch.from_numpy(sample)).numpy()
    session = onnxruntime.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    actual = session.run(["output"], {"input": sample})[0]
    if actual.dtype != np.float32 or tuple(actual.shape) != (1, 3, 28, 36):
        raise ValueError(f"Unexpected ONNX output contract: {actual.dtype} {actual.shape}")
    difference = np.abs(expected - actual)
    maximum = float(difference.max())
    mean = float(difference.mean())
    if maximum > 0.0001 or mean > 0.00001:
        raise ValueError(f"ONNX verification mismatch: max={maximum} mean={mean}")
    return {"max_abs_error": maximum, "mean_abs_error": mean, "output_height": 28, "output_width": 36}


def export_model(spec: ModelSpec, weights_path: Path, output_path: Path) -> dict:
    import torch

    os.environ.setdefault("PYTHONHASHSEED", "0")
    torch.manual_seed(SEED)
    torch.use_deterministic_algorithms(True)
    model = build_rrdbnet(spec.blocks).to(dtype=torch.float32, device="cpu").eval()
    load_official_weights(model, weights_path)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="zoos-onnx-export-", dir=output_path.parent) as temporary:
        first = Path(temporary) / "first.onnx"
        second = Path(temporary) / "second.onnx"
        export_once(model, first)
        export_once(model, second)
        first_hash = sha256(first)
        second_hash = sha256(second)
        if first_hash != second_hash or first.read_bytes() != second.read_bytes():
            raise ValueError("Repeated ONNX exports are not byte-for-byte deterministic")
        verification = verify_export(model, first)
        os.replace(first, output_path)

    return {
        "preset": spec.preset,
        "architecture": {"name": "RRDBNet", "blocks": spec.blocks, "scale": 4},
        "precision": "FP32",
        "layout": "NCHW",
        "opset": OPSET,
        "input": "input",
        "output": "output",
        "dynamic_axes": ["height", "width"],
        "source": {"path": weights_path.name, "sha256": sha256(weights_path)},
        "onnx": {"path": output_path.name, "size": output_path.stat().st_size, "sha256": sha256(output_path)},
        "verification": verification,
    }


def validate_catalog(results: list[dict], catalog_path: Path) -> None:
    catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    if catalog.get("approved_for_distribution") is not False or catalog.get("bundled_in_release") is not False:
        raise ValueError("Generated ONNX catalog must remain development-only")
    expected = {item["preset"]: item for item in catalog.get("files", [])}
    if set(expected) != set(MODEL_SPECS):
        raise ValueError("Generated ONNX catalog must contain photo and anime")
    for result in results:
        item = expected[result["preset"]]
        actual_destination = f"models/{result['onnx']['path']}"
        if (
            item.get("source_sha256") != result["source"]["sha256"]
            or item.get("destination") != actual_destination
            or item.get("size") != result["onnx"]["size"]
            or item.get("sha256") != result["onnx"]["sha256"]
        ):
            raise ValueError(f"Generated {result['preset']} model does not match the pinned catalog")


def environment_evidence() -> dict[str, str]:
    hardware = platform.processor() or "unknown"
    if platform.system() == "Darwin":
        result = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and result.stdout.strip():
            hardware = result.stdout.strip()
    system = "macOS" if platform.system() == "Darwin" else platform.system()
    version = platform.mac_ver()[0] if system == "macOS" else platform.release()
    return {
        "architecture": platform.machine(),
        "hardware": hardware,
        "operating_system": f"{system} {version}",
        "platform": system,
    }


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--weights-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--catalog", type=Path, required=True)
    return parser.parse_args()


def main() -> None:
    arguments = parse_arguments()
    results = []
    for spec in MODEL_SPECS.values():
        weights = arguments.weights_dir / spec.source_name
        if not weights.is_file() or weights.is_symlink():
            raise ValueError(f"Missing regular source weight: {weights}")
        results.append(export_model(spec, weights, arguments.output_dir / spec.output_name))
    validate_catalog(results, arguments.catalog)
    evidence = {
        "schema_version": 1,
        "environment": environment_evidence(),
        "toolchain": {"python": "3.12", "torch": "2.13.0", "onnx": "1.22.0", "onnxruntime": "1.29.0", "numpy": "2.5.2"},
        "models": results,
    }
    arguments.evidence.parent.mkdir(parents=True, exist_ok=True)
    arguments.evidence.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(evidence, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
