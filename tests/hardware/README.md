# Apple M5 Goal 1A hardware gate

This ignored integration gate exercises the verified Real-ESRGAN cache through the Rust wrapper
and the core image-safety plan, verification, and atomic publication path. It runs photo/anime,
x2/x4, and PNG/JPEG (eight cases) three times each. A separate test sends `SIGTERM` to the
wrapper process group and checks that the wrapper, upstream process, partial output, and final
output are gone.

The cache is never downloaded by the test. Fetch and verify it separately, then run from the
workspace root on the Apple M5 host:

```sh
ZOOS_M5_RUNTIME_ASSETS=/Users/mj/dev/zoos-upscale/.cache/runtime-assets/realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal \
  cargo test -p zoos-runner-realesrgan --test apple_m5_goal1a -- --ignored --nocapture --test-threads=1
```

`ZOOS_M5_RUNTIME_ASSETS` must be an absolute path containing `bin/realesrgan-ncnn-vulkan` and the
four allowlisted model files. The test invokes only absolute executable paths and does not use a
shell or resolve the wrapper/upstream through `PATH`.

The fixture and committed PNG goldens are decoded with the pinned Rust `image` crate before
pixel hashing and comparison. Passing requires
`max_abs_error <= 1`, `mean_abs_error <= 0.01`, and `PSNR >= 70 dB`; byte and decoded-pixel hashes
must also be identical across the three repeats of each case. Recorded hashes are Apple M5
regression evidence, not a compatibility verdict for other GPUs.
