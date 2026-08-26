#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, RgbImage};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zoos_core::{
    ImageOutputPlan, plan_image_output, publish_verified_output, recheck_input,
    verify_partial_output,
};
use zoos_runner_protocol::{
    ImageModelId, ImageOutputFormat, ImagePreset, ImageRunnerInput, ImageRunnerOutput, ImageTask,
    ImageUpscaleJobRequest, ImageUpscaleParameters, PROTOCOL_VERSION, RunnerCapabilities,
    RunnerEvent, RunnerEventPayload,
};

const RUNTIME_ASSETS_ENV: &str = "ZOOS_M5_RUNTIME_ASSETS";
const EXPECTED_DEVICE_EVENT: &str = "gpu:0 Apple M5";

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    gate_id: String,
    scope: GateScope,
    repeats_per_case: usize,
    thresholds: Thresholds,
    runtime_assets: Vec<Artifact>,
    fixtures: Vec<Fixture>,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct GateScope {
    platform: String,
    gpu_index: u32,
    gpu_name: String,
    backend: String,
    regression_only: bool,
    cross_gpu_support_evidence: bool,
}

#[derive(Debug, Deserialize)]
struct Thresholds {
    max_abs_error: u8,
    mean_abs_error: f64,
    psnr_db: f64,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    path: PathBuf,
    size: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    format: String,
    path: PathBuf,
    size: u64,
    width: u32,
    height: u32,
    sha256: String,
    pixel_sha256: String,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    preset: String,
    scale: u8,
    fixture: String,
    golden_path: PathBuf,
    width: u32,
    height: u32,
    size: u64,
    sha256: String,
    pixel_sha256: String,
}

#[derive(Debug)]
struct RuntimeAssets {
    root: PathBuf,
    engine: PathBuf,
    models: PathBuf,
}

#[derive(Debug)]
struct PixelMetrics {
    max_abs_error: u8,
    mean_abs_error: f64,
    psnr_db: f64,
}

#[test]
#[ignore = "requires the verified local Real-ESRGAN cache and Apple M5 GPU"]
fn apple_m5_vertical_matrix_matches_goldens_and_is_deterministic() {
    let workspace = workspace_root();
    let manifest = load_manifest(&workspace);
    assert_manifest_policy(&manifest);
    let fixtures = verify_committed_artifacts(&workspace, &manifest);
    let runtime = verify_runtime_assets(&manifest);
    let wrapper = wrapper_binary();
    probe_wrapper(&wrapper, &runtime);

    let run_root = tempfile::Builder::new()
        .prefix("zoos-apple-m5-goal1a-")
        .tempdir()
        .expect("hardware-gate temporary directory must be created");

    for case in &manifest.cases {
        let fixture = fixtures
            .get(case.fixture.as_str())
            .unwrap_or_else(|| panic!("unknown fixture {}", case.fixture));
        let golden_path = absolute_repo_path(&workspace, &case.golden_path);
        let golden = decode_rgb8(&golden_path);
        let mut output_hashes = Vec::with_capacity(manifest.repeats_per_case);
        let mut pixel_hashes = Vec::with_capacity(manifest.repeats_per_case);

        for repeat in 1..=manifest.repeats_per_case {
            let job_id = format!("{}-repeat-{repeat}", case.id);
            let job_root = run_root.path().join(&job_id);
            fs::create_dir(&job_root).expect("job directory must be created");
            let source_path = absolute_repo_path(&workspace, &fixture.path);
            let input_name = source_path.file_name().expect("fixture must have a name");
            let input_path = job_root.join(input_name);
            fs::copy(&source_path, &input_path).expect("fixture must copy to isolated job root");
            assert_eq!(sha256_file(&input_path), fixture.sha256);

            let plan = plan_image_output(&input_path, case.scale, &job_id)
                .expect("core image-safety planning must accept the fixture");
            assert_eq!(plan.input.width, fixture.width);
            assert_eq!(plan.input.height, fixture.height);
            assert_eq!(plan.input.sha256, fixture.sha256);
            assert_eq!(plan.output_width, case.width);
            assert_eq!(plan.output_height, case.height);

            let request = request_for_case(&plan, case);
            let job_path = job_root.join("runner-job.json");
            serde_json::to_writer_pretty(
                File::create(&job_path).expect("runner job must be created"),
                &request,
            )
            .expect("runner job must serialize");

            let output = run_wrapper(&wrapper, &runtime, &job_path);
            assert!(
                output.status.success(),
                "{} repeat {} failed: status={} stderr={} stdout={}",
                case.id,
                repeat,
                output.status,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                output.stderr.is_empty(),
                "wrapper stderr must be empty on success: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let events = parse_events(&output.stdout, &job_id, true);
            assert_device_and_completion_events(&events, &plan.partial_path);

            let partial_hash =
                verify_partial_output(&plan).expect("core must verify upstream partial PNG");
            assert_eq!(recheck_input(&plan.input).unwrap(), fixture.sha256);
            let verification_path = job_root.join("verification.json");
            let verification = publish_verified_output(&plan, &verification_path)
                .expect("core must atomically publish verified output");
            assert_eq!(verification.input_sha256_before, fixture.sha256);
            assert_eq!(verification.input_sha256_after, fixture.sha256);
            assert_eq!(verification.output_sha256, partial_hash);
            assert!(!plan.partial_path.exists());
            assert!(plan.final_path.is_file());

            let produced = decode_rgb8(&plan.final_path);
            assert_eq!(produced.dimensions(), (case.width, case.height));
            assert!(produced.as_raw().iter().any(|channel| *channel != 0));
            let metrics = compare_pixels(&produced, &golden);
            assert!(
                metrics.max_abs_error <= manifest.thresholds.max_abs_error,
                "{} repeat {} max_abs_error {} exceeds {}",
                case.id,
                repeat,
                metrics.max_abs_error,
                manifest.thresholds.max_abs_error
            );
            assert!(
                metrics.mean_abs_error <= manifest.thresholds.mean_abs_error,
                "{} repeat {} mean_abs_error {} exceeds {}",
                case.id,
                repeat,
                metrics.mean_abs_error,
                manifest.thresholds.mean_abs_error
            );
            assert!(
                metrics.psnr_db >= manifest.thresholds.psnr_db,
                "{} repeat {} PSNR {} dB is below {} dB",
                case.id,
                repeat,
                metrics.psnr_db,
                manifest.thresholds.psnr_db
            );

            output_hashes.push(sha256_file(&plan.final_path));
            pixel_hashes.push(sha256_bytes(produced.as_raw()));
            eprintln!(
                "M5_GATE case={} repeat={} output_sha256={} pixel_sha256={} max_abs={} mean_abs={:.8} psnr_db={}",
                case.id,
                repeat,
                output_hashes.last().unwrap(),
                pixel_hashes.last().unwrap(),
                metrics.max_abs_error,
                metrics.mean_abs_error,
                if metrics.psnr_db.is_infinite() {
                    "infinity".to_owned()
                } else {
                    format!("{:.4}", metrics.psnr_db)
                }
            );
        }

        assert!(
            output_hashes.windows(2).all(|pair| pair[0] == pair[1]),
            "{} encoded PNG changed across repeats: {output_hashes:?}",
            case.id
        );
        assert!(
            pixel_hashes.windows(2).all(|pair| pair[0] == pair[1]),
            "{} decoded RGB pixels changed across repeats: {pixel_hashes:?}",
            case.id
        );
    }
}

#[test]
#[ignore = "requires the verified local Real-ESRGAN cache and Apple M5 GPU"]
fn apple_m5_cancel_terminates_process_group_and_removes_outputs() {
    let workspace = workspace_root();
    let manifest = load_manifest(&workspace);
    assert_manifest_policy(&manifest);
    let fixtures = verify_committed_artifacts(&workspace, &manifest);
    let runtime = verify_runtime_assets(&manifest);
    let wrapper = wrapper_binary();

    let run_root = tempfile::Builder::new()
        .prefix("zoos-apple-m5-cancel-")
        .tempdir()
        .expect("cancellation temporary directory must be created");
    let small_fixture = fixtures.get("rgb8-png").expect("PNG fixture must exist");
    let source = decode_rgb8(&absolute_repo_path(&workspace, &small_fixture.path));
    let large = RgbImage::from_fn(1024, 768, |x, y| {
        *source.get_pixel(x % source.width(), y % source.height())
    });
    let input_path = run_root.path().join("cancel-input.png");
    large
        .save_with_format(&input_path, ImageFormat::Png)
        .expect("cancellation fixture must save as RGB8 PNG");
    let plan = plan_image_output(&input_path, 4, "cancel-process-group")
        .expect("core must plan cancellation output");
    let cancel_case = Case {
        id: "cancel-photo-x4-png".into(),
        preset: "photo".into(),
        scale: 4,
        fixture: "generated-cancel-fixture".into(),
        golden_path: PathBuf::new(),
        width: 4096,
        height: 3072,
        size: 0,
        sha256: String::new(),
        pixel_sha256: String::new(),
    };
    let request = request_for_case(&plan, &cancel_case);
    let job_path = run_root.path().join("cancel-job.json");
    serde_json::to_writer_pretty(
        File::create(&job_path).expect("cancellation job must be created"),
        &request,
    )
    .expect("cancellation job must serialize");

    let mut command = wrapper_command(&wrapper, &runtime);
    command
        .args(["run", "--job"])
        .arg(&job_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().expect("wrapper must start without a shell");
    let process_group = i32::try_from(child.id()).expect("wrapper PID must fit i32");
    assert_eq!(unsafe { libc::getpgid(process_group) }, process_group);

    let stdout = child.stdout.take().expect("wrapper stdout must be piped");
    let (line_sender, line_receiver) = mpsc::channel();
    let stdout_task = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("wrapper stdout line must be readable");
            let _ = line_sender.send(line.clone());
            lines.push(line);
        }
        lines
    });
    let device_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(line) = line_receiver.recv_timeout(Duration::from_millis(100)) {
            let event: RunnerEvent = serde_json::from_str(&line).expect("event must be NDJSON");
            if matches!(
                event.payload,
                RunnerEventPayload::Warning { ref code, ref message }
                    if code == "GPU_DEVICE" && message == EXPECTED_DEVICE_EVENT
            ) {
                break;
            }
        }
        assert!(
            child
                .try_wait()
                .expect("wrapper status must be readable")
                .is_none(),
            "cancellation fixture completed before reporting the upstream GPU"
        );
        if Instant::now() >= device_deadline {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
            panic!("wrapper did not report the upstream GPU before cancellation");
        }
    }

    assert_eq!(unsafe { libc::kill(-process_group, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wrapper status must be readable") {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
            panic!("wrapper process group did not terminate after SIGTERM");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_task
        .join()
        .expect("wrapper stdout reader must join")
        .join("\n")
        .into_bytes();
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("wrapper stderr must be piped")
        .read_to_end(&mut stderr)
        .expect("wrapper stderr must be readable");
    assert!(
        status.code() == Some(50) || status.signal() == Some(libc::SIGTERM),
        "cancelled wrapper status={status} stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    let events = parse_events(&stdout, "cancel-process-group", status.code() == Some(50));
    if status.code() == Some(50) {
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            RunnerEventPayload::Failed { error_code, .. } if error_code == "CANCELLED"
        )));
    }

    let group_deadline = Instant::now() + Duration::from_secs(2);
    while process_group_exists(process_group) && Instant::now() < group_deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !process_group_exists(process_group),
        "wrapper or upstream process remained in process group {process_group}"
    );
    assert!(
        !plan.partial_path.exists(),
        "partial output survived cancel"
    );
    assert!(!plan.final_path.exists(), "final output survived cancel");
    let output_entries = fs::read_dir(plan.final_path.parent().unwrap())
        .expect("output directory must remain readable")
        .count();
    assert_eq!(output_entries, 0, "cancel left files in output directory");
    eprintln!(
        "M5_GATE cancel status={} process_group={} remaining_processes=0 partial=0 final=0",
        status, process_group
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must canonicalize")
}

fn load_manifest(workspace: &Path) -> Manifest {
    let path = workspace.join("tests/hardware/apple-m5-goal1a-manifest.json");
    serde_json::from_reader(BufReader::new(
        File::open(path).expect("hardware manifest must open"),
    ))
    .expect("hardware manifest must deserialize")
}

fn assert_manifest_policy(manifest: &Manifest) {
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.gate_id, "apple-m5-goal1a");
    assert_eq!(manifest.scope.platform, "macos-arm64");
    assert_eq!(manifest.scope.gpu_index, 0);
    assert_eq!(manifest.scope.gpu_name, "Apple M5");
    assert_eq!(manifest.scope.backend, "vulkan");
    assert!(manifest.scope.regression_only);
    assert!(!manifest.scope.cross_gpu_support_evidence);
    assert_eq!(manifest.repeats_per_case, 3);
    assert_eq!(manifest.cases.len(), 8);
    assert_eq!(manifest.thresholds.max_abs_error, 1);
    assert!((manifest.thresholds.mean_abs_error - 0.01).abs() < f64::EPSILON);
    assert!((manifest.thresholds.psnr_db - 70.0).abs() < f64::EPSILON);
}

fn verify_committed_artifacts<'a>(
    workspace: &Path,
    manifest: &'a Manifest,
) -> HashMap<&'a str, &'a Fixture> {
    let mut fixtures = HashMap::new();
    for fixture in &manifest.fixtures {
        let path = absolute_repo_path(workspace, &fixture.path);
        assert_eq!(fs::metadata(&path).unwrap().len(), fixture.size);
        assert_eq!(sha256_file(&path), fixture.sha256);
        let decoded = decode_rgb8(&path);
        assert_eq!(decoded.dimensions(), (fixture.width, fixture.height));
        assert_eq!(sha256_bytes(decoded.as_raw()), fixture.pixel_sha256);
        match fixture.format.as_str() {
            "png" => assert_eq!(fixture.id, "rgb8-png"),
            "jpeg" => assert_eq!(fixture.id, "rgb8-jpeg"),
            format => panic!("unsupported fixture format {format}"),
        }
        assert!(fixtures.insert(fixture.id.as_str(), fixture).is_none());
    }
    for case in &manifest.cases {
        let path = absolute_repo_path(workspace, &case.golden_path);
        assert_eq!(fs::metadata(&path).unwrap().len(), case.size);
        assert_eq!(sha256_file(&path), case.sha256);
        let decoded = decode_rgb8(&path);
        assert_eq!(decoded.dimensions(), (case.width, case.height));
        assert_eq!(sha256_bytes(decoded.as_raw()), case.pixel_sha256);
        assert!(decoded.as_raw().iter().any(|channel| *channel != 0));
    }
    fixtures
}

fn verify_runtime_assets(manifest: &Manifest) -> RuntimeAssets {
    let configured = PathBuf::from(
        env::var_os(RUNTIME_ASSETS_ENV).unwrap_or_else(|| {
            panic!(
                "{RUNTIME_ASSETS_ENV} is required; see tests/hardware/README.md for the explicit command"
            )
        }),
    );
    assert!(
        configured.is_absolute(),
        "{RUNTIME_ASSETS_ENV} must be absolute"
    );
    let root = configured
        .canonicalize()
        .expect("runtime-assets root must canonicalize");
    for asset in &manifest.runtime_assets {
        assert_safe_relative_path(&asset.path);
        let path = root.join(&asset.path);
        assert_eq!(fs::metadata(&path).unwrap().len(), asset.size);
        assert_eq!(sha256_file(&path), asset.sha256);
    }
    let engine = root.join("bin/realesrgan-ncnn-vulkan");
    assert_ne!(
        fs::metadata(&engine).unwrap().permissions().mode() & 0o111,
        0
    );
    RuntimeAssets {
        engine,
        models: root.join("models"),
        root,
    }
}

fn probe_wrapper(wrapper: &Path, runtime: &RuntimeAssets) {
    let output = wrapper_command(wrapper, runtime)
        .args(["--capabilities", "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("wrapper capabilities probe must execute");
    assert!(
        output.status.success(),
        "capabilities probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let capabilities: RunnerCapabilities =
        serde_json::from_slice(&output.stdout).expect("capabilities must be valid JSON");
    capabilities.validate().expect("capabilities must validate");
    assert_eq!(capabilities.runner_id, "zoos-runner-realesrgan");
    assert_eq!(capabilities.devices.len(), 1);
    assert_eq!(capabilities.devices[0].index, 0);
    assert_eq!(capabilities.devices[0].name, "gpu:0");
    assert_eq!(capabilities.devices[0].backend, "vulkan");
    let upstream = capabilities
        .upstream
        .expect("upstream provenance is required");
    assert_eq!(upstream.version, "0.2.0");
    assert_eq!(
        upstream.source_commit.as_deref(),
        Some("37026f49824c5cf84062e7c6a5dd71445dcf610f")
    );
}

fn request_for_case(plan: &ImageOutputPlan, case: &Case) -> ImageUpscaleJobRequest {
    let (preset, model_id) = match case.preset.as_str() {
        "photo" => (ImagePreset::Photo, ImageModelId::RealEsrganX4plus),
        "anime" => (ImagePreset::Anime, ImageModelId::RealEsrganX4plusAnime),
        preset => panic!("unsupported preset {preset}"),
    };
    ImageUpscaleJobRequest {
        protocol_version: PROTOCOL_VERSION,
        job_id: plan.job_id.clone(),
        task: ImageTask::ImageUpscale,
        input: ImageRunnerInput {
            path: plan.input.path.clone(),
            sha256: plan.input.sha256.clone(),
            width: plan.input.width,
            height: plan.input.height,
            format: plan.input.format,
        },
        output: ImageRunnerOutput {
            path: plan.partial_path.clone(),
            format: ImageOutputFormat::Png,
        },
        parameters: ImageUpscaleParameters {
            preset,
            model_id,
            scale: case.scale,
            tile_size: 256,
            gpu_id: 0,
            threads: "1:2:2".into(),
        },
    }
}

fn wrapper_binary() -> PathBuf {
    let wrapper = PathBuf::from(env!("CARGO_BIN_EXE_zoos-runner-realesrgan-bin"));
    assert!(wrapper.is_absolute(), "Cargo wrapper path must be absolute");
    wrapper
        .canonicalize()
        .expect("Cargo-built wrapper binary must canonicalize")
}

fn wrapper_command(wrapper: &Path, runtime: &RuntimeAssets) -> Command {
    assert!(wrapper.is_absolute());
    assert!(runtime.engine.is_absolute());
    assert!(runtime.models.is_absolute());
    assert!(runtime.root.is_absolute());
    let mut command = Command::new(wrapper);
    command
        .arg("--engine")
        .arg(&runtime.engine)
        .arg("--models")
        .arg(&runtime.models);
    command
}

fn run_wrapper(wrapper: &Path, runtime: &RuntimeAssets, job_path: &Path) -> Output {
    assert!(job_path.is_absolute());
    wrapper_command(wrapper, runtime)
        .args(["run", "--job"])
        .arg(job_path)
        .stdin(Stdio::null())
        .output()
        .expect("wrapper must execute without a shell")
}

fn parse_events(bytes: &[u8], job_id: &str, terminal_required: bool) -> Vec<RunnerEvent> {
    let text = std::str::from_utf8(bytes).expect("wrapper NDJSON must be UTF-8");
    let events = text
        .lines()
        .map(|line| serde_json::from_str::<RunnerEvent>(line).expect("event must be valid NDJSON"))
        .collect::<Vec<_>>();
    assert!(!events.is_empty(), "wrapper must emit events");
    for (index, event) in events.iter().enumerate() {
        event
            .validate(job_id, (index + 1) as u64)
            .expect("runner event sequence must validate");
    }
    if terminal_required {
        assert!(events.last().unwrap().is_terminal());
    }
    events
}

fn assert_device_and_completion_events(events: &[RunnerEvent], output: &Path) {
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        RunnerEventPayload::Warning { code, message }
            if code == "GPU_DEVICE" && message == EXPECTED_DEVICE_EVENT
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        RunnerEventPayload::Completed { output: completed } if completed.path == output
    )));
}

fn compare_pixels(actual: &RgbImage, expected: &RgbImage) -> PixelMetrics {
    assert_eq!(actual.dimensions(), expected.dimensions());
    let actual = actual.as_raw();
    let expected = expected.as_raw();
    assert_eq!(actual.len(), expected.len());
    let mut max_abs_error = 0_u8;
    let mut absolute_error = 0_u64;
    let mut squared_error = 0_f64;
    for (&actual, &expected) in actual.iter().zip(expected) {
        let difference = actual.abs_diff(expected);
        max_abs_error = max_abs_error.max(difference);
        absolute_error += u64::from(difference);
        squared_error += f64::from(difference).powi(2);
    }
    let samples = actual.len() as f64;
    let mean_abs_error = absolute_error as f64 / samples;
    let mean_squared_error = squared_error / samples;
    let psnr_db = if mean_squared_error == 0.0 {
        f64::INFINITY
    } else {
        10.0 * ((255.0 * 255.0) / mean_squared_error).log10()
    };
    PixelMetrics {
        max_abs_error,
        mean_abs_error,
        psnr_db,
    }
}

fn decode_rgb8(path: &Path) -> RgbImage {
    match image::open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display())) {
        DynamicImage::ImageRgb8(image) => image,
        image => panic!(
            "{} decoded as {:?}, expected RGB8",
            path.display(),
            image.color()
        ),
    }
}

fn sha256_file(path: &Path) -> String {
    let mut file = File::open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("hash input must be readable");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn absolute_repo_path(workspace: &Path, relative: &Path) -> PathBuf {
    assert_safe_relative_path(relative);
    let path = workspace.join(relative);
    assert!(path.is_absolute());
    path
}

fn assert_safe_relative_path(path: &Path) {
    assert!(!path.is_absolute());
    assert!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "manifest path must contain only normal relative components: {}",
        path.display()
    );
}

fn process_group_exists(process_group: i32) -> bool {
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
