use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use image::{ColorType, ImageDecoder, ImageReader};
use sha2::{Digest, Sha256};
use zoos_runner_protocol::{
    DeviceCapability, ImageBackendSettingsV2, ImageDeviceV2, ImageModelId, ImageSemanticModelV2,
    ImageUpscaleJobRequest, ImageUpscaleJobRequestV2, ModelCapability, PROTOCOL_VERSION,
    RunnerCapabilities, RunnerEvent, RunnerEventPayload, RunnerOutput, RunnerTask, UpstreamInfo,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INVALID_INPUT: i32 = 10;
const EXIT_ASSET: i32 = 20;
const EXIT_UPSTREAM: i32 = 30;
const EXIT_CANCELLED: i32 = 50;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

const ENGINE_HASH: &str = "c1c35d92079085de96b9d547fd7e4464bc8a2e9ccf28d7b8c712d72ade91b7cc";
const MODEL_FILES: [(&str, &str); 4] = [
    (
        "realesrgan-x4plus.param",
        "35330ececcea33b6c397a72548e788d5d53becee4734c50b7fada36e89f10a86",
    ),
    (
        "realesrgan-x4plus.bin",
        "713ee713b0353afaa27976f0563a64a5043bd70b9bd8936c2e26e25ebcdbcddf",
    ),
    (
        "realesrgan-x4plus-anime.param",
        "2b8fb6e0ae4d2d85704ca08c119a2f5ea40add4f2ecd512eb7f4cd44b6127ed4",
    ),
    (
        "realesrgan-x4plus-anime.bin",
        "fe01c269cfd10cdef8e018ab66ebe750cf79c7af4d1f9c16c737e1295229bacc",
    ),
];

#[derive(Clone)]
struct Assets {
    engine: PathBuf,
    models: PathBuf,
}

pub fn run_cli(arguments: impl IntoIterator<Item = String>) -> i32 {
    ensure_signal_handlers();
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let parsed = parse_cli(&arguments);
    let (assets, action) = match parsed {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_INVALID_INPUT;
        }
    };
    match action {
        Action::Capabilities => match verify_assets(&assets, ENGINE_HASH, &MODEL_FILES) {
            Ok(()) => {
                println!("{}", serde_json::json!(capabilities()));
                EXIT_SUCCESS
            }
            Err(error) => {
                let code = if !assets.engine.exists() || !assets.models.is_dir() {
                    "ENGINE_NOT_INSTALLED"
                } else {
                    "ASSET_HASH_MISMATCH"
                };
                eprintln!("{code}: {error}");
                EXIT_ASSET
            }
        },
        Action::Run(job) => run_job_file(
            &job,
            &assets,
            ENGINE_HASH,
            &MODEL_FILES,
            &mut io::stdout().lock(),
        ),
    }
}

enum Action {
    Capabilities,
    Run(PathBuf),
}

fn parse_cli(arguments: &[String]) -> Result<(Assets, Action), String> {
    let mut engine = None;
    let mut models = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--engine" | "--models" => {
                let flag = &arguments[index];
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                if flag == "--engine" {
                    engine = Some(PathBuf::from(value));
                } else {
                    models = Some(PathBuf::from(value));
                }
                index += 2;
            }
            _ => {
                rest.push(arguments[index].clone());
                index += 1;
            }
        }
    }
    let assets = Assets {
        engine: engine.ok_or("--engine is required")?,
        models: models.ok_or("--models is required")?,
    };
    if !assets.engine.is_absolute() || !assets.models.is_absolute() {
        return Err("engine and model paths must be absolute".into());
    }
    let action = match rest.as_slice() {
        [flag, format] if flag == "--capabilities" && format == "--json" => Action::Capabilities,
        [command, flag, job] if command == "run" && flag == "--job" => {
            let path = PathBuf::from(job);
            if !path.is_absolute() { return Err("job path must be absolute".into()); }
            Action::Run(path)
        }
        _ => return Err("usage: zoos-runner-realesrgan --engine <absolute> --models <absolute> [--capabilities --json | run --job <absolute>]".into()),
    };
    Ok((assets, action))
}

fn capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        protocol_version: PROTOCOL_VERSION,
        runner_id: "zoos-runner-realesrgan".into(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        tasks: vec![RunnerTask::ImageUpscale],
        upstream: Some(UpstreamInfo {
            name: "Real-ESRGAN-ncnn-vulkan".into(),
            version: "0.2.0".into(),
            source_commit: Some("37026f49824c5cf84062e7c6a5dd71445dcf610f".into()),
        }),
        models: vec![
            ModelCapability {
                id: "realesrgan-x4plus".into(),
                scales: vec![2, 4],
            },
            ModelCapability {
                id: "realesrgan-x4plus-anime".into(),
                scales: vec![2, 4],
            },
        ],
        scales: vec![2, 4],
        devices: vec![DeviceCapability {
            index: 0,
            name: "gpu:0".into(),
            backend: "vulkan".into(),
        }],
        test_behaviors: Vec::new(),
    }
}

fn run_job_file(
    job_path: &Path,
    assets: &Assets,
    engine_hash: &str,
    model_files: &[(&str, &str)],
    output: &mut impl Write,
) -> i32 {
    let request = match read_job_request(job_path) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid image job: {error}");
            return EXIT_INVALID_INPUT;
        }
    };
    match request {
        SupportedRequest::V1(request) => {
            run_job(&request, assets, engine_hash, model_files, output)
        }
        SupportedRequest::V2(request) => {
            run_job_v2(&request, assets, engine_hash, model_files, output)
        }
    }
}

enum SupportedRequest {
    V1(ImageUpscaleJobRequest),
    V2(ImageUpscaleJobRequestV2),
}

fn read_job_request(path: &Path) -> Result<SupportedRequest, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let version = value
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or("protocol_version is required")?;
    match version {
        1 => {
            let request: ImageUpscaleJobRequest =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            request.validate().map_err(|error| error.to_string())?;
            Ok(SupportedRequest::V1(request))
        }
        2 => {
            let request: ImageUpscaleJobRequestV2 =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            request.validate().map_err(|error| error.to_string())?;
            Ok(SupportedRequest::V2(request))
        }
        version => Err(format!("unsupported protocol version {version}")),
    }
}

fn run_job(
    request: &ImageUpscaleJobRequest,
    assets: &Assets,
    engine_hash: &str,
    model_files: &[(&str, &str)],
    output: &mut impl Write,
) -> i32 {
    let model = match request.parameters.model_id {
        ImageModelId::RealEsrganX4plus => "realesrgan-x4plus",
        ImageModelId::RealEsrganX4plusAnime => "realesrgan-x4plus-anime",
    };
    run_upstream(
        UpstreamJob {
            job_id: &request.job_id,
            input: &request.input.path,
            output: &request.output.path,
            model,
            scale: request.parameters.scale,
            tile_size: request.parameters.tile_size,
            gpu_id: request.parameters.gpu_id,
            threads: &request.parameters.threads,
            expected_output_dimensions: None,
        },
        assets,
        engine_hash,
        model_files,
        output,
    )
}

fn run_job_v2(
    request: &ImageUpscaleJobRequestV2,
    assets: &Assets,
    engine_hash: &str,
    model_files: &[(&str, &str)],
    output: &mut impl Write,
) -> i32 {
    let mut events = EventWriter::new(output, &request.job_id);
    let (tile_size, threads) = match (
        &request.parameters.device,
        &request.parameters.backend_settings,
    ) {
        (
            ImageDeviceV2::Vulkan { index: 0 },
            ImageBackendSettingsV2::Vulkan { tile_size, threads },
        ) => (*tile_size, threads.as_str()),
        _ => {
            let _ = events.failed(
                "GPU_UNAVAILABLE",
                "Real-ESRGAN runner protocol v2 requires Vulkan device index 0 and Vulkan settings",
            );
            return EXIT_INVALID_INPUT;
        }
    };
    if let Err(error) = verify_v2_input(request) {
        let _ = events.failed(error.code, &error.message);
        return EXIT_INVALID_INPUT;
    }
    let expected_width = match request
        .input
        .width
        .checked_mul(request.parameters.native_scale.into())
    {
        Some(value) => value,
        None => {
            let _ = events.failed("OUTPUT_TOO_LARGE", "native output width overflowed");
            return EXIT_INVALID_INPUT;
        }
    };
    let expected_height = match request
        .input
        .height
        .checked_mul(request.parameters.native_scale.into())
    {
        Some(value) => value,
        None => {
            let _ = events.failed("OUTPUT_TOO_LARGE", "native output height overflowed");
            return EXIT_INVALID_INPUT;
        }
    };
    let model = match request.parameters.semantic_model {
        ImageSemanticModelV2::Photo => "realesrgan-x4plus",
        ImageSemanticModelV2::Anime => "realesrgan-x4plus-anime",
    };
    run_upstream(
        UpstreamJob {
            job_id: &request.job_id,
            input: &request.input.path,
            output: &request.output.path,
            model,
            // Both x2 and x4 user requests first produce the model's native x4 artifact.
            scale: request.parameters.native_scale,
            tile_size,
            gpu_id: 0,
            threads,
            expected_output_dimensions: Some((expected_width, expected_height)),
        },
        assets,
        engine_hash,
        model_files,
        output,
    )
}

struct InputValidationError {
    code: &'static str,
    message: String,
}

fn verify_v2_input(request: &ImageUpscaleJobRequestV2) -> Result<(), InputValidationError> {
    let actual_hash = hash_file(&request.input.path).map_err(|message| InputValidationError {
        code: "UNSUPPORTED_IMAGE_MODE",
        message,
    })?;
    if actual_hash != request.input.sha256 {
        return Err(InputValidationError {
            code: "INPUT_CHANGED",
            message: "normalized input SHA-256 does not match the request".into(),
        });
    }
    let reader = ImageReader::open(&request.input.path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|error| InputValidationError {
            code: "UNSUPPORTED_IMAGE_MODE",
            message: format!("normalized input cannot be opened: {error}"),
        })?;
    if reader.format() != Some(image::ImageFormat::Png) {
        return Err(InputValidationError {
            code: "UNSUPPORTED_IMAGE_MODE",
            message: "normalized input must be PNG".into(),
        });
    }
    let decoder = reader
        .into_decoder()
        .map_err(|error| InputValidationError {
            code: "UNSUPPORTED_IMAGE_MODE",
            message: format!("normalized input cannot be decoded: {error}"),
        })?;
    if decoder.color_type() != ColorType::Rgb8
        || decoder.dimensions() != (request.input.width, request.input.height)
    {
        return Err(InputValidationError {
            code: "UNSUPPORTED_IMAGE_MODE",
            message: "normalized input must be RGB8 PNG with the declared dimensions".into(),
        });
    }
    drop(decoder);
    image::open(&request.input.path).map_err(|error| InputValidationError {
        code: "UNSUPPORTED_IMAGE_MODE",
        message: format!("normalized input pixels cannot be decoded: {error}"),
    })?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct UpstreamJob<'a> {
    job_id: &'a str,
    input: &'a Path,
    output: &'a Path,
    model: &'a str,
    scale: u8,
    tile_size: u32,
    gpu_id: u32,
    threads: &'a str,
    expected_output_dimensions: Option<(u32, u32)>,
}

fn run_upstream(
    job: UpstreamJob<'_>,
    assets: &Assets,
    engine_hash: &str,
    model_files: &[(&str, &str)],
    output: &mut impl Write,
) -> i32 {
    let mut events = EventWriter::new(output, job.job_id);
    if events
        .emit(RunnerEventPayload::Started {
            stage: "validating_assets".into(),
        })
        .is_err()
    {
        return EXIT_UPSTREAM;
    }
    if let Err(error) = verify_assets(assets, engine_hash, model_files) {
        let code = if !assets.engine.exists() || !assets.models.is_dir() {
            "ENGINE_NOT_INSTALLED"
        } else {
            "ASSET_HASH_MISMATCH"
        };
        let _ = events.failed(code, &error);
        return EXIT_ASSET;
    }
    match fs::symlink_metadata(job.output) {
        Ok(_) => {
            let _ = events.failed("OUTPUT_EXISTS", "destination already exists");
            return EXIT_UPSTREAM;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = events.failed(
                "UPSTREAM_FAILED",
                &format!("could not inspect output: {error}"),
            );
            return EXIT_UPSTREAM;
        }
    }
    let partial = match claim_private_upstream_output(job.output) {
        Ok(path) => path,
        Err(error) => {
            let _ = events.failed("UPSTREAM_FAILED", &error);
            return EXIT_UPSTREAM;
        }
    };
    let args = upstream_args(&job, assets, &partial);
    let mut child = match Command::new(&assets.engine)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            let _ = events.failed(
                "ENGINE_NOT_INSTALLED",
                &format!("could not start verified engine: {error}"),
            );
            return EXIT_ASSET;
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = thread::spawn(move || {
        let _ = io::copy(&mut BufReader::new(stdout), &mut io::sink());
    });
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stderr_task = thread::spawn(move || {
        let mut total = 0usize;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if total < MAX_DIAGNOSTIC_BYTES {
                let remaining = MAX_DIAGNOSTIC_BYTES - total;
                let bounded = line.chars().take(remaining.min(1024)).collect::<String>();
                total += bounded.len();
                let _ = stderr_tx.send(bounded);
            }
        }
    });
    let started = Instant::now();
    let mut last_event = Instant::now();
    let mut last_percent = 0u64;
    let mut diagnostic = String::new();
    let mut device_reported = false;
    loop {
        while let Ok(line) = stderr_rx.try_recv() {
            if !diagnostic.is_empty() {
                diagnostic.push('\n');
            }
            diagnostic.push_str(&line);
            if !device_reported && let Some(device) = parse_device(&line) {
                let _ = events.emit(RunnerEventPayload::Warning {
                    code: "GPU_DEVICE".into(),
                    message: format!("gpu:0 {device}"),
                });
                device_reported = true;
                last_event = Instant::now();
            } else if let Some(percent) = parse_percent(&line) {
                last_percent = percent.max(last_percent);
                let _ = events.progress(
                    "upscaling",
                    last_percent,
                    100,
                    started.elapsed(),
                    Some("percent"),
                );
                last_event = Instant::now();
            }
        }
        if CANCELLED.swap(false, Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&partial);
            let _ = events.failed("CANCELLED", "image upscale was cancelled");
            let _ = stdout_task.join();
            let _ = stderr_task.join();
            return EXIT_CANCELLED;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = stdout_task.join();
                let _ = stderr_task.join();
                if !status.success() {
                    let _ = fs::remove_file(&partial);
                    let code = if status.code().is_none() {
                        "CANCELLED"
                    } else if last_percent == 0 {
                        "GPU_UNAVAILABLE"
                    } else {
                        "UPSTREAM_FAILED"
                    };
                    let detail = diagnostic
                        .lines()
                        .last()
                        .unwrap_or("no upstream diagnostic");
                    let _ = events.failed(
                        code,
                        &format!("Real-ESRGAN exited with status {status}: {detail}"),
                    );
                    return if code == "CANCELLED" {
                        EXIT_CANCELLED
                    } else {
                        EXIT_UPSTREAM
                    };
                }
                break;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&partial);
                let _ = events.failed("UPSTREAM_FAILED", &error.to_string());
                return EXIT_UPSTREAM;
            }
        }
        if last_event.elapsed() >= HEARTBEAT_INTERVAL {
            let _ = events.progress(
                "upscaling",
                last_percent,
                100,
                started.elapsed(),
                Some("heartbeat"),
            );
            last_event = Instant::now();
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !partial.is_file() || fs::metadata(&partial).is_ok_and(|metadata| metadata.len() == 0) {
        let _ = fs::remove_file(&partial);
        let _ = events.failed(
            "UPSTREAM_FAILED",
            "engine succeeded without producing output",
        );
        return EXIT_UPSTREAM;
    }
    if let Err(error) = File::open(&partial).and_then(|file| file.sync_all()) {
        let _ = fs::remove_file(&partial);
        let _ = events.failed(
            "UPSTREAM_FAILED",
            &format!("could not sync native output: {error}"),
        );
        return EXIT_UPSTREAM;
    }
    if let Some(expected) = job.expected_output_dimensions
        && let Err(error) = verify_v2_output(&partial, expected)
    {
        let _ = fs::remove_file(&partial);
        let _ = events.failed("UPSTREAM_FAILED", &error);
        return EXIT_UPSTREAM;
    }
    if let Err(error) = fs::hard_link(&partial, job.output) {
        let _ = fs::remove_file(&partial);
        let code = if error.kind() == io::ErrorKind::AlreadyExists {
            "OUTPUT_EXISTS"
        } else {
            "UPSTREAM_FAILED"
        };
        let _ = events.failed(code, &format!("could not publish native output: {error}"));
        return EXIT_UPSTREAM;
    }
    if let Err(error) = fs::remove_file(&partial) {
        let _ = fs::remove_file(job.output);
        let _ = events.failed(
            "UPSTREAM_FAILED",
            &format!("could not remove private native output: {error}"),
        );
        return EXIT_UPSTREAM;
    }
    if events
        .emit(RunnerEventPayload::Completed {
            output: RunnerOutput {
                path: job.output.to_path_buf(),
            },
        })
        .is_err()
    {
        let _ = fs::remove_file(job.output);
        return EXIT_UPSTREAM;
    }
    EXIT_SUCCESS
}

fn verify_v2_output(path: &Path, expected: (u32, u32)) -> Result<(), String> {
    let reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|error| format!("native x4 output cannot be opened: {error}"))?;
    if reader.format() != Some(image::ImageFormat::Png) {
        return Err("native x4 output is not PNG".into());
    }
    let decoder = reader
        .into_decoder()
        .map_err(|error| format!("native x4 output cannot be decoded: {error}"))?;
    if decoder.color_type() != ColorType::Rgb8 || decoder.dimensions() != expected {
        return Err(format!(
            "native x4 output must be RGB8 PNG with dimensions {}x{}",
            expected.0, expected.1
        ));
    }
    drop(decoder);
    image::open(path)
        .map_err(|error| format!("native x4 output pixels cannot be decoded: {error}"))?;
    Ok(())
}

fn upstream_args(
    job: &UpstreamJob<'_>,
    assets: &Assets,
    engine_output: &Path,
) -> Vec<std::ffi::OsString> {
    [
        "-i".into(),
        job.input.as_os_str().to_owned(),
        "-o".into(),
        engine_output.as_os_str().to_owned(),
        "-m".into(),
        assets.models.as_os_str().to_owned(),
        "-n".into(),
        job.model.into(),
        "-s".into(),
        job.scale.to_string().into(),
        "-g".into(),
        job.gpu_id.to_string().into(),
        "-t".into(),
        job.tile_size.to_string().into(),
        "-j".into(),
        job.threads.into(),
        "-f".into(),
        "png".into(),
    ]
    .into()
}

static PRIVATE_OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn claim_private_upstream_output(destination: &Path) -> Result<PathBuf, String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "native output has no parent directory".to_string())?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "native output filename is invalid".to_string())?;
    for _ in 0..100 {
        let sequence = PRIVATE_OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{name}.zoos-upstream-{}-{sequence}.partial.png",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                if let Err(error) = file.sync_all() {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error.to_string());
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not claim private native output: {error}")),
        }
    }
    Err("could not allocate a private native output path".into())
}

fn verify_assets(
    assets: &Assets,
    engine_hash: &str,
    models: &[(&str, &str)],
) -> Result<(), String> {
    if !assets.engine.is_file() || !assets.models.is_dir() {
        return Err("verified runtime assets are not installed".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&assets.engine)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err("verified engine is not executable".into());
        }
    }
    verify_hash(&assets.engine, engine_hash)?;
    for (name, hash) in models {
        verify_hash(&assets.models.join(name), hash)?;
    }
    Ok(())
}

fn verify_hash(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(format!("asset hash mismatch for {}", path.display()));
    }
    Ok(())
}

fn parse_percent(line: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|part| part.strip_suffix('%')?.parse::<f64>().ok())
        .map(|value| value.clamp(0.0, 100.0).round() as u64)
}

fn parse_device(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("[0 ")?;
    let value = rest.get(..rest.find(']')?)?.trim();
    (!value.is_empty()).then_some(value)
}

struct EventWriter<'a, W> {
    output: &'a mut W,
    job_id: &'a str,
    sequence: u64,
}
impl<'a, W: Write> EventWriter<'a, W> {
    fn new(output: &'a mut W, job_id: &'a str) -> Self {
        Self {
            output,
            job_id,
            sequence: 1,
        }
    }
    fn emit(&mut self, payload: RunnerEventPayload) -> io::Result<()> {
        serde_json::to_writer(
            &mut *self.output,
            &RunnerEvent::new(self.sequence, self.job_id, payload),
        )?;
        self.output.write_all(b"\n")?;
        self.output.flush()?;
        self.sequence += 1;
        Ok(())
    }
    fn failed(&mut self, code: &str, message: &str) -> io::Result<()> {
        self.emit(RunnerEventPayload::Failed {
            error_code: code.into(),
            message: message.into(),
        })
    }
    fn progress(
        &mut self,
        stage: &str,
        completed: u64,
        total: u64,
        elapsed: Duration,
        chunk: Option<&str>,
    ) -> io::Result<()> {
        self.emit(RunnerEventPayload::Progress {
            stage: stage.into(),
            completed_units: completed,
            total_units: total,
            unit: "percent".into(),
            elapsed_ms: elapsed.as_millis() as u64,
            chunk_id: chunk.map(str::to_owned),
            rate: None,
            rate_unit: None,
            estimated_remaining_ms: None,
        })
    }
}

static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn cancellation_handler(_: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn ensure_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            cancellation_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            cancellation_handler as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn ensure_signal_handlers() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use zoos_runner_protocol::{
        IMAGE_PROTOCOL_VERSION_V2, ImageInferenceFormatV2, ImageInferenceInputV2, ImageInputFormat,
        ImageIntermediateOutputV2, ImageOutputFormat, ImagePixelFormatV2, ImagePreset,
        ImageRunnerInput, ImageRunnerOutput, ImageTask, ImageUpscaleParameters,
        ImageUpscaleParametersV2,
    };

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    fn request(root: &Path) -> ImageUpscaleJobRequest {
        ImageUpscaleJobRequest {
            protocol_version: 1,
            job_id: "job-한글".into(),
            task: ImageTask::ImageUpscale,
            input: ImageRunnerInput {
                path: root.join("입력 사진.png"),
                sha256: "0".repeat(64),
                width: 1,
                height: 1,
                format: ImageInputFormat::Png,
            },
            output: ImageRunnerOutput {
                path: root.join("결과 폴더").join("출력.png"),
                format: ImageOutputFormat::Png,
            },
            parameters: ImageUpscaleParameters {
                preset: ImagePreset::Photo,
                model_id: ImageModelId::RealEsrganX4plus,
                scale: 2,
                tile_size: 256,
                gpu_id: 0,
                threads: "1:2:2".into(),
            },
        }
    }

    fn request_v2(root: &Path) -> ImageUpscaleJobRequestV2 {
        let input = root.join("normalized input.png");
        image::RgbImage::from_pixel(2, 3, image::Rgb([10, 20, 30]))
            .save(&input)
            .unwrap();
        ImageUpscaleJobRequestV2 {
            protocol_version: IMAGE_PROTOCOL_VERSION_V2,
            job_id: "v2-job".into(),
            task: ImageTask::ImageUpscale,
            input: ImageInferenceInputV2 {
                sha256: hash(&fs::read(&input).unwrap()),
                path: input,
                width: 2,
                height: 3,
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            output: ImageIntermediateOutputV2 {
                path: root.join("native-x4.png"),
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            parameters: ImageUpscaleParametersV2 {
                semantic_model: ImageSemanticModelV2::Photo,
                requested_scale: 2,
                native_scale: 4,
                device: ImageDeviceV2::Vulkan { index: 0 },
                backend_settings: ImageBackendSettingsV2::Vulkan {
                    tile_size: 256,
                    threads: "1:2:2".into(),
                },
            },
        }
    }

    #[test]
    fn exact_upstream_arguments_preserve_unicode_and_spaces() {
        let root = Path::new("/tmp/유니 코드");
        let request = request(root);
        let assets = Assets {
            engine: root.join("engine"),
            models: root.join("models dir"),
        };
        let job = UpstreamJob {
            job_id: &request.job_id,
            input: &request.input.path,
            output: Path::new("/tmp/결과 partial.png"),
            model: "realesrgan-x4plus",
            scale: request.parameters.scale,
            tile_size: request.parameters.tile_size,
            gpu_id: request.parameters.gpu_id,
            threads: &request.parameters.threads,
            expected_output_dimensions: None,
        };
        let args = upstream_args(&job, &assets, job.output);
        assert_eq!(
            args,
            vec![
                "-i",
                "/tmp/유니 코드/입력 사진.png",
                "-o",
                "/tmp/결과 partial.png",
                "-m",
                "/tmp/유니 코드/models dir",
                "-n",
                "realesrgan-x4plus",
                "-s",
                "2",
                "-g",
                "0",
                "-t",
                "256",
                "-j",
                "1:2:2",
                "-f",
                "png"
            ]
            .into_iter()
            .map(Into::into)
            .collect::<Vec<std::ffi::OsString>>()
        );
    }

    #[test]
    fn v2_exact_arguments_always_request_native_x4() {
        let root = Path::new("/tmp/v2 gpu");
        let assets = Assets {
            engine: root.join("engine"),
            models: root.join("models"),
        };
        let job = UpstreamJob {
            job_id: "v2",
            input: &root.join("normalized.png"),
            output: &root.join("native-x4.png"),
            model: "realesrgan-x4plus-anime",
            scale: 4,
            tile_size: 256,
            gpu_id: 0,
            threads: "1:2:2",
            expected_output_dimensions: Some((8, 12)),
        };
        assert_eq!(
            upstream_args(&job, &assets, job.output),
            vec![
                "-i",
                "/tmp/v2 gpu/normalized.png",
                "-o",
                "/tmp/v2 gpu/native-x4.png",
                "-m",
                "/tmp/v2 gpu/models",
                "-n",
                "realesrgan-x4plus-anime",
                "-s",
                "4",
                "-g",
                "0",
                "-t",
                "256",
                "-j",
                "1:2:2",
                "-f",
                "png",
            ]
            .into_iter()
            .map(Into::into)
            .collect::<Vec<std::ffi::OsString>>()
        );
    }

    #[test]
    fn v2_cpu_request_is_rejected_by_gpu_runner() {
        let temp = tempdir().unwrap();
        let mut request = request_v2(temp.path());
        request.parameters.device = ImageDeviceV2::Cpu;
        request.parameters.backend_settings = ImageBackendSettingsV2::OrtCpu {
            tile_size: 128,
            intra_threads: 2,
            inter_threads: 1,
        };
        let assets = Assets {
            engine: temp.path().join("missing engine"),
            models: temp.path().join("missing models"),
        };
        let mut output = Vec::new();
        assert_eq!(
            run_job_v2(&request, &assets, "", &[], &mut output),
            EXIT_INVALID_INPUT
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("GPU_UNAVAILABLE")
        );
    }

    #[test]
    fn v2_input_hash_mismatch_is_rejected_before_engine_start() {
        let temp = tempdir().unwrap();
        let mut request = request_v2(temp.path());
        request.input.sha256 = "0".repeat(64);
        let assets = Assets {
            engine: temp.path().join("missing engine"),
            models: temp.path().join("missing models"),
        };
        let mut output = Vec::new();
        assert_eq!(
            run_job_v2(&request, &assets, "", &[], &mut output),
            EXIT_INVALID_INPUT
        );
        assert!(String::from_utf8(output).unwrap().contains("INPUT_CHANGED"));
        assert!(!request.output.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn requested_output_dangling_symlink_is_not_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("결과 폴더")).unwrap();
        fs::create_dir(root.join("models")).unwrap();
        let engine = root.join("engine");
        fs::write(&engine, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        let model = root.join("models/model");
        fs::write(&model, b"model").unwrap();
        let request = request(root);
        let external = root.join("outside.png");
        symlink(&external, &request.output.path).unwrap();
        let assets = Assets {
            engine: engine.clone(),
            models: root.join("models"),
        };
        let mut output = Vec::new();

        assert_eq!(
            run_job(
                &request,
                &assets,
                &hash(&fs::read(engine).unwrap()),
                &[("model", &hash(b"model"))],
                &mut output,
            ),
            EXIT_UPSTREAM
        );
        assert!(String::from_utf8(output).unwrap().contains("OUTPUT_EXISTS"));
        assert!(!external.exists());
        assert!(
            fs::symlink_metadata(request.output.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn v2_invalid_native_output_is_removed() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("models")).unwrap();
        let engine = root.join("engine");
        fs::write(
            &engine,
            "#!/bin/sh\nout=''\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -o ]; then out=$2; shift 2; else shift; fi; done\nprintf invalid > \"$out\"\n",
        )
        .unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("models/model"), b"model").unwrap();
        let assets = Assets {
            engine: engine.clone(),
            models: root.join("models"),
        };
        let request = request_v2(root);
        let mut output = Vec::new();
        assert_eq!(
            run_job_v2(
                &request,
                &assets,
                &hash(&fs::read(engine).unwrap()),
                &[("model", &hash(b"model"))],
                &mut output,
            ),
            EXIT_UPSTREAM
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("UPSTREAM_FAILED")
        );
        assert!(!request.output.path.exists());
    }

    #[test]
    fn parses_real_upstream_percent_and_device_lines() {
        assert_eq!(parse_percent("25.00%"), Some(25));
        assert_eq!(parse_percent("100.00%"), Some(100));
        assert_eq!(parse_device("[0 Apple M5]  queueC=..."), Some("Apple M5"));
    }

    #[test]
    fn fake_upstream_runs_without_path_or_shell_and_emits_heartbeat() {
        ensure_signal_handlers();
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("결과 폴더")).unwrap();
        fs::create_dir(root.join("models")).unwrap();
        let engine = root.join("fake engine");
        fs::write(&engine, "#!/bin/sh\nout=''\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -o ]; then out=$2; shift 2; else shift; fi; done\n/bin/sleep 3\nprintf png > \"$out\"\n").unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        let model = root.join("models/model");
        fs::write(&model, b"model").unwrap();
        let assets = Assets {
            engine: engine.clone(),
            models: root.join("models"),
        };
        let mut output = Vec::new();
        let code = run_job(
            &request(root),
            &assets,
            &hash(&fs::read(engine).unwrap()),
            &[("model", &hash(b"model"))],
            &mut output,
        );
        assert_eq!(code, 0);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("heartbeat"));
        assert!(request(root).output.path.is_file());
    }

    #[test]
    fn hash_mismatch_is_structured() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("결과 폴더")).unwrap();
        fs::create_dir(root.join("models")).unwrap();
        let engine = root.join("engine");
        fs::write(&engine, b"bad").unwrap();
        let assets = Assets {
            engine: engine.clone(),
            models: root.join("models"),
        };
        let mut output = Vec::new();
        assert_eq!(
            run_job(&request(root), &assets, &"0".repeat(64), &[], &mut output),
            EXIT_ASSET
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("ASSET_HASH_MISMATCH")
        );
    }

    #[test]
    fn upstream_failure_is_structured_and_leaves_no_partial() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("결과 폴더")).unwrap();
        fs::create_dir(root.join("models")).unwrap();
        let engine = root.join("engine");
        fs::write(&engine, "#!/bin/sh\nexit 7\n").unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("models/model"), b"model").unwrap();
        let assets = Assets {
            engine: engine.clone(),
            models: root.join("models"),
        };
        let request = request(root);
        let mut output = Vec::new();
        assert_eq!(
            run_job(
                &request,
                &assets,
                &hash(&fs::read(engine).unwrap()),
                &[("model", &hash(b"model"))],
                &mut output,
            ),
            EXIT_UPSTREAM
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("GPU_UNAVAILABLE")
        );
        assert!(!request.output.path.exists());
    }

    #[test]
    fn malformed_job_is_rejected() {
        let temp = tempdir().unwrap();
        let job = temp.path().join("job.json");
        fs::write(&job, "{}").unwrap();
        let assets = Assets {
            engine: temp.path().join("engine"),
            models: temp.path().join("models"),
        };
        let mut output = Vec::new();
        assert_eq!(
            run_job_file(&job, &assets, "", &[], &mut output),
            EXIT_INVALID_INPUT
        );
        assert!(output.is_empty());
    }
}
