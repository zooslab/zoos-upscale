import importlib.util
import sys
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
MODULE_PATH = ROOT / "tools" / "model" / "export_realesrgan.py"
SPEC = importlib.util.spec_from_file_location("export_realesrgan", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ExportContractTests(unittest.TestCase):
    def test_toolchain_is_exact_and_optional(self):
        project = tomllib.loads((ROOT / "tools" / "model" / "pyproject.toml").read_text())
        self.assertEqual(project["project"]["dependencies"], [])
        self.assertEqual(
            project["project"]["optional-dependencies"]["export"],
            ["numpy==2.5.2", "onnx==1.22.0", "onnxruntime==1.29.0", "torch==2.13.0"],
        )

    def test_model_specs_pin_official_rrdbnet_variants(self):
        self.assertEqual(MODULE.OPSET, 17)
        self.assertEqual(MODULE.MODEL_SPECS["photo"].blocks, 23)
        self.assertEqual(MODULE.MODEL_SPECS["anime"].blocks, 6)
        self.assertEqual(MODULE.MODEL_SPECS["photo"].source_name, "RealESRGAN_x4plus.pth")
        self.assertEqual(MODULE.MODEL_SPECS["anime"].source_name, "RealESRGAN_x4plus_anime_6B.pth")

    def test_generated_catalog_is_development_only(self):
        import json

        catalog = json.loads((ROOT / "assets" / "catalog" / "realesrgan-onnx-models.json").read_text())
        self.assertIs(catalog["approved_for_distribution"], False)
        self.assertIs(catalog["bundled_in_release"], False)
        self.assertEqual([item["preset"] for item in catalog["files"]], ["photo", "anime"])


if __name__ == "__main__":
    unittest.main()
