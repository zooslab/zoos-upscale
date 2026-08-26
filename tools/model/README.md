# Model tools

This directory is reserved for development-only model export, conversion, fixture generation, and quality evaluation tools.

- Python is managed by `uv` and pinned to the 3.12 series.
- The production Tauri application and native sidecars must not depend on this environment.
- Model weights and converted artifacts must not be committed.

Verify the lightweight default environment from the repository root:

```bash
uv sync --project tools/model --locked
```

The export stack is deliberately optional so routine CI does not install Torch or ONNX Runtime:

```bash
pnpm goal1b:fetch
uv sync --project tools/model --extra export --locked
pnpm goal1b:models
```

`goal1b:models` recreates the official 23-block photo and 6-block anime RRDBNet architectures, loads the pinned official `.pth` files strictly, and exports FP32 NCHW x4 ONNX models with dynamic height/width and opset 17. Each model is exported twice and must be byte-identical; ONNX checker and ONNX Runtime CPU output are compared against PyTorch before publication to `.cache/model-assets`.

Python, Torch, ONNX, ONNX Runtime, and NumPy are development tools only. The application build and runtime never invoke them, download assets, or bundle generated models.
