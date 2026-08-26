# Third-party notices

Zoos Upscale is licensed under the Apache License 2.0. Third-party components retain their own licenses and copyright notices.

The repository does not yet distribute AI model weights, inference binaries, or FFmpeg binaries. Before any release bundles them, this file and the release manifest must record each component's exact version, source, license, build options, and required notices.

## Source dependencies

Rust and JavaScript dependencies are pinned by their lockfiles. Rust dependencies are checked by the pinned `cargo-deny` gate. The deterministic production JavaScript inventory is tracked in `licenses/javascript-production.json` and verified in CI.

## Planned runtime components

The following components are candidates, not currently bundled artifacts:

- Tauri and its Rust/JavaScript dependencies
- Svelte and Vite build dependencies
- FFmpeg and ffprobe
- Real-ESRGAN ncnn Vulkan
- RIFE ncnn Vulkan
- ncnn
- ONNX Runtime
- Model weights selected through a separate provenance and license review

Adding a name here does not declare redistribution approval. Code licenses, model-weight licenses, codec configuration, and transitive dependencies must be reviewed independently.
