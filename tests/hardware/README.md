# Apple M5 Goal 1A hardware gate

This ignored integration gate exercises the verified Real-ESRGAN cache through the Rust wrapper
and the core image-safety plan, verification, and atomic publication path. It runs photo/anime,
x2/x4, and PNG/JPEG (eight cases) three times each. A separate test sends `SIGTERM` to the
wrapper process group and checks that the wrapper, upstream process, partial output, and final
output are gone.

The cache is never downloaded by the test. Fetch and verify it separately, then run from the
workspace root on the Apple M5 host:

```sh
ZOOS_M5_RUNTIME_ASSETS="$PWD/.cache/runtime-assets/realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal" \
  cargo test -p zoos-runner-realesrgan --test apple_m5_goal1a -- --ignored --nocapture --test-threads=1
```

The smaller orchestrator smoke gate runs photo PNG x2 through `JobOrchestrator`, the real process
backend, wrapper, image verification, and atomic publication path:

```sh
ZOOS_M5_RUNTIME_ASSETS="$PWD/.cache/runtime-assets/realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal" \
  cargo test -p zoos-runner-realesrgan --test apple_m5_orchestrator -- --ignored --nocapture --test-threads=1
```

`ZOOS_M5_RUNTIME_ASSETS` must be an absolute path containing `bin/realesrgan-ncnn-vulkan` and the
four allowlisted model files. The test invokes only absolute executable paths and does not use a
shell or resolve the wrapper/upstream through `PATH`.

The fixture and committed PNG goldens are decoded with the pinned Rust `image` crate before
pixel hashing and comparison. Passing requires
`max_abs_error <= 1`, `mean_abs_error <= 0.01`, and `PSNR >= 70 dB`; byte and decoded-pixel hashes
must also be identical across the three repeats of each case. Recorded hashes are Apple M5
regression evidence, not a compatibility verdict for other GPUs.

## Goal 1B CPU/GPU quality and image-pipeline gate

The Goal 1B gate uses `JobOrchestrator`, both registered native runners, and
`create_image_job_v2`. It checks photo/anime at x2/x4 on an identical RGB fixture, records
Apple M5-only byte hashes, and enforces the measured CPU-to-GPU pixel thresholds in
`apple-m5-goal1b-thresholds.json`. It also generates alpha, EXIF-orientation, and encoding
fixtures at runtime to cover PNG/WebP alpha preservation, orientation normalization, metadata
preserve/strip including ICC, JPEG quality 95, lossless WebP, same-stem PNG/JPEG batch filename
reservation and sequential completion, and real CPU-process cancellation cleanup. Every successful
job also verifies the recorded backend, Apple M5 device, runtime/model SHA-256, and final hash.

The ORT runner mirrors the pinned ncnn shader's 10-pixel `REFLECT_101` boundary padding. The
committed thresholds require every photo/anime x2/x4 comparison to remain above 50 dB PSNR;
they are regression limits for this fixture rather than a promise of byte-identical backends.

The engine, ONNX Runtime, and ONNX model files stay in the verified local cache and are never
committed. Build both wrappers and run the ignored gate from the workspace root:

```sh
cargo build -p zoos-runner-ort -p zoos-runner-realesrgan --bins --locked
ZOOS_M5_RUNTIME_ASSETS="$PWD/.cache/runtime-assets/realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal" \
ZOOS_M5_ORT_RUNTIME="$PWD/.cache/runtime-assets/onnxruntime-macos-arm64/1.29.0/lib/libonnxruntime.1.29.0.dylib" \
ZOOS_M5_ONNX_MODELS="$PWD/.cache/model-assets/realesrgan-onnx/goal1b-v1/models" \
  cargo test -p zoos-core --test apple_m5_goal1b --locked -- \
  --ignored --nocapture --test-threads=1
```

The CPU/GPU comparison detects backend drift on this exact Apple M5 fixture; it is not a claim
that different GPU vendors or drivers will produce the same pixels.

## Goal 2 FFmpeg/RIFE video gate

The Goal 2 ignored gate builds CFR MP4, MOV, and MKV fixtures from Rust-generated 64×64 PNG
frames, PCM WAV tracks, and SRT text. It validates 25→50, 30→60, and
30000/1001→60000/1001 interpolation, video-only and multi-stream mux plans, a 60-second bounded
workspace run, scene-cut protection, exact frame counts, A/V sync, source immutability, atomic
publication, cancellation cleanup, and GPU/CPU regression metrics. Bitmap subtitle rejection stays
in the ordinary `zoos-media` fixture tests.

The test never downloads assets and invokes only absolute executable paths. Prepare the verified
development caches and wrapper, then run serially on the Apple M5 host:

```sh
pnpm goal2:fetch
cargo build -p zoos-runner-rife --bin zoos-runner-rife-bin --locked
ZOOS_M5_FFMPEG_ASSETS="$PWD/.cache/runtime-assets/ffmpeg-macos-arm64/9.0.1" \
ZOOS_M5_RIFE_ASSETS="$PWD/.cache/runtime-assets/rife-ncnn-vulkan-macos/20221029/macos-universal" \
ZOOS_M5_RIFE_WRAPPER="$PWD/target/debug/zoos-runner-rife-bin" \
  cargo test -p zoos-core --test apple_m5_goal2 --locked -- \
  --ignored --nocapture --test-threads=1
```

Every run verifies its output SHA-256, but VideoToolbox byte hashes are not pinned because repeated
encodes are not byte-deterministic. The committed CPU/GPU thresholds are Apple M5 regression
evidence only and do not establish support for other GPUs or operating systems.
The decoded-frame guard is `max_abs_error <= 14`, `mean_abs_error <= 0.30`, and
`PSNR >= 51.5 dB`; scene-cut midpoint replacement permits a post-H.264 maximum error of 2.
