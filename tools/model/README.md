# Model tools

This directory is reserved for development-only model export, conversion, fixture generation, and quality evaluation tools.

- Python is managed by `uv` and pinned to the 3.12 series.
- The production Tauri application and native sidecars must not depend on this environment.
- Model weights and converted artifacts must not be committed.

Create the environment from the repository root:

```bash
uv sync --project tools/model --locked
```
