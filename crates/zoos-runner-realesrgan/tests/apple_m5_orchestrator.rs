#![cfg(target_os = "macos")]

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::{ColorType, ImageDecoder, ImageFormat, ImageReader};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zoos_core::{
    ImagePreset, JobKind, JobOrchestrator, JobStatus, JobSummary, RunnerLaunchSpec, RunnerRegistry,
};
use zoos_runner_protocol::{RunnerEvent, RunnerEventPayload};

const RUNTIME_ASSETS_ENV: &str = "ZOOS_M5_RUNTIME_ASSETS";
const FIXTURE_SHA256: &str = "45509f989382199bc534485c1ef956c5cdb0f3dd8bd895eecc815d70baba3ea7";

#[derive(Debug, Deserialize)]
struct Verification {
    schema_version: u32,
    job_id: String,
    input_sha256_before: String,
    input_sha256_after: String,
    output_path: PathBuf,
    output_sha256: String,
    output_format: String,
    output_width: u32,
    output_height: u32,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    job_id: String,
    runner_id: String,
    result: Option<String>,
    exit_code: Option<i32>,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
}

#[tokio::test]
#[ignore = "requires the verified local Real-ESRGAN cache and Apple M5 GPU"]
async fn apple_m5_real_orchestrator_completes_photo_png_x2() {
    let repository = workspace_root();
    let runtime_assets = verified_runtime_assets();
    let fixture = repository.join("tests/hardware/fixtures/rgb8-pattern-64x48.png");
    assert_eq!(sha256_file(&fixture), FIXTURE_SHA256);

    let run_root = tempfile::Builder::new()
        .prefix("zoos-apple-m5-orchestrator-")
        .tempdir()
        .expect("orchestrator temporary directory must be created");
    let input_dir = run_root.path().join("input");
    fs::create_dir(&input_dir).expect("input directory must be created");
    let input = input_dir.join("photo.png");
    fs::copy(&fixture, &input).expect("fixture must copy to isolated input");
    let input_hash_before = sha256_file(&input);
    assert_eq!(input_hash_before, FIXTURE_SHA256);

    let wrapper = PathBuf::from(env!("CARGO_BIN_EXE_zoos-runner-realesrgan-bin"));
    let launch = RunnerLaunchSpec::new("zoos-runner-realesrgan", wrapper)
        .expect("wrapper path must be absolute")
        .with_arguments([
            OsString::from("--engine"),
            runtime_assets
                .join("bin/realesrgan-ncnn-vulkan")
                .into_os_string(),
            OsString::from("--models"),
            runtime_assets.join("models").into_os_string(),
        ])
        .expect("runtime arguments must be fixed and absolute");
    let registry = RunnerRegistry::with_runner(JobKind::ImageUpscale, launch);
    let workspace = run_root.path().join("workspace");
    let orchestrator = JobOrchestrator::with_runner_registry(
        &workspace,
        registry,
        Duration::from_secs(15),
        Duration::from_secs(2),
    )
    .expect("orchestrator must acquire its workspace");

    let created = orchestrator
        .create_image_job(&input, ImagePreset::Photo, 2)
        .expect("RGB8 PNG image job must be created");
    assert_eq!(created.status, JobStatus::Created);
    assert_eq!(created.kind, JobKind::ImageUpscale);
    assert_eq!(created.input_name.as_deref(), Some("photo.png"));
    assert_eq!(created.image_settings.expect("image settings").scale, 2);
    let planned_output = created.output_path.clone().expect("output must be planned");
    let expected_partial = planned_output
        .parent()
        .expect("output must have a parent")
        .join(format!(
            ".{}.zoos-{}.partial.png",
            planned_output
                .file_name()
                .expect("output must have a file name")
                .to_string_lossy(),
            created.job_id
        ));

    let started = orchestrator
        .start_job(&created.job_id)
        .await
        .expect("image job must start");
    assert_eq!(started.status, JobStatus::Probing);
    let completed = wait_for_terminal(&orchestrator, &created.job_id).await;
    assert_eq!(completed.status, JobStatus::Completed, "{completed:?}");
    assert_eq!(completed.progress_percent, 100);
    assert!(completed.error.is_none());
    assert_eq!(
        completed.output_path.as_deref(),
        Some(planned_output.as_path())
    );

    assert_eq!(sha256_file(&fixture), FIXTURE_SHA256);
    assert_eq!(sha256_file(&input), input_hash_before);
    assert!(planned_output.is_file());
    assert!(
        !fs::read_dir(planned_output.parent().expect("output parent"))
            .expect("output directory must list")
            .any(|entry| entry
                .expect("output entry must read")
                .file_name()
                .to_string_lossy()
                .contains(".partial.png")),
        "successful publish must leave no partial output"
    );
    let reader = ImageReader::open(&planned_output)
        .expect("output must open")
        .with_guessed_format()
        .expect("output format must be detected");
    assert_eq!(reader.format(), Some(ImageFormat::Png));
    let decoder = reader.into_decoder().expect("output must decode");
    assert_eq!(decoder.color_type(), ColorType::Rgb8);
    assert_eq!(decoder.dimensions(), (128, 96));
    let output_pixels = image::open(&planned_output)
        .expect("output pixels must decode")
        .into_rgb8();
    assert!(output_pixels.as_raw().iter().any(|channel| *channel != 0));

    let job_workspace = workspace.join(&created.job_id);
    let verification: Verification = read_json(&job_workspace.join("verification.json"));
    assert_eq!(verification.schema_version, 1);
    assert_eq!(verification.job_id, created.job_id);
    assert_eq!(verification.input_sha256_before, input_hash_before);
    assert_eq!(verification.input_sha256_after, input_hash_before);
    assert_eq!(verification.output_path, planned_output);
    assert_eq!(verification.output_sha256, sha256_file(&planned_output));
    assert_eq!(verification.output_format, "png");
    assert_eq!(
        (verification.output_width, verification.output_height),
        (128, 96)
    );

    let events = read_events(&job_workspace.join("logs.jsonl"));
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| event.job_id == created.job_id));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        RunnerEventPayload::Warning { code, message }
            if code == "GPU_DEVICE" && message == "gpu:0 Apple M5"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        RunnerEventPayload::Started { stage } if stage == "validating_assets"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        RunnerEventPayload::Completed { output } if output.path == expected_partial
    )));
    assert!(!expected_partial.exists());

    let manifest: Manifest = read_json(&job_workspace.join("manifest.json"));
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.job_id, created.job_id);
    assert_eq!(manifest.runner_id, "zoos-runner-realesrgan");
    assert_eq!(manifest.result.as_deref(), Some("completed"));
    assert_eq!(manifest.exit_code, Some(0));
    assert!(manifest.started_at_ms.is_some());
    assert!(manifest.finished_at_ms >= manifest.started_at_ms);
}

async fn wait_for_terminal(orchestrator: &JobOrchestrator, job_id: &str) -> JobSummary {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let current = orchestrator
                .list_jobs()
                .expect("job list must remain readable")
                .into_iter()
                .find(|job| job.job_id == job_id)
                .expect("created job must remain listed");
            if current.status.is_terminal() {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real image job must reach terminal state")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate must be inside the workspace")
        .to_owned()
}

fn verified_runtime_assets() -> PathBuf {
    let configured = PathBuf::from(env::var_os(RUNTIME_ASSETS_ENV).unwrap_or_else(|| {
        panic!("{RUNTIME_ASSETS_ENV} must point to the verified macos-universal cache")
    }));
    assert!(
        configured.is_absolute(),
        "{RUNTIME_ASSETS_ENV} must be absolute"
    );
    configured
        .canonicalize()
        .expect("verified runtime-assets directory must exist")
}

fn read_events(path: &Path) -> Vec<RunnerEvent> {
    BufReader::new(File::open(path).expect("event log must open"))
        .lines()
        .map(|line| {
            serde_json::from_str(&line.expect("event line must read"))
                .expect("event line must be valid RunnerProtocol JSON")
        })
        .collect()
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
