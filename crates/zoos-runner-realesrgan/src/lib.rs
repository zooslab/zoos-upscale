use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use zoos_runner_protocol::{
    DeviceCapability, ImageModelId, ImageUpscaleJobRequest, ModelCapability, PROTOCOL_VERSION,
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
    let request: ImageUpscaleJobRequest = match File::open(job_path)
        .map_err(|error| error.to_string())
        .and_then(|file| serde_json::from_reader(file).map_err(|error| error.to_string()))
        .and_then(|request: ImageUpscaleJobRequest| {
            request
                .validate()
                .map(|()| request)
                .map_err(|error| error.to_string())
        }) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid image job: {error}");
            return EXIT_INVALID_INPUT;
        }
    };
    run_job(&request, assets, engine_hash, model_files, output)
}

fn run_job(
    request: &ImageUpscaleJobRequest,
    assets: &Assets,
    engine_hash: &str,
    model_files: &[(&str, &str)],
    output: &mut impl Write,
) -> i32 {
    let mut events = EventWriter::new(output, &request.job_id);
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
    if request.output.path.exists() {
        let _ = events.failed("OUTPUT_EXISTS", "destination already exists");
        return EXIT_UPSTREAM;
    }
    let partial = request.output.path.clone();
    let model = match request.parameters.model_id {
        ImageModelId::RealEsrganX4plus => "realesrgan-x4plus",
        ImageModelId::RealEsrganX4plusAnime => "realesrgan-x4plus-anime",
    };
    let args = upstream_args(request, &partial, assets, model);
    let mut child = match Command::new(&assets.engine)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
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
    if !partial.is_file() {
        let _ = events.failed(
            "UPSTREAM_FAILED",
            "engine succeeded without producing output",
        );
        return EXIT_UPSTREAM;
    }
    if events
        .emit(RunnerEventPayload::Completed {
            output: RunnerOutput {
                path: request.output.path.clone(),
            },
        })
        .is_err()
    {
        return EXIT_UPSTREAM;
    }
    EXIT_SUCCESS
}

fn upstream_args(
    request: &ImageUpscaleJobRequest,
    partial: &Path,
    assets: &Assets,
    model: &str,
) -> Vec<std::ffi::OsString> {
    [
        "-i".into(),
        request.input.path.as_os_str().to_owned(),
        "-o".into(),
        partial.as_os_str().to_owned(),
        "-m".into(),
        assets.models.as_os_str().to_owned(),
        "-n".into(),
        model.into(),
        "-s".into(),
        request.parameters.scale.to_string().into(),
        "-g".into(),
        "0".into(),
        "-t".into(),
        "256".into(),
        "-j".into(),
        "1:2:2".into(),
        "-f".into(),
        "png".into(),
    ]
    .into()
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
        ImageInputFormat, ImageOutputFormat, ImagePreset, ImageRunnerInput, ImageRunnerOutput,
        ImageTask, ImageUpscaleParameters,
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

    #[test]
    fn exact_upstream_arguments_preserve_unicode_and_spaces() {
        let root = Path::new("/tmp/유니 코드");
        let request = request(root);
        let assets = Assets {
            engine: root.join("engine"),
            models: root.join("models dir"),
        };
        let args = upstream_args(
            &request,
            Path::new("/tmp/결과 partial.png"),
            &assets,
            "realesrgan-x4plus",
        );
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
