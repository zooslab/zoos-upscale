#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use image::{DynamicImage, GenericImageView, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zoos_core::{
    ImageBackend, ImageBatchMetadata, ImageOutputFormat, ImagePipelineVerification, ImagePreset,
    ImageSettings, JPEG_OUTPUT_QUALITY, JobKind, JobOrchestrator, JobStatus, JobSummary,
    MetadataPolicy, RunnerLaunchSpec, RunnerRegistry,
};

const GPU_ASSETS_ENV: &str = "ZOOS_M5_RUNTIME_ASSETS";
const ORT_RUNTIME_ENV: &str = "ZOOS_M5_ORT_RUNTIME";
const ONNX_MODELS_ENV: &str = "ZOOS_M5_ONNX_MODELS";
const GPU_RUNTIME_SHA256: &str = "c1c35d92079085de96b9d547fd7e4464bc8a2e9ccf28d7b8c712d72ade91b7cc";
const PHOTO_PARAM_SHA256: &str = "35330ececcea33b6c397a72548e788d5d53becee4734c50b7fada36e89f10a86";
const PHOTO_BIN_SHA256: &str = "713ee713b0353afaa27976f0563a64a5043bd70b9bd8936c2e26e25ebcdbcddf";
const ANIME_PARAM_SHA256: &str = "2b8fb6e0ae4d2d85704ca08c119a2f5ea40add4f2ecd512eb7f4cd44b6127ed4";
const ANIME_BIN_SHA256: &str = "fe01c269cfd10cdef8e018ab66ebe750cf79c7af4d1f9c16c737e1295229bacc";
const ORT_RUNTIME_SHA256: &str = "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb";
const PHOTO_ONNX_SHA256: &str = "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b";
const ANIME_ONNX_SHA256: &str = "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1";

#[derive(Debug, Deserialize)]
struct ThresholdManifest {
    schema_version: u32,
    gate_id: String,
    fixture: String,
    cases: BTreeMap<String, QualityThreshold>,
}

#[derive(Debug, Deserialize)]
struct QualityThreshold {
    measured_max_abs_error: u8,
    measured_mean_abs_error: f64,
    measured_psnr_db: f64,
    max_abs_error: u8,
    mean_abs_error: f64,
    min_psnr_db: f64,
}

#[derive(Debug, Deserialize)]
struct EvidenceManifest {
    schema_version: u32,
    gate_id: String,
    platform: String,
    cpu: String,
    gpu: String,
    regression_only: bool,
    outputs: BTreeMap<String, OutputEvidence>,
}

#[derive(Debug, Deserialize)]
struct OutputEvidence {
    cpu_sha256: String,
    gpu_sha256: String,
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
struct SuccessManifest {
    result: Option<String>,
    exit_code: Option<i32>,
    actual_backend: Option<ImageBackend>,
    actual_device: Option<String>,
    runtime_sha256: Option<String>,
    model_param_sha256: Option<String>,
    model_bin_sha256: Option<String>,
    model_onnx_sha256: Option<String>,
    final_sha256: Option<String>,
    icc_preserved: Option<bool>,
    exif_preserved: Option<bool>,
    alpha_preserved: Option<bool>,
}

#[derive(Debug)]
struct CompletedJob {
    verification: ImagePipelineVerification,
    pixels: DynamicImage,
}

#[derive(Debug)]
struct Metrics {
    max_abs_error: u8,
    mean_abs_error: f64,
    psnr_db: f64,
}

#[tokio::test]
#[ignore = "requires verified Goal 1B local assets and Apple M5 GPU"]
async fn apple_m5_goal1b_cpu_gpu_quality_and_image_pipeline_gate() {
    let repository = workspace_root();
    let thresholds: ThresholdManifest =
        read_json(&repository.join("tests/hardware/apple-m5-goal1b-thresholds.json"));
    let evidence: EvidenceManifest =
        read_json(&repository.join("tests/hardware/apple-m5-goal1b-evidence.json"));
    assert_eq!(thresholds.schema_version, 1);
    assert_eq!(thresholds.gate_id, "apple-m5-goal1b");
    assert_eq!(thresholds.fixture, "rgb8-pattern-64x48.png");
    assert_eq!(evidence.schema_version, 1);
    assert_eq!(evidence.gate_id, thresholds.gate_id);
    assert_eq!(evidence.platform, "macOS arm64");
    assert_eq!(evidence.cpu, "Apple M5 / ONNX Runtime 1.29.0 CPU");
    assert_eq!(evidence.gpu, "gpu:0 Apple M5 / Vulkan");
    assert!(evidence.regression_only);

    let run_root = tempfile::Builder::new()
        .prefix("zoos-apple-m5-goal1b-")
        .tempdir()
        .expect("hardware gate temporary directory must be created");
    let input_root = run_root.path().join("inputs");
    fs::create_dir(&input_root).expect("input directory must be created");
    let workspace = run_root.path().join("workspace");
    let orchestrator = orchestrator(&workspace);

    cancellation_gate(&orchestrator, &workspace, &input_root).await;

    let fixture = repository.join("tests/hardware/fixtures/rgb8-pattern-64x48.png");
    let fixture_hash = sha256_file(&fixture);
    let mut matrix_outputs = BTreeMap::new();
    for preset in [ImagePreset::Photo, ImagePreset::Anime] {
        for scale in [2_u8, 4] {
            let case_id = format!("{}-x{scale}", preset_name(preset));
            let cpu_input = input_root.join(format!("{case_id}-cpu.png"));
            let gpu_input = input_root.join(format!("{case_id}-gpu.png"));
            fs::copy(&fixture, &cpu_input).expect("CPU fixture must copy");
            fs::copy(&fixture, &gpu_input).expect("GPU fixture must copy");
            let cpu = run_job(
                &orchestrator,
                &workspace,
                &cpu_input,
                settings(preset, scale, ImageOutputFormat::Png, MetadataPolicy::Strip),
                ImageBackend::OrtCpu,
            )
            .await;
            let gpu = run_job(
                &orchestrator,
                &workspace,
                &gpu_input,
                settings(preset, scale, ImageOutputFormat::Png, MetadataPolicy::Strip),
                ImageBackend::VulkanGpu,
            )
            .await;
            assert_eq!(sha256_file(&cpu_input), fixture_hash);
            assert_eq!(sha256_file(&gpu_input), fixture_hash);
            assert_eq!(cpu.verification.actual_backend, ImageBackend::OrtCpu);
            assert_eq!(gpu.verification.actual_backend, ImageBackend::VulkanGpu);
            assert_eq!(cpu.pixels.color(), image::ColorType::Rgb8);
            assert_eq!(gpu.pixels.color(), image::ColorType::Rgb8);
            let cpu_pixels = cpu.pixels.clone().into_rgb8();
            let gpu_pixels = gpu.pixels.into_rgb8();
            assert_eq!(cpu_pixels.dimensions(), gpu_pixels.dimensions());
            assert_eq!(
                cpu_pixels.dimensions(),
                (64 * u32::from(scale), 48 * u32::from(scale))
            );
            let metrics = compare_pixels(&cpu_pixels, &gpu_pixels);
            let limit = thresholds
                .cases
                .get(&case_id)
                .unwrap_or_else(|| panic!("missing threshold for {case_id}"));
            assert_eq!(metrics.max_abs_error, limit.measured_max_abs_error);
            assert!((metrics.mean_abs_error - limit.measured_mean_abs_error).abs() < 0.000_000_01);
            assert!((metrics.psnr_db - limit.measured_psnr_db).abs() < 0.000_1);
            assert!(
                metrics.max_abs_error <= limit.max_abs_error,
                "{case_id}: {metrics:?}"
            );
            assert!(
                metrics.mean_abs_error <= limit.mean_abs_error,
                "{case_id}: {metrics:?}"
            );
            assert!(
                metrics.psnr_db >= limit.min_psnr_db,
                "{case_id}: {metrics:?}"
            );
            let expected = evidence
                .outputs
                .get(&case_id)
                .unwrap_or_else(|| panic!("missing evidence for {case_id}"));
            eprintln!(
                "GOAL1B_M5 case={case_id} max_abs={} mean_abs={:.8} psnr_db={:.4} cpu_sha={} gpu_sha={}",
                metrics.max_abs_error,
                metrics.mean_abs_error,
                metrics.psnr_db,
                cpu.verification.output_sha256,
                gpu.verification.output_sha256
            );
            assert_eq!(cpu.verification.output_sha256, expected.cpu_sha256);
            assert_eq!(gpu.verification.output_sha256, expected.gpu_sha256);
            assert_eq!(cpu.verification.output_width, expected.width);
            assert_eq!(cpu.verification.output_height, expected.height);
            matrix_outputs.insert(case_id, cpu);
        }
    }
    assert_eq!(matrix_outputs.len(), 4);

    alpha_gate(&orchestrator, &workspace, &input_root).await;
    orientation_and_metadata_gate(&orchestrator, &workspace, &input_root).await;
    encoding_gate(
        &orchestrator,
        &workspace,
        &input_root,
        &fixture,
        matrix_outputs,
    )
    .await;
    batch_reservation_gate(&orchestrator, &workspace, &input_root).await;
}

async fn cancellation_gate(orchestrator: &JobOrchestrator, workspace: &Path, input_root: &Path) {
    let input = input_root.join("cancel-cpu.png");
    write_rgb_fixture(&input, 64, 48);
    let before = sha256_file(&input);
    let created = orchestrator
        .create_image_job_v2(
            &input,
            settings(
                ImagePreset::Photo,
                4,
                ImageOutputFormat::Png,
                MetadataPolicy::Strip,
            ),
            ImageBackend::OrtCpu,
            None,
        )
        .expect("CPU cancellation job must create");
    let final_path = created.output_path.clone().expect("output must be planned");
    orchestrator
        .start_job(&created.job_id)
        .await
        .expect("CPU cancellation job must start");
    wait_for_status(orchestrator, &created.job_id, JobStatus::Running).await;
    orchestrator
        .cancel_job(&created.job_id)
        .await
        .expect("running CPU job must accept cancellation");
    let terminal = wait_for_terminal(orchestrator, &created.job_id).await;
    assert_eq!(terminal.status, JobStatus::Cancelled, "{terminal:?}");
    assert_eq!(sha256_file(&input), before);
    assert!(!final_path.exists());
    assert_no_owned_artifacts(workspace, &created.job_id, &final_path);
    let job_path = workspace.join(&created.job_id).join("runner-job.json");
    let pgrep = Command::new("pgrep")
        .args(["-f", &job_path.to_string_lossy()])
        .output()
        .expect("pgrep must execute");
    assert!(
        !pgrep.status.success(),
        "cancelled runner process remains: {}",
        String::from_utf8_lossy(&pgrep.stdout)
    );
}

async fn alpha_gate(orchestrator: &JobOrchestrator, workspace: &Path, input_root: &Path) {
    let input = input_root.join("alpha-gradient.png");
    let mut image = RgbaImage::new(8, 6);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgba([
            (x * 27) as u8,
            (y * 39) as u8,
            ((x + y) * 17) as u8,
            ((x * 19 + y * 23) % 256) as u8,
        ]);
    }
    image
        .save_with_format(&input, ImageFormat::Png)
        .expect("alpha fixture must save");
    let before = sha256_file(&input);
    let png = run_job(
        orchestrator,
        workspace,
        &input,
        settings(
            ImagePreset::Anime,
            2,
            ImageOutputFormat::Png,
            MetadataPolicy::Strip,
        ),
        ImageBackend::OrtCpu,
    )
    .await;
    let webp_input = input_root.join("alpha-gradient-webp.png");
    fs::copy(&input, &webp_input).expect("alpha fixture must copy");
    let webp = run_job(
        orchestrator,
        workspace,
        &webp_input,
        settings(
            ImagePreset::Anime,
            2,
            ImageOutputFormat::Webp,
            MetadataPolicy::Strip,
        ),
        ImageBackend::OrtCpu,
    )
    .await;
    assert_eq!(sha256_file(&input), before);
    assert!(png.verification.alpha_preserved);
    assert!(webp.verification.alpha_preserved);
    let png = png.pixels.into_rgba8();
    let webp = webp.pixels.into_rgba8();
    assert_eq!(png.dimensions(), (16, 12));
    assert_eq!(
        png.as_raw(),
        webp.as_raw(),
        "lossless WebP must retain RGBA pixels"
    );
    let alpha = png.pixels().map(|pixel| pixel[3]).collect::<Vec<_>>();
    assert!(alpha.contains(&0));
    assert!(alpha.iter().any(|value| *value > 0 && *value < 255));
}

async fn orientation_and_metadata_gate(
    orchestrator: &JobOrchestrator,
    workspace: &Path,
    input_root: &Path,
) {
    let source = input_root.join("orientation-6.jpg");
    write_oriented_jpeg(&source);
    let source_hash = sha256_file(&source);
    let preserve = orchestrator
        .create_image_job_v2(
            &source,
            settings(
                ImagePreset::Photo,
                2,
                ImageOutputFormat::Jpeg,
                MetadataPolicy::Preserve,
            ),
            ImageBackend::OrtCpu,
            None,
        )
        .expect("oriented JPEG job must create");
    let prepared = image::open(
        workspace
            .join(&preserve.job_id)
            .join("work/inference-rgb.png"),
    )
    .expect("normalized inference image must exist")
    .into_rgb8();
    assert_eq!(
        prepared.dimensions(),
        (2, 3),
        "orientation 6 must rotate to upright dimensions"
    );
    orchestrator
        .start_job(&preserve.job_id)
        .await
        .expect("preserve job must start");
    let preserve = completed_job(orchestrator, workspace, preserve).await;
    assert_eq!(sha256_file(&source), source_hash);
    assert_eq!(preserve.pixels.dimensions(), (4, 6));
    assert!(preserve.verification.exif_preserved);
    assert!(preserve.verification.icc_preserved);
    assert!(
        fs::read(&preserve.verification.output_path)
            .unwrap()
            .windows(12)
            .any(|window| window == b"ICC_PROFILE\0")
    );
    assert_eq!(
        exif_orientation(&fs::read(&preserve.verification.output_path).unwrap()),
        Some(1)
    );

    let strip_source = input_root.join("orientation-6-strip.jpg");
    fs::copy(&source, &strip_source).expect("oriented JPEG must copy");
    let strip = run_job(
        orchestrator,
        workspace,
        &strip_source,
        settings(
            ImagePreset::Photo,
            2,
            ImageOutputFormat::Jpeg,
            MetadataPolicy::Strip,
        ),
        ImageBackend::OrtCpu,
    )
    .await;
    assert!(!strip.verification.exif_preserved);
    assert!(!strip.verification.icc_preserved);
    assert!(
        !fs::read(&strip.verification.output_path)
            .unwrap()
            .windows(12)
            .any(|window| window == b"ICC_PROFILE\0")
    );
    assert_eq!(
        exif_orientation(&fs::read(&strip.verification.output_path).unwrap()),
        None
    );
}

async fn encoding_gate(
    orchestrator: &JobOrchestrator,
    workspace: &Path,
    input_root: &Path,
    fixture: &Path,
    mut matrix: BTreeMap<String, CompletedJob>,
) {
    assert_eq!(JPEG_OUTPUT_QUALITY, 95);
    let jpeg_input = input_root.join("rgb-jpeg95.png");
    fs::copy(fixture, &jpeg_input).expect("JPEG fixture must copy");
    let jpeg = run_job(
        orchestrator,
        workspace,
        &jpeg_input,
        settings(
            ImagePreset::Photo,
            2,
            ImageOutputFormat::Jpeg,
            MetadataPolicy::Strip,
        ),
        ImageBackend::OrtCpu,
    )
    .await;
    assert_eq!(
        image::guess_format(&fs::read(&jpeg.verification.output_path).unwrap()).unwrap(),
        ImageFormat::Jpeg
    );
    assert_eq!(jpeg.pixels.dimensions(), (128, 96));

    let webp_input = input_root.join("rgb-lossless-webp.png");
    fs::copy(fixture, &webp_input).expect("WebP fixture must copy");
    let webp = run_job(
        orchestrator,
        workspace,
        &webp_input,
        settings(
            ImagePreset::Photo,
            2,
            ImageOutputFormat::Webp,
            MetadataPolicy::Strip,
        ),
        ImageBackend::OrtCpu,
    )
    .await;
    let png = matrix
        .remove("photo-x2")
        .expect("photo x2 CPU result")
        .pixels
        .into_rgb8();
    assert_eq!(
        webp.pixels.into_rgb8().as_raw(),
        png.as_raw(),
        "lossless WebP must equal PNG pixels"
    );
}

async fn batch_reservation_gate(
    orchestrator: &JobOrchestrator,
    workspace: &Path,
    input_root: &Path,
) {
    let png = input_root.join("batch-pair.png");
    let jpeg = input_root.join("batch-pair.jpg");
    write_rgb_fixture(&png, 8, 6);
    image::open(&png)
        .expect("batch PNG must decode")
        .into_rgb8()
        .save_with_format(&jpeg, ImageFormat::Jpeg)
        .expect("same-stem JPEG must save");
    let png_hash = sha256_file(&png);
    let jpeg_hash = sha256_file(&jpeg);
    let image_settings = settings(
        ImagePreset::Photo,
        2,
        ImageOutputFormat::Png,
        MetadataPolicy::Strip,
    );
    let first = orchestrator
        .create_image_job_v2(
            &png,
            image_settings,
            ImageBackend::OrtCpu,
            Some(ImageBatchMetadata {
                batch_id: "m5-same-stem".into(),
                index: 1,
                total: 2,
            }),
        )
        .expect("first batch item must reserve output");
    let second = orchestrator
        .create_image_job_v2(
            &jpeg,
            image_settings,
            ImageBackend::OrtCpu,
            Some(ImageBatchMetadata {
                batch_id: "m5-same-stem".into(),
                index: 2,
                total: 2,
            }),
        )
        .expect("second same-stem item must reserve a distinct output");
    assert_eq!(first.batch_id.as_deref(), Some("m5-same-stem"));
    assert_eq!((first.batch_index, first.batch_total), (Some(1), Some(2)));
    assert_eq!((second.batch_index, second.batch_total), (Some(2), Some(2)));
    assert_ne!(first.output_path, second.output_path);
    assert!(
        second
            .output_path
            .as_ref()
            .unwrap()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .ends_with("_2")
    );

    orchestrator
        .start_job(&first.job_id)
        .await
        .expect("first batch item must start");
    let first = completed_job(orchestrator, workspace, first).await;
    orchestrator
        .start_job(&second.job_id)
        .await
        .expect("second batch item must start after the first");
    let second = completed_job(orchestrator, workspace, second).await;
    assert_eq!(sha256_file(&png), png_hash);
    assert_eq!(sha256_file(&jpeg), jpeg_hash);
    assert_eq!(first.pixels.dimensions(), (16, 12));
    assert_eq!(second.pixels.dimensions(), (16, 12));
}

fn orchestrator(workspace: &Path) -> JobOrchestrator {
    let target = workspace_root().join("target/debug");
    let gpu_assets = env_path(GPU_ASSETS_ENV);
    let gpu = RunnerLaunchSpec::new(
        "zoos-runner-realesrgan",
        target.join("zoos-runner-realesrgan-bin"),
    )
    .expect("GPU wrapper path must be absolute")
    .with_arguments([
        OsString::from("--engine"),
        gpu_assets
            .join("bin/realesrgan-ncnn-vulkan")
            .into_os_string(),
        OsString::from("--models"),
        gpu_assets.join("models").into_os_string(),
    ])
    .expect("GPU arguments must be absolute");
    let cpu = RunnerLaunchSpec::new("zoos-runner-ort", target.join("zoos-runner-ort-bin"))
        .expect("CPU wrapper path must be absolute")
        .with_arguments([
            OsString::from("--runtime"),
            env_path(ORT_RUNTIME_ENV).into_os_string(),
            OsString::from("--models"),
            env_path(ONNX_MODELS_ENV).into_os_string(),
        ])
        .expect("CPU arguments must be absolute");
    let mut registry = RunnerRegistry::with_runner_id(gpu);
    registry.register_runner(cpu);
    JobOrchestrator::with_runner_registry(
        workspace,
        registry,
        Duration::from_secs(20),
        Duration::from_secs(3),
    )
    .expect("Goal 1B orchestrator must start")
}

async fn run_job(
    orchestrator: &JobOrchestrator,
    workspace: &Path,
    input: &Path,
    settings: ImageSettings,
    backend: ImageBackend,
) -> CompletedJob {
    let before = sha256_file(input);
    let created = orchestrator
        .create_image_job_v2(input, settings, backend, None)
        .expect("Goal 1B image job must create");
    assert_eq!(created.kind, JobKind::ImageUpscale);
    assert_eq!(created.selected_backend, Some(backend));
    orchestrator
        .start_job(&created.job_id)
        .await
        .expect("image job must start");
    let completed = completed_job(orchestrator, workspace, created).await;
    assert_eq!(
        sha256_file(input),
        before,
        "source image must remain unchanged"
    );
    completed
}

async fn completed_job(
    orchestrator: &JobOrchestrator,
    workspace: &Path,
    created: JobSummary,
) -> CompletedJob {
    let terminal = wait_for_terminal(orchestrator, &created.job_id).await;
    assert_eq!(terminal.status, JobStatus::Completed, "{terminal:?}");
    let output = terminal
        .output_path
        .as_ref()
        .expect("completed output path")
        .clone();
    let verification: ImagePipelineVerification =
        read_json(&workspace.join(&created.job_id).join("verification.json"));
    assert_eq!(verification.job_id, created.job_id);
    assert_eq!(
        verification.source_sha256_before,
        verification.source_sha256_after
    );
    assert_eq!(verification.output_sha256, sha256_file(&output));
    assert_eq!(verification.output_path, output);
    assert_no_owned_artifacts(workspace, &created.job_id, &output);
    let manifest: SuccessManifest =
        read_json(&workspace.join(&created.job_id).join("manifest.json"));
    assert_success_manifest(&manifest, &created, &verification);
    CompletedJob {
        verification,
        pixels: image::open(output).expect("published output must decode"),
    }
}

fn assert_success_manifest(
    manifest: &SuccessManifest,
    created: &JobSummary,
    verification: &ImagePipelineVerification,
) {
    let backend = created
        .selected_backend
        .expect("selected backend must persist");
    let preset = created
        .image_settings
        .expect("image settings must persist")
        .preset;
    assert_eq!(manifest.result.as_deref(), Some("completed"));
    assert_eq!(manifest.exit_code, Some(0));
    assert_eq!(manifest.actual_backend, Some(backend));
    assert_eq!(
        manifest.final_sha256.as_deref(),
        Some(verification.output_sha256.as_str())
    );
    assert_eq!(manifest.icc_preserved, Some(verification.icc_preserved));
    assert_eq!(manifest.exif_preserved, Some(verification.exif_preserved));
    assert_eq!(manifest.alpha_preserved, Some(verification.alpha_preserved));
    match (backend, preset) {
        (ImageBackend::OrtCpu, ImagePreset::Photo) => {
            assert_eq!(manifest.actual_device.as_deref(), Some("cpu:0 Apple M5"));
            assert_eq!(manifest.runtime_sha256.as_deref(), Some(ORT_RUNTIME_SHA256));
            assert_eq!(
                manifest.model_onnx_sha256.as_deref(),
                Some(PHOTO_ONNX_SHA256)
            );
            assert!(manifest.model_param_sha256.is_none());
            assert!(manifest.model_bin_sha256.is_none());
        }
        (ImageBackend::OrtCpu, ImagePreset::Anime) => {
            assert_eq!(manifest.actual_device.as_deref(), Some("cpu:0 Apple M5"));
            assert_eq!(manifest.runtime_sha256.as_deref(), Some(ORT_RUNTIME_SHA256));
            assert_eq!(
                manifest.model_onnx_sha256.as_deref(),
                Some(ANIME_ONNX_SHA256)
            );
            assert!(manifest.model_param_sha256.is_none());
            assert!(manifest.model_bin_sha256.is_none());
        }
        (ImageBackend::VulkanGpu, ImagePreset::Photo) => {
            assert_eq!(manifest.actual_device.as_deref(), Some("gpu:0 Apple M5"));
            assert_eq!(manifest.runtime_sha256.as_deref(), Some(GPU_RUNTIME_SHA256));
            assert_eq!(
                manifest.model_param_sha256.as_deref(),
                Some(PHOTO_PARAM_SHA256)
            );
            assert_eq!(manifest.model_bin_sha256.as_deref(), Some(PHOTO_BIN_SHA256));
            assert!(manifest.model_onnx_sha256.is_none());
        }
        (ImageBackend::VulkanGpu, ImagePreset::Anime) => {
            assert_eq!(manifest.actual_device.as_deref(), Some("gpu:0 Apple M5"));
            assert_eq!(manifest.runtime_sha256.as_deref(), Some(GPU_RUNTIME_SHA256));
            assert_eq!(
                manifest.model_param_sha256.as_deref(),
                Some(ANIME_PARAM_SHA256)
            );
            assert_eq!(manifest.model_bin_sha256.as_deref(), Some(ANIME_BIN_SHA256));
            assert!(manifest.model_onnx_sha256.is_none());
        }
        (ImageBackend::Auto, _) => panic!("auto is not an executable backend"),
    }
}

async fn wait_for_status(orchestrator: &JobOrchestrator, job_id: &str, expected: JobStatus) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = current_job(orchestrator, job_id);
            assert!(
                !current.status.is_terminal(),
                "job became terminal before {expected:?}: {current:?}"
            );
            if current.status == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job must reach expected state");
}

async fn wait_for_terminal(orchestrator: &JobOrchestrator, job_id: &str) -> JobSummary {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let current = current_job(orchestrator, job_id);
            if current.status.is_terminal() {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real image job must reach terminal state")
}

fn current_job(orchestrator: &JobOrchestrator, job_id: &str) -> JobSummary {
    orchestrator
        .list_jobs()
        .expect("jobs must remain readable")
        .into_iter()
        .find(|job| job.job_id == job_id)
        .expect("created job must remain listed")
}

fn assert_no_owned_artifacts(workspace: &Path, job_id: &str, final_path: &Path) {
    let job = workspace.join(job_id);
    let work = job.join("work");
    if work.exists() {
        assert_eq!(
            fs::read_dir(&work)
                .expect("work directory must list")
                .count(),
            0,
            "intermediate files remain in {}",
            work.display()
        );
    }
    let output_parent = final_path.parent().expect("output parent");
    if output_parent.exists() {
        assert!(
            !fs::read_dir(output_parent).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(&format!("zoos-{job_id}"))),
            "partial output remains"
        );
    }
}

fn settings(
    preset: ImagePreset,
    scale: u8,
    output_format: ImageOutputFormat,
    metadata: MetadataPolicy,
) -> ImageSettings {
    ImageSettings {
        preset,
        scale,
        backend: ImageBackend::Auto,
        output_format,
        metadata,
    }
}

fn write_rgb_fixture(path: &Path, width: u32, height: u32) {
    let mut image = RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = Rgb([
            ((x * 11 + y * 3) % 256) as u8,
            ((x * 5 + y * 13) % 256) as u8,
            ((x * 17 + y * 7) % 256) as u8,
        ]);
    }
    image
        .save_with_format(path, ImageFormat::Png)
        .expect("RGB fixture must save");
}

fn write_oriented_jpeg(path: &Path) {
    let mut image = RgbImage::new(3, 2);
    let colors = [
        Rgb([255, 0, 0]),
        Rgb([0, 255, 0]),
        Rgb([0, 0, 255]),
        Rgb([255, 255, 0]),
        Rgb([255, 0, 255]),
        Rgb([0, 255, 255]),
    ];
    for (pixel, color) in image.pixels_mut().zip(colors) {
        *pixel = color;
    }
    image
        .save_with_format(path, ImageFormat::Jpeg)
        .expect("JPEG fixture must save");
    let jpeg = fs::read(path).expect("JPEG fixture must read");
    assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
    let mut exif =
        b"Exif\0\0MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
    let segment_len = (exif.len() + 2) as u16;
    let mut output = Vec::with_capacity(jpeg.len() + exif.len() + 64);
    output.extend_from_slice(&jpeg[..2]);
    output.extend_from_slice(&[0xff, 0xe1]);
    output.extend_from_slice(&segment_len.to_be_bytes());
    output.append(&mut exif);
    let mut icc = b"ICC_PROFILE\0\x01\x01zoos-goal1b-icc-profile-v1".to_vec();
    let icc_len = (icc.len() + 2) as u16;
    output.extend_from_slice(&[0xff, 0xe2]);
    output.extend_from_slice(&icc_len.to_be_bytes());
    output.append(&mut icc);
    output.extend_from_slice(&jpeg[2..]);
    fs::write(path, output).expect("oriented JPEG must write");
    assert_eq!(exif_orientation(&fs::read(path).unwrap()), Some(6));
}

fn exif_orientation(bytes: &[u8]) -> Option<u16> {
    let exif = bytes.windows(6).position(|window| window == b"Exif\0\0")? + 6;
    let tiff = bytes.get(exif..)?;
    let little = tiff.get(..2)? == b"II";
    let u16_at = |offset: usize| -> Option<u16> {
        let value = [*tiff.get(offset)?, *tiff.get(offset + 1)?];
        Some(if little {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let value = [
            *tiff.get(offset)?,
            *tiff.get(offset + 1)?,
            *tiff.get(offset + 2)?,
            *tiff.get(offset + 3)?,
        ];
        Some(if little {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?);
    for index in 0..count {
        let entry = ifd + 2 + index * 12;
        if u16_at(entry)? == 0x0112 {
            return u16_at(entry + 8);
        }
    }
    None
}

fn compare_pixels(left: &RgbImage, right: &RgbImage) -> Metrics {
    assert_eq!(left.dimensions(), right.dimensions());
    let mut max_abs_error = 0_u8;
    let mut absolute_sum = 0_u64;
    let mut squared_sum = 0_f64;
    for (left, right) in left.as_raw().iter().zip(right.as_raw()) {
        let difference = left.abs_diff(*right);
        max_abs_error = max_abs_error.max(difference);
        absolute_sum += u64::from(difference);
        squared_sum += f64::from(difference).powi(2);
    }
    let samples = left.as_raw().len() as f64;
    let mse = squared_sum / samples;
    Metrics {
        max_abs_error,
        mean_abs_error: absolute_sum as f64 / samples,
        psnr_db: if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * ((255.0_f64 * 255.0) / mse).log10()
        },
    }
}

fn preset_name(preset: ImagePreset) -> &'static str {
    match preset {
        ImagePreset::Photo => "photo",
        ImagePreset::Anime => "anime",
    }
}

fn env_path(name: &str) -> PathBuf {
    let path = PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")));
    assert!(path.is_absolute(), "{name} must be absolute");
    path.canonicalize()
        .unwrap_or_else(|_| panic!("{name} must exist"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate must be inside workspace")
        .to_owned()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_reader(File::open(path).expect("JSON artifact must open"))
        .expect("JSON artifact must deserialize")
}

fn sha256_file(path: &Path) -> String {
    let mut file = File::open(path).expect("hashed file must open");
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("hashed file must read");
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    format!("{:x}", digest.finalize())
}
