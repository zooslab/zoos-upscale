use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use zoos_runner_protocol::{
    DeviceCapability, ModelCapability, MuxStreamAction, MuxStreamKind, PROTOCOL_VERSION,
    RationalRate, RunnerCapabilities, RunnerEvent, RunnerEventPayload, RunnerOutput, RunnerTask,
    UpstreamInfo, VideoContainer, VideoDevice, VideoInterpolateJobRequest,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INVALID_INPUT: i32 = 10;
const EXIT_ASSET: i32 = 20;
const EXIT_UPSTREAM: i32 = 30;
const EXIT_CANCELLED: i32 = 50;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const PROCESS_TERM_GRACE: Duration = Duration::from_millis(750);
const MEDIA_PHASE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const INTERPOLATION_PHASE_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const FFMPEG_HASH: &str = "653e700a788f3376ebc3817a3dcda56e111111410f7edd8eea919c4089216d4e";
const FFPROBE_HASH: &str = "edaf9c5f53aef960ceb5f779d986e7dea86ee549e6716a2c03b70010b88a4da6";
const RIFE_ENGINE_HASH: &str = "d11429c72f0cddfb170fd131ee9373dc5329a5729c4382c0acfd40092e5ed19a";
const RIFE_MODEL_FILES: [(&str, &str); 2] = [
    (
        "flownet.param",
        "724569596bcd1e7b9fa50455c604777ebed99746d2ef40aa86e31b5725f1053c",
    ),
    (
        "flownet.bin",
        "f334ed2260149ce0188a6dcf049844e8b0cdd912e01cbcfb63553157d2508958",
    ),
];

#[derive(Debug, Clone)]
struct Assets {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    engine: PathBuf,
    models: PathBuf,
}

enum Action {
    Capabilities,
    Run(PathBuf),
}

pub fn run_cli(arguments: impl IntoIterator<Item = String>) -> i32 {
    ensure_signal_handlers();
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let (assets, action) = match parse_cli(&arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_INVALID_INPUT;
        }
    };
    match action {
        Action::Capabilities => match verify_assets(&assets) {
            Ok(()) => {
                println!("{}", serde_json::json!(capabilities()));
                EXIT_SUCCESS
            }
            Err(error) => {
                let code = if assets_installed(&assets) {
                    "ASSET_HASH_MISMATCH"
                } else {
                    "ENGINE_NOT_INSTALLED"
                };
                eprintln!("{code}: {error}");
                EXIT_ASSET
            }
        },
        Action::Run(job) => run_job_file(&job, &assets, &mut io::stdout().lock()),
    }
}

fn parse_cli(arguments: &[String]) -> Result<(Assets, Action), String> {
    let mut ffmpeg = None;
    let mut ffprobe = None;
    let mut engine = None;
    let mut models = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let target = match arguments[index].as_str() {
            "--ffmpeg" => Some(&mut ffmpeg),
            "--ffprobe" => Some(&mut ffprobe),
            "--engine" => Some(&mut engine),
            "--models" => Some(&mut models),
            _ => None,
        };
        if let Some(target) = target {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {}", arguments[index]))?;
            *target = Some(PathBuf::from(value));
            index += 2;
        } else {
            rest.push(arguments[index].clone());
            index += 1;
        }
    }
    let assets = Assets {
        ffmpeg: ffmpeg.ok_or("--ffmpeg is required")?,
        ffprobe: ffprobe.ok_or("--ffprobe is required")?,
        engine: engine.ok_or("--engine is required")?,
        models: models.ok_or("--models is required")?,
    };
    if [
        assets.ffmpeg.as_path(),
        assets.ffprobe.as_path(),
        assets.engine.as_path(),
        assets.models.as_path(),
    ]
    .iter()
    .any(|path| !path.is_absolute())
    {
        return Err("all runtime asset paths must be absolute".into());
    }
    let action = match rest.as_slice() {
        [flag, format] if flag == "--capabilities" && format == "--json" => {
            Action::Capabilities
        }
        [command, flag, job] if command == "run" && flag == "--job" => {
            let job = PathBuf::from(job);
            if !job.is_absolute() {
                return Err("job path must be absolute".into());
            }
            Action::Run(job)
        }
        _ => return Err("usage: zoos-runner-rife --ffmpeg <absolute> --ffprobe <absolute> --engine <absolute> --models <absolute> [--capabilities --json | run --job <absolute>]".into()),
    };
    Ok((assets, action))
}

fn capabilities() -> RunnerCapabilities {
    RunnerCapabilities {
        protocol_version: PROTOCOL_VERSION,
        runner_id: "zoos-runner-rife".into(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        tasks: vec![RunnerTask::VideoInterpolate],
        upstream: Some(UpstreamInfo {
            name: "rife-ncnn-vulkan".into(),
            version: "20221029".into(),
            source_commit: Some("a7532fc".into()),
        }),
        models: vec![ModelCapability {
            id: "rife-v4.6".into(),
            scales: vec![2],
        }],
        scales: vec![2],
        devices: vec![
            DeviceCapability {
                index: 0,
                name: "gpu:0".into(),
                backend: "vulkan".into(),
            },
            DeviceCapability {
                index: 0,
                name: "cpu".into(),
                backend: "ncnn_cpu".into(),
            },
        ],
        test_behaviors: Vec::new(),
    }
}

fn run_job_file(job_path: &Path, assets: &Assets, output: &mut impl Write) -> i32 {
    let request: VideoInterpolateJobRequest = match fs::read(job_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        .and_then(|request: VideoInterpolateJobRequest| {
            request.validate().map_err(|error| error.to_string())?;
            Ok(request)
        }) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid video job: {error}");
            return EXIT_INVALID_INPUT;
        }
    };
    let mut events = EventWriter::new(output, &request.job_id);
    if let Err(error) = verify_assets(assets) {
        let code = if assets_installed(assets) {
            "ASSET_HASH_MISMATCH"
        } else {
            "ENGINE_NOT_INSTALLED"
        };
        let _ = events.failed(code, &error);
        return EXIT_ASSET;
    }
    if let Err(error) = validate_private_paths(&request, job_path) {
        let _ = events.failed("INVALID_JOB", &error);
        return EXIT_INVALID_INPUT;
    }
    if sha256_file(&request.input.path).ok().as_deref() != Some(request.input.sha256.as_str()) {
        let _ = events.failed("INPUT_CHANGED", "input hash changed before execution");
        return EXIT_INVALID_INPUT;
    }
    let _ = events.emit(RunnerEventPayload::Started {
        stage: "extracting".into(),
    });
    let device_message = match request.parameters.device {
        VideoDevice::Vulkan { index } => format!("gpu:{index} | rife-ncnn-vulkan"),
        VideoDevice::NcnnCpu => "cpu | rife-ncnn-vulkan".into(),
    };
    let _ = events.emit(RunnerEventPayload::Warning {
        code: "VIDEO_DEVICE".into(),
        message: device_message,
    });

    let execution = match execute_video_job(&request, assets, &mut events) {
        Ok(execution) => execution,
        Err(failure) => {
            cleanup_job_outputs(&request);
            let _ = events.failed(failure.code, &failure.message);
            return failure.exit_code;
        }
    };
    if sha256_file(&request.input.path).ok().as_deref() != Some(request.input.sha256.as_str()) {
        cleanup_job_outputs(&request);
        let _ = events.failed("INPUT_CHANGED", "input hash changed during execution");
        return EXIT_INVALID_INPUT;
    }
    if !is_regular_nonempty(&request.output.path) {
        cleanup_job_outputs(&request);
        let _ = events.failed(
            "UPSTREAM_FAILED",
            "mux completed without a regular output file",
        );
        return EXIT_UPSTREAM;
    }
    if let Err(error) = cleanup_success_intermediates(&request) {
        cleanup_job_outputs(&request);
        let _ = events.failed("UPSTREAM_FAILED", &error);
        return EXIT_UPSTREAM;
    }
    if let Err(error) = write_runner_evidence(&request, &execution) {
        cleanup_job_outputs(&request);
        let _ = events.failed("UPSTREAM_FAILED", &error);
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
        cleanup_job_outputs(&request);
        return EXIT_UPSTREAM;
    }
    EXIT_SUCCESS
}

#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
    exit_code: i32,
}

struct ExecutionStats {
    chunk_count: usize,
    scene_cut_count: u64,
}

impl Failure {
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            code: "UPSTREAM_FAILED",
            message: message.into(),
            exit_code: EXIT_UPSTREAM,
        }
    }

    fn cancelled() -> Self {
        Self {
            code: "CANCELLED",
            message: "video interpolation was cancelled".into(),
            exit_code: EXIT_CANCELLED,
        }
    }
}

fn execute_video_job<W: Write>(
    request: &VideoInterpolateJobRequest,
    assets: &Assets,
    events: &mut EventWriter<'_, W>,
) -> Result<ExecutionStats, Failure> {
    fs::create_dir_all(&request.work.root).map_err(|error| Failure::upstream(error.to_string()))?;
    let chunks = plan_chunks(
        request.parameters.frame_count,
        u64::from(request.parameters.chunk_frames),
    )?;
    let started = Instant::now();
    let chunk_dir = request.work.root.join("chunks");
    fs::create_dir(&chunk_dir).map_err(|error| Failure::upstream(error.to_string()))?;
    let mut chunk_paths = Vec::with_capacity(chunks.len());
    let mut scene_cut_count = 0u64;
    for (position, chunk) in chunks.iter().enumerate() {
        reset_directory(&request.work.input_frames)?;
        reset_directory(&request.work.output_frames)?;
        let extract_args = ffmpeg_extract_args(request, *chunk);
        run_command(
            &assets.ffmpeg,
            &extract_args,
            events,
            CommandProgress {
                stage: "extracting",
                chunk_id: format!("chunk-{position}"),
                completed: position as u64,
                total: chunks.len() as u64,
                started,
                deadline: MEDIA_PHASE_TIMEOUT,
                watch: Some((request.work.input_frames.clone(), input_frame_count(*chunk))),
            },
        )?;
        let input_count = chunk.end_frame - chunk.start_frame + 1;
        require_png_count(&request.work.input_frames, input_count)?;
        let rife_args = rife_args(request, assets, input_count);
        run_command(
            &assets.engine,
            &rife_args,
            events,
            CommandProgress {
                stage: "interpolating",
                chunk_id: format!("chunk-{position}"),
                completed: position as u64,
                total: chunks.len() as u64,
                started,
                deadline: INTERPOLATION_PHASE_TIMEOUT,
                watch: Some((request.work.output_frames.clone(), input_count * 2)),
            },
        )?;
        require_png_count(&request.work.output_frames, input_count * 2)?;
        scene_cut_count += repair_rife_frames(
            &request.work.input_frames,
            &request.work.output_frames,
            input_count,
            request.parameters.scene_threshold_permille,
        )?;
        require_png_count(&request.work.output_frames, input_count * 2)?;
        let expected_frames = chunk_output_frames(*chunk);
        // NUT preserves the exact rational frame time base. Matroska's default millisecond
        // time base quantizes 60000/1001 and made the final MOV probe as a non-target CFR.
        let chunk_path = chunk_dir.join(format!("{position:06}.nut"));
        let chunk_temporary = temporary_output_path(&chunk_path, "encode");
        let encode_args = ffmpeg_encode_args(request, &chunk_temporary, expected_frames);
        run_command(
            &assets.ffmpeg,
            &encode_args,
            events,
            CommandProgress {
                stage: "encoding",
                chunk_id: format!("chunk-{position}"),
                completed: (position + 1) as u64,
                total: chunks.len() as u64,
                started,
                deadline: MEDIA_PHASE_TIMEOUT,
                watch: None,
            },
        )?;
        if !is_regular_nonempty(&chunk_temporary) {
            return Err(Failure::upstream("chunk encoder produced no output"));
        }
        publish_no_replace(&chunk_temporary, &chunk_path)?;
        remove_directory(&request.work.input_frames)?;
        remove_directory(&request.work.output_frames)?;
        chunk_paths.push(chunk_path);
    }
    let concat_list = request.work.root.join("concat.txt");
    write_concat_list(
        &concat_list,
        &chunk_paths,
        &chunks,
        request.parameters.target_rate,
    )?;
    let joined_video = request.work.root.join("joined-video.nut");
    let joined_temporary = temporary_output_path(&joined_video, "concat");
    run_command(
        &assets.ffmpeg,
        &ffmpeg_concat_args(
            &concat_list,
            &joined_temporary,
            request.parameters.target_rate,
        ),
        events,
        CommandProgress {
            stage: "joining",
            chunk_id: "concat".into(),
            completed: chunks.len() as u64,
            total: chunks.len() as u64,
            started,
            deadline: MEDIA_PHASE_TIMEOUT,
            watch: None,
        },
    )?;
    if !is_regular_nonempty(&joined_temporary) {
        return Err(Failure::upstream("concat produced no joined video"));
    }
    publish_no_replace(&joined_temporary, &joined_video)?;
    let final_temporary = temporary_output_path(&request.output.path, "mux");
    run_command(
        &assets.ffmpeg,
        &ffmpeg_mux_args(request, &joined_video, &final_temporary),
        events,
        CommandProgress {
            stage: "muxing",
            chunk_id: "final".into(),
            completed: chunks.len() as u64,
            total: chunks.len() as u64,
            started,
            deadline: MEDIA_PHASE_TIMEOUT,
            watch: None,
        },
    )?;
    if !is_regular_nonempty(&final_temporary) {
        return Err(Failure::upstream("mux produced no final video"));
    }
    publish_no_replace(&final_temporary, &request.output.path)?;
    Ok(ExecutionStats {
        chunk_count: chunks.len(),
        scene_cut_count,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chunk {
    start_frame: u64,
    end_frame: u64,
    final_chunk: bool,
}

fn plan_chunks(frame_count: u64, chunk_intervals: u64) -> Result<Vec<Chunk>, Failure> {
    if frame_count < 2 || chunk_intervals == 0 {
        return Err(Failure::upstream("video must contain at least two frames"));
    }
    let last_frame = frame_count - 1;
    let mut start = 0;
    let mut chunks = Vec::new();
    while start < last_frame {
        let end = start.saturating_add(chunk_intervals).min(last_frame);
        chunks.push(Chunk {
            start_frame: start,
            end_frame: end,
            final_chunk: end == last_frame,
        });
        start = end;
    }
    Ok(chunks)
}

fn chunk_output_frames(chunk: Chunk) -> u64 {
    let intervals = chunk.end_frame - chunk.start_frame;
    intervals * 2 + u64::from(chunk.final_chunk) * 2
}

fn input_frame_count(chunk: Chunk) -> u64 {
    chunk.end_frame - chunk.start_frame + 1
}

fn ffmpeg_extract_args(request: &VideoInterpolateJobRequest, chunk: Chunk) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-n".into(),
        "-i".into(),
        request.input.path.as_os_str().to_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        format!(
            "select=between(n\\,{}\\,{})",
            chunk.start_frame, chunk.end_frame
        )
        .into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-start_number".into(),
        "0".into(),
        "-progress".into(),
        "pipe:1".into(),
        request.work.input_frames.join("%08d.png").into_os_string(),
    ]
}

fn rife_args(
    request: &VideoInterpolateJobRequest,
    assets: &Assets,
    input_count: u64,
) -> Vec<OsString> {
    let gpu = match request.parameters.device {
        VideoDevice::Vulkan { index } => index.to_string(),
        VideoDevice::NcnnCpu => "-1".into(),
    };
    vec![
        "-i".into(),
        request.work.input_frames.as_os_str().to_owned(),
        "-o".into(),
        request.work.output_frames.as_os_str().to_owned(),
        "-n".into(),
        input_count.saturating_mul(2).to_string().into(),
        "-m".into(),
        assets.models.as_os_str().to_owned(),
        "-g".into(),
        gpu.into(),
        "-j".into(),
        "1:2:2".into(),
        "-f".into(),
        "%08d.png".into(),
    ]
}

fn ffmpeg_encode_args(
    request: &VideoInterpolateJobRequest,
    output: &Path,
    frame_count: u64,
) -> Vec<OsString> {
    let rate = rate_text(request.parameters.target_rate);
    let gop = ((u64::from(request.parameters.target_rate.numerator) * 2)
        / u64::from(request.parameters.target_rate.denominator))
    .max(1);
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-n".into(),
        "-framerate".into(),
        rate.into(),
        "-start_number".into(),
        "1".into(),
        "-i".into(),
        request.work.output_frames.join("%08d.png").into_os_string(),
        "-frames:v".into(),
        frame_count.to_string().into(),
        "-an".into(),
        "-c:v".into(),
        "h264_videotoolbox".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-b:v".into(),
        "12000000".into(),
        "-maxrate".into(),
        "12000000".into(),
        "-bufsize".into(),
        "24000000".into(),
        "-g".into(),
        gop.to_string().into(),
        "-keyint_min".into(),
        gop.to_string().into(),
        "-r".into(),
        rate_text(request.parameters.target_rate).into(),
        "-fps_mode".into(),
        "cfr".into(),
        "-progress".into(),
        "pipe:1".into(),
        output.as_os_str().to_owned(),
    ]
}

fn ffmpeg_concat_args(list: &Path, output: &Path, target_rate: RationalRate) -> Vec<OsString> {
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-n".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list.as_os_str().to_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-c:v".into(),
        "copy".into(),
        "-r".into(),
        rate_text(target_rate).into(),
        "-fps_mode".into(),
        "cfr".into(),
        "-progress".into(),
        "pipe:1".into(),
        output.as_os_str().to_owned(),
    ]
}

fn ffmpeg_mux_args(
    request: &VideoInterpolateJobRequest,
    video: &Path,
    output: &Path,
) -> Vec<OsString> {
    let mut args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-n".into(),
        "-i".into(),
        video.as_os_str().to_owned(),
        "-i".into(),
        request.input.path.as_os_str().to_owned(),
    ];
    let mut subtitle_index = 0u32;
    for stream in &request.mux_plan.streams {
        match stream.action {
            MuxStreamAction::InterpolateVideo => {
                args.extend([OsString::from("-map"), OsString::from("0:v:0")]);
            }
            MuxStreamAction::Copy => {
                args.extend([
                    OsString::from("-map"),
                    OsString::from(format!("1:{}", stream.input_index)),
                ]);
                if stream.kind == MuxStreamKind::Subtitle {
                    args.extend([
                        OsString::from(format!("-c:s:{subtitle_index}")),
                        OsString::from("copy"),
                    ]);
                    subtitle_index += 1;
                }
            }
            MuxStreamAction::TranscodeMovText => {
                args.extend([
                    OsString::from("-map"),
                    OsString::from(format!("1:{}", stream.input_index)),
                    OsString::from(format!("-c:s:{subtitle_index}")),
                    OsString::from("mov_text"),
                ]);
                subtitle_index += 1;
            }
        }
    }
    args.extend([
        OsString::from("-c:v"),
        OsString::from("copy"),
        OsString::from("-r"),
        OsString::from(rate_text(request.parameters.target_rate)),
        OsString::from("-fps_mode"),
        OsString::from("cfr"),
    ]);
    if matches!(
        request.output.container,
        VideoContainer::Mp4 | VideoContainer::Mov
    ) {
        args.extend([
            OsString::from("-video_track_timescale"),
            OsString::from(request.parameters.target_rate.numerator.to_string()),
        ]);
    }
    if request
        .mux_plan
        .streams
        .iter()
        .any(|stream| stream.kind == MuxStreamKind::Audio)
    {
        args.extend([OsString::from("-c:a"), OsString::from("copy")]);
    }
    args.extend([
        OsString::from("-map_metadata"),
        OsString::from(if request.mux_plan.copy_metadata {
            "1"
        } else {
            "-1"
        }),
        OsString::from("-map_chapters"),
        OsString::from(if request.mux_plan.copy_chapters {
            "1"
        } else {
            "-1"
        }),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        output.as_os_str().to_owned(),
    ]);
    args
}

fn repair_rife_frames(
    input: &Path,
    output: &Path,
    input_count: u64,
    threshold_permille: u16,
) -> Result<u64, Failure> {
    let mut scene_cut_count = 0u64;
    for index in 0..input_count {
        let source = frame_path(input, index);
        let even = output_frame_path(output, index * 2);
        copy_replace(&source, &even)?;
        if index + 1 < input_count
            && scene_difference_permille(&source, &frame_path(input, index + 1))?
                >= u64::from(threshold_permille)
        {
            scene_cut_count += 1;
            copy_replace(
                &frame_path(input, index + 1),
                &output_frame_path(output, index * 2 + 1),
            )?;
        }
    }
    copy_replace(
        &frame_path(input, input_count - 1),
        &output_frame_path(output, input_count * 2 - 1),
    )?;
    Ok(scene_cut_count)
}

fn scene_difference_permille(first: &Path, second: &Path) -> Result<u64, Failure> {
    let first = image::open(first)
        .map_err(|error| Failure::upstream(error.to_string()))?
        .into_rgb8();
    let second = image::open(second)
        .map_err(|error| Failure::upstream(error.to_string()))?
        .into_rgb8();
    if first.dimensions() != second.dimensions() {
        return Err(Failure::upstream("adjacent frame dimensions differ"));
    }
    let total = first
        .as_raw()
        .iter()
        .zip(second.as_raw())
        .map(|(left, right)| u64::from(left.abs_diff(*right)))
        .sum::<u64>();
    let denominator = (first.as_raw().len() as u64).saturating_mul(255);
    Ok(total.saturating_mul(1_000) / denominator.max(1))
}

fn copy_replace(source: &Path, destination: &Path) -> Result<(), Failure> {
    let mut input = open_read_no_follow(source)
        .map_err(|error| Failure::upstream(format!("unsafe source frame: {error}")))?;
    atomic_replace_file(destination, |output| {
        io::copy(&mut input, output)?;
        Ok(())
    })
    .map_err(|error| Failure::upstream(error.to_string()))
}

fn frame_path(directory: &Path, index: u64) -> PathBuf {
    directory.join(format!("{index:08}.png"))
}

fn output_frame_path(directory: &Path, zero_based_index: u64) -> PathBuf {
    directory.join(format!("{:08}.png", zero_based_index + 1))
}

struct CommandProgress<'a> {
    stage: &'a str,
    chunk_id: String,
    completed: u64,
    total: u64,
    started: Instant,
    deadline: Duration,
    watch: Option<(PathBuf, u64)>,
}

fn run_command<W: Write>(
    program: &Path,
    arguments: &[OsString],
    events: &mut EventWriter<'_, W>,
    progress: CommandProgress<'_>,
) -> Result<(), Failure> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| Failure::upstream(format!("could not start upstream: {error}")))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child_group(&mut child);
            return Err(Failure::upstream("missing stdout"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child_group(&mut child);
            return Err(Failure::upstream("missing stderr"));
        }
    };
    let stdout_task = thread::spawn(move || io::copy(&mut BufReader::new(stdout), &mut io::sink()));
    let stderr_task = thread::spawn(move || read_bounded(stderr));
    let mut last_event = Instant::now();
    let phase_started = Instant::now();
    let status = loop {
        if CANCELLED.swap(false, Ordering::SeqCst) {
            terminate_child_group(&mut child);
            let _ = stdout_task.join();
            let _ = stderr_task.join();
            return Err(Failure::cancelled());
        }
        if phase_started.elapsed() >= progress.deadline {
            terminate_child_group(&mut child);
            let _ = stdout_task.join();
            let _ = stderr_task.join();
            return Err(Failure::upstream(format!(
                "{} phase exceeded its {:?} hard deadline",
                progress.stage, progress.deadline
            )));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_child_group(&mut child);
                let _ = stdout_task.join();
                let _ = stderr_task.join();
                return Err(Failure::upstream(error.to_string()));
            }
        }
        if last_event.elapsed() >= HEARTBEAT_INTERVAL {
            let elapsed = progress.started.elapsed();
            let (completed, total, unit, rate_unit) = progress_snapshot(&progress);
            let rate = (completed > 0 && elapsed.as_secs_f64() > 0.0)
                .then(|| completed as f64 / elapsed.as_secs_f64());
            let _ = events.progress(ProgressEvent {
                stage: progress.stage,
                completed,
                total,
                elapsed,
                chunk_id: &progress.chunk_id,
                rate,
                unit,
                rate_unit,
            });
            last_event = Instant::now();
        }
        thread::sleep(POLL_INTERVAL);
    };
    // A successful or failed parent may still have descendants holding pipes or files.
    // Always drain its dedicated process group before joining the reader threads.
    terminate_child_group(&mut child);
    let _ = stdout_task.join();
    let diagnostic = stderr_task.join().unwrap_or_default();
    if !status.success() {
        let detail = diagnostic
            .lines()
            .last()
            .unwrap_or("no upstream diagnostic");
        return Err(Failure::upstream(format!(
            "upstream exited with {status}: {detail}"
        )));
    }
    let elapsed = progress.started.elapsed();
    let (completed, total, unit, rate_unit) = progress_snapshot(&progress);
    let rate = (completed > 0 && elapsed.as_secs_f64() > 0.0)
        .then(|| completed as f64 / elapsed.as_secs_f64());
    let _ = events.progress(ProgressEvent {
        stage: progress.stage,
        completed,
        total,
        elapsed,
        chunk_id: &progress.chunk_id,
        rate,
        unit,
        rate_unit,
    });
    Ok(())
}

fn progress_snapshot(progress: &CommandProgress<'_>) -> (u64, u64, &'static str, &'static str) {
    if let Some((directory, expected)) = &progress.watch {
        (
            observed_file_count(directory).min(*expected),
            (*expected).max(1),
            "frame",
            "frame/s",
        )
    } else {
        (
            progress.completed,
            progress.total.max(1),
            "chunk",
            "chunk/s",
        )
    }
}

fn observed_file_count(directory: &Path) -> u64 {
    fs::read_dir(directory)
        .map(|entries| entries.filter_map(Result::ok).count() as u64)
        .unwrap_or(0)
}

fn read_bounded(mut reader: impl Read) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(unix)]
fn terminate_child_group(child: &mut Child) {
    let process_group = child.id() as libc::pid_t;
    unsafe {
        libc::kill(-process_group, libc::SIGTERM);
    }
    let deadline = Instant::now() + PROCESS_TERM_GRACE;
    while process_group_exists(process_group) && Instant::now() < deadline {
        let _ = child.try_wait();
        thread::sleep(POLL_INTERVAL);
    }
    if process_group_exists(process_group) {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
        let kill_deadline = Instant::now() + PROCESS_TERM_GRACE;
        while process_group_exists(process_group) && Instant::now() < kill_deadline {
            thread::sleep(POLL_INTERVAL);
        }
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn process_group_exists(process_group: libc::pid_t) -> bool {
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn terminate_child_group(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn validate_private_paths(
    request: &VideoInterpolateJobRequest,
    job_path: &Path,
) -> Result<(), String> {
    let root = &request.work.root;
    let workspace = job_path
        .parent()
        .ok_or_else(|| "job file has no workspace parent".to_string())?;
    if root != &workspace.join("work")
        || request.output.path.parent() != Some(root.as_path())
        || request.work.input_frames.parent() != Some(root.as_path())
        || request.work.output_frames.parent() != Some(root.as_path())
        || request.output.path == request.work.input_frames
        || request.output.path == request.work.output_frames
        || request.input.path.starts_with(root)
    {
        return Err("video work and destination paths are not safely separated".into());
    }
    let expected_extension = match request.output.container {
        VideoContainer::Mp4 => "mp4",
        VideoContainer::Mov => "mov",
        VideoContainer::Mkv => "mkv",
    };
    if request.output.path.extension() != Some(OsStr::new(expected_extension)) {
        return Err("private output extension does not match its container".into());
    }
    for stream in &request.mux_plan.streams {
        if stream.kind == MuxStreamKind::Subtitle {
            let valid = match request.output.container {
                VideoContainer::Mkv => stream.action == MuxStreamAction::Copy,
                VideoContainer::Mp4 | VideoContainer::Mov => {
                    stream.action == MuxStreamAction::TranscodeMovText
                }
            };
            if !valid {
                return Err("subtitle mux action does not match the output container".into());
            }
        }
    }
    for path in [
        root.clone(),
        request.work.input_frames.clone(),
        request.work.output_frames.clone(),
        request.work.root.join("chunks"),
    ] {
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            return Err("managed work path must not be a symlink".into());
        }
    }
    for path in [
        request.output.path.clone(),
        request.work.root.join("concat.txt"),
        request.work.root.join("joined-video.nut"),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(format!(
                    "managed internal output already exists: {}",
                    path.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn verify_assets(assets: &Assets) -> Result<(), String> {
    if [FFMPEG_HASH, FFPROBE_HASH, RIFE_ENGINE_HASH]
        .iter()
        .any(|hash| !is_sha256(hash))
        || RIFE_MODEL_FILES.iter().any(|(_, hash)| !is_sha256(hash))
    {
        return Err("runtime catalog contains an invalid SHA-256".into());
    }
    for (path, hash) in [
        (&assets.ffmpeg, FFMPEG_HASH),
        (&assets.ffprobe, FFPROBE_HASH),
        (&assets.engine, RIFE_ENGINE_HASH),
    ] {
        verify_executable(path, hash)?;
        verify_arm64_macho(path)?;
    }
    let model_metadata = fs::symlink_metadata(&assets.models).map_err(|error| error.to_string())?;
    if model_metadata.file_type().is_symlink() || !model_metadata.is_dir() {
        return Err("model path must be a regular directory".into());
    }
    for (name, hash) in RIFE_MODEL_FILES {
        verify_regular_hash(&assets.models.join(name), hash)?;
    }
    let allowed = RIFE_MODEL_FILES
        .iter()
        .map(|(name, _)| OsStr::new(name))
        .collect::<Vec<_>>();
    for entry in fs::read_dir(&assets.models).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if !allowed.contains(&entry.file_name().as_os_str()) {
            return Err(format!(
                "unexpected model asset: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn assets_installed(assets: &Assets) -> bool {
    assets.ffmpeg.exists()
        && assets.ffprobe.exists()
        && assets.engine.exists()
        && assets.models.exists()
}

fn verify_executable(path: &Path, hash: &str) -> Result<(), String> {
    verify_regular_hash(path, hash)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)
            .map_err(|error| error.to_string())?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(format!("{} is not executable", path.display()));
        }
    }
    Ok(())
}

fn verify_regular_hash(path: &Path, hash: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let actual = sha256_file(path)?;
    if actual != hash {
        return Err(format!("asset hash mismatch for {}", path.display()));
    }
    Ok(())
}

fn verify_arm64_macho(path: &Path) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let thin_arm64 = bytes.len() >= 8
        && ((bytes[..4] == [0xcf, 0xfa, 0xed, 0xfe]
            && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == 0x0100_000c)
            || (bytes[..4] == [0xfe, 0xed, 0xfa, 0xcf]
                && u32::from_be_bytes(bytes[4..8].try_into().unwrap()) == 0x0100_000c));
    let fat_arm64 = fat_contains_arm64(&bytes);
    if thin_arm64 || fat_arm64 {
        Ok(())
    } else {
        Err(format!("{} is not an arm64 Mach-O", path.display()))
    }
}

fn fat_contains_arm64(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let (big_endian, stride) = match bytes[..4] {
        [0xca, 0xfe, 0xba, 0xbe] => (true, 20usize),
        [0xbe, 0xba, 0xfe, 0xca] => (false, 20usize),
        [0xca, 0xfe, 0xba, 0xbf] => (true, 32usize),
        [0xbf, 0xba, 0xfe, 0xca] => (false, 32usize),
        _ => return false,
    };
    let read_u32 = |bytes: [u8; 4]| {
        if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };
    let count = read_u32(bytes[4..8].try_into().unwrap()) as usize;
    if count > 64 || 8usize.saturating_add(count.saturating_mul(stride)) > bytes.len() {
        return false;
    }
    (0..count).any(|index| {
        let offset = 8 + index * stride;
        read_u32(bytes[offset..offset + 4].try_into().unwrap()) == 0x0100_000c
    })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = open_read_no_follow(path).map_err(|error| error.to_string())?;
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

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn reset_directory(path: &Path) -> Result<(), Failure> {
    remove_directory(path)?;
    fs::create_dir(path).map_err(|error| Failure::upstream(error.to_string()))
}

fn remove_directory(path: &Path) -> Result<(), Failure> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(Failure::upstream("refusing to remove symlinked work path"))
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|error| Failure::upstream(error.to_string()))
        }
        Ok(_) => Err(Failure::upstream("work path is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Failure::upstream(error.to_string())),
    }
}

fn cleanup_job_outputs(request: &VideoInterpolateJobRequest) {
    let _ = fs::remove_file(&request.output.path);
    cleanup_work_root(&request.work.root);
}

fn cleanup_success_intermediates(request: &VideoInterpolateJobRequest) -> Result<(), String> {
    for directory in [
        request.work.input_frames.clone(),
        request.work.output_frames.clone(),
        request.work.root.join("chunks"),
    ] {
        remove_directory(&directory).map_err(|failure| failure.message)?;
    }
    for file in [
        request.work.root.join("concat.txt"),
        request.work.root.join("joined-video.nut"),
    ] {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn write_runner_evidence(
    request: &VideoInterpolateJobRequest,
    execution: &ExecutionStats,
) -> Result<(), String> {
    let evidence_path = request.work.root.join("runner-evidence.json");
    let temporary_path = request
        .work
        .root
        .join(format!(".runner-evidence.{}.tmp", request.job_id));
    let (backend, device) = match request.parameters.device {
        VideoDevice::Vulkan { index } => ("vulkan", format!("gpu:{index}")),
        VideoDevice::NcnnCpu => ("ncnn_cpu", "cpu".into()),
    };
    let evidence = serde_json::json!({
        "schema_version": 1,
        "job_id": request.job_id,
        "selected_backend": backend,
        "selected_device": device,
        "source_frames": request.parameters.frame_count,
        "output_frames": request.parameters.frame_count.saturating_mul(2),
        "chunk_count": execution.chunk_count,
        "scene_cut_count": execution.scene_cut_count,
        "ffmpeg_sha256": FFMPEG_HASH,
        "ffprobe_sha256": FFPROBE_HASH,
        "rife_sha256": RIFE_ENGINE_HASH,
        "model_sha256": {
            "flownet.param": RIFE_MODEL_FILES[0].1,
            "flownet.bin": RIFE_MODEL_FILES[1].1,
        }
    });
    let result = (|| {
        let mut file = open_new_no_follow(&temporary_path).map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(&mut file, &evidence).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::hard_link(&temporary_path, &evidence_path).map_err(|error| error.to_string())?;
        fs::remove_file(&temporary_path).map_err(|error| error.to_string())?;
        File::open(&request.work.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
        let _ = fs::remove_file(&evidence_path);
    }
    result
}

fn cleanup_work_root(path: &Path) {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        let _ = fs::remove_dir_all(path);
    }
}

fn write_concat_list(
    path: &Path,
    chunk_paths: &[PathBuf],
    chunks: &[Chunk],
    target_rate: RationalRate,
) -> Result<(), Failure> {
    if chunk_paths.len() != chunks.len() {
        return Err(Failure::upstream("concat chunk metadata mismatch"));
    }
    atomic_replace_file(path, |file| {
        for (chunk_path, chunk) in chunk_paths.iter().zip(chunks) {
            let escaped = chunk_path.to_string_lossy().replace('\'', "'\\''");
            let duration = chunk_output_frames(*chunk) as f64 * f64::from(target_rate.denominator)
                / f64::from(target_rate.numerator);
            writeln!(file, "file '{escaped}'")?;
            writeln!(file, "duration {duration:.12}")?;
        }
        Ok(())
    })
    .map_err(|error| Failure::upstream(error.to_string()))
}

fn temporary_output_path(path: &Path, phase: &str) -> PathBuf {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or("video");
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or("tmp");
    path.with_file_name(format!(
        ".{stem}.{phase}.{}.{sequence}.{extension}",
        std::process::id()
    ))
}

fn publish_no_replace(source: &Path, destination: &Path) -> Result<(), Failure> {
    if !is_regular_nonempty(source) {
        return Err(Failure::upstream(
            "refusing to publish a non-regular internal output",
        ));
    }
    fs::hard_link(source, destination).map_err(|error| {
        Failure::upstream(format!(
            "could not atomically publish internal output {}: {error}",
            destination.display()
        ))
    })?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(Failure::upstream(error.to_string()));
    }
    sync_parent(destination).map_err(|error| Failure::upstream(error.to_string()))
}

fn atomic_replace_file(
    destination: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = temporary_output_path(destination, "write");
    let result = (|| {
        let mut file = open_new_no_follow(&temporary)?;
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, destination)?;
        sync_parent(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn open_new_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("path is not a regular file"));
    }
    Ok(file)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    File::open(parent)?.sync_all()
}

fn rate_text(rate: RationalRate) -> String {
    format!("{}/{}", rate.numerator, rate.denominator)
}

fn is_regular_nonempty(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() > 0
    })
}

fn require_png_count(directory: &Path, expected: u64) -> Result<(), Failure> {
    let mut count = 0u64;
    for entry in fs::read_dir(directory).map_err(|error| Failure::upstream(error.to_string()))? {
        let entry = entry.map_err(|error| Failure::upstream(error.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| Failure::upstream(error.to_string()))?;
        if file_type.is_file() && entry.path().extension() == Some(OsStr::new("png")) {
            count += 1;
        } else {
            return Err(Failure::upstream(
                "frame directory contains an unexpected entry",
            ));
        }
    }
    if count == expected {
        Ok(())
    } else {
        Err(Failure::upstream(format!(
            "frame count mismatch: expected {expected}, got {count}"
        )))
    }
}

struct EventWriter<'a, W> {
    output: &'a mut W,
    job_id: &'a str,
    sequence: u64,
}

struct ProgressEvent<'a> {
    stage: &'a str,
    completed: u64,
    total: u64,
    elapsed: Duration,
    chunk_id: &'a str,
    rate: Option<f64>,
    unit: &'a str,
    rate_unit: &'a str,
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

    fn progress(&mut self, progress: ProgressEvent<'_>) -> io::Result<()> {
        self.emit(RunnerEventPayload::Progress {
            stage: progress.stage.into(),
            completed_units: progress.completed,
            total_units: progress.total,
            unit: progress.unit.into(),
            elapsed_ms: progress.elapsed.as_millis() as u64,
            chunk_id: Some(progress.chunk_id.into()),
            rate: progress.rate,
            rate_unit: progress.rate.map(|_| progress.rate_unit.into()),
            estimated_remaining_ms: progress.rate.and_then(|rate| {
                (rate > 0.0).then(|| {
                    (((progress.total.saturating_sub(progress.completed)) as f64 / rate) * 1_000.0)
                        as u64
                })
            }),
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
    use image::{Rgb, RgbImage};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn root() -> PathBuf {
        std::env::current_dir().expect("cwd")
    }

    #[test]
    fn cli_requires_all_absolute_assets_and_exact_action() {
        let base = root();
        let args = vec![
            "--ffmpeg".into(),
            base.join("ffmpeg").display().to_string(),
            "--ffprobe".into(),
            base.join("ffprobe").display().to_string(),
            "--engine".into(),
            base.join("rife").display().to_string(),
            "--models".into(),
            base.join("models").display().to_string(),
            "--capabilities".into(),
            "--json".into(),
        ];
        assert!(matches!(parse_cli(&args), Ok((_, Action::Capabilities))));
        let mut relative = args;
        relative[1] = "ffmpeg".into();
        assert!(parse_cli(&relative).is_err());
    }

    #[test]
    fn chunk_boundaries_overlap_once_and_have_exact_output_counts() {
        let chunks = plan_chunks(11, 4).expect("chunks");
        assert_eq!(
            chunks,
            vec![
                Chunk {
                    start_frame: 0,
                    end_frame: 4,
                    final_chunk: false
                },
                Chunk {
                    start_frame: 4,
                    end_frame: 8,
                    final_chunk: false
                },
                Chunk {
                    start_frame: 8,
                    end_frame: 10,
                    final_chunk: true
                },
            ]
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk_output_frames(*chunk))
                .collect::<Vec<_>>(),
            vec![8, 8, 6]
        );
    }

    #[test]
    fn scene_cut_replaces_midpoint_and_even_frames_preserve_sources() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let input = temporary.path().join("input");
        let output = temporary.path().join("output");
        fs::create_dir(&input).unwrap();
        fs::create_dir(&output).unwrap();
        for (index, color) in [[0, 0, 0], [255, 255, 255]].into_iter().enumerate() {
            RgbImage::from_pixel(2, 2, Rgb(color))
                .save(frame_path(&input, index as u64))
                .unwrap();
        }
        for index in 0..4 {
            RgbImage::from_pixel(2, 2, Rgb([127, 127, 127]))
                .save(output_frame_path(&output, index))
                .unwrap();
        }
        repair_rife_frames(&input, &output, 2, 500).expect("repair");
        assert_eq!(
            image::open(output_frame_path(&output, 0))
                .unwrap()
                .into_rgb8(),
            image::open(frame_path(&input, 0)).unwrap().into_rgb8()
        );
        assert_eq!(
            image::open(output_frame_path(&output, 1))
                .unwrap()
                .into_rgb8(),
            image::open(frame_path(&input, 1)).unwrap().into_rgb8()
        );
        assert_eq!(
            image::open(output_frame_path(&output, 2))
                .unwrap()
                .into_rgb8(),
            image::open(frame_path(&input, 1)).unwrap().into_rgb8()
        );
        assert_eq!(
            image::open(output_frame_path(&output, 3))
                .unwrap()
                .into_rgb8(),
            image::open(frame_path(&input, 1)).unwrap().into_rgb8()
        );
    }

    #[test]
    fn mux_arguments_follow_explicit_stream_actions() {
        let request = fixture_request();
        let args = ffmpeg_mux_args(
            &request,
            &root().join("video.mkv"),
            &root().join("output.mkv"),
        );
        let text = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(text.windows(2).any(|pair| pair == ["-map", "0:v:0"]));
        assert!(text.windows(2).any(|pair| pair == ["-map", "1:1"]));
        assert!(text.windows(2).any(|pair| pair == ["-map_metadata", "1"]));
        assert!(text.windows(2).any(|pair| pair == ["-map_chapters", "1"]));
        assert!(text.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(text.windows(2).any(|pair| pair == ["-r", "60/1"]));
        assert!(text.windows(2).any(|pair| pair == ["-fps_mode", "cfr"]));
        assert!(!text.iter().any(|arg| arg == "-video_track_timescale"));
    }

    #[test]
    fn mov_mux_pins_target_rate_and_track_timescale_without_reencoding() {
        let mut request = fixture_request();
        request.output.container = VideoContainer::Mov;
        request.parameters.target_rate = RationalRate {
            numerator: 60_000,
            denominator: 1_001,
        };
        let args = ffmpeg_mux_args(
            &request,
            &root().join("video.mkv"),
            &root().join("output.mov"),
        );
        let text = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(text.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(text.windows(2).any(|pair| pair == ["-r", "60000/1001"]));
        assert!(text.windows(2).any(|pair| pair == ["-fps_mode", "cfr"]));
        assert!(
            text.windows(2)
                .any(|pair| pair == ["-video_track_timescale", "60000"])
        );
    }

    #[test]
    fn regular_hash_and_symlink_checks_fail_closed() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let file = temporary.path().join("asset");
        fs::write(&file, b"asset").unwrap();
        let hash = sha256_file(&file).unwrap();
        verify_regular_hash(&file, &hash).expect("hash");
        assert!(verify_regular_hash(&file, &"0".repeat(64)).is_err());
        #[cfg(unix)]
        {
            let link = temporary.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(verify_regular_hash(&link, &hash).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn executable_hash_and_arm64_architecture_are_both_required() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let executable = temporary.path().join("engine");
        let mut macho = vec![0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];
        macho.extend_from_slice(&[0; 24]);
        fs::write(&executable, &macho).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let hash = sha256_file(&executable).unwrap();
        verify_executable(&executable, &hash).expect("executable");
        verify_arm64_macho(&executable).expect("arm64");
        fs::write(&executable, b"not macho").unwrap();
        assert!(verify_arm64_macho(&executable).is_err());
        let mut fake = vec![0u8; 32];
        fake[12..16].copy_from_slice(&0x0100_000cu32.to_be_bytes());
        fs::write(&executable, fake).unwrap();
        assert!(verify_arm64_macho(&executable).is_err());
    }

    #[test]
    fn input_hash_recheck_detects_mutation() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let input = temporary.path().join("input.mov");
        fs::write(&input, b"before").unwrap();
        let before = sha256_file(&input).unwrap();
        fs::write(&input, b"after").unwrap();
        assert_ne!(sha256_file(&input).unwrap(), before);
    }

    #[test]
    fn runner_evidence_is_atomic_and_records_execution_hashes() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let mut request = fixture_request();
        request.job_id = "evidence-job".into();
        request.work.root = temporary.path().join("work");
        request.work.input_frames = request.work.root.join("input-frames");
        request.work.output_frames = request.work.root.join("output-frames");
        request.output.path = request.work.root.join("interpolated.mkv");
        fs::create_dir(&request.work.root).unwrap();
        write_runner_evidence(
            &request,
            &ExecutionStats {
                chunk_count: 3,
                scene_cut_count: 2,
            },
        )
        .expect("evidence");
        let evidence: serde_json::Value = serde_json::from_slice(
            &fs::read(request.work.root.join("runner-evidence.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["schema_version"], 1);
        assert_eq!(evidence["chunk_count"], 3);
        assert_eq!(evidence["scene_cut_count"], 2);
        assert_eq!(evidence["rife_sha256"], RIFE_ENGINE_HASH);
        assert!(
            !request
                .work
                .root
                .join(".runner-evidence.evidence-job.tmp")
                .exists()
        );
    }

    #[test]
    fn successful_cleanup_preserves_private_output_for_core_publish() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let workspace = temporary.path().join("job");
        let mut request = fixture_request();
        request.work.root = workspace.join("work");
        request.work.input_frames = request.work.root.join("input-frames");
        request.work.output_frames = request.work.root.join("output-frames");
        request.output.path = request.work.root.join("interpolated.mkv");
        fs::create_dir_all(&request.work.input_frames).unwrap();
        fs::create_dir(&request.work.output_frames).unwrap();
        fs::create_dir(request.work.root.join("chunks")).unwrap();
        for file in [
            request.output.path.clone(),
            request.work.root.join("concat.txt"),
            request.work.root.join("joined-video.nut"),
        ] {
            fs::write(file, b"owned").unwrap();
        }
        cleanup_success_intermediates(&request).expect("cleanup");
        assert!(request.output.path.exists());
        assert!(!request.work.input_frames.exists());
        assert!(!request.work.output_frames.exists());
        assert!(!request.work.root.join("chunks").exists());
    }

    #[test]
    #[cfg(unix)]
    fn managed_internal_files_replace_symlinks_without_touching_targets() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let victim = temporary.path().join("victim");
        fs::write(&victim, b"keep").unwrap();

        let concat = temporary.path().join("concat.txt");
        std::os::unix::fs::symlink(&victim, &concat).unwrap();
        let chunk = temporary.path().join("chunk.mkv");
        write_concat_list(
            &concat,
            std::slice::from_ref(&chunk),
            &[Chunk {
                start_frame: 0,
                end_frame: 1,
                final_chunk: true,
            }],
            RationalRate {
                numerator: 60,
                denominator: 1,
            },
        )
        .expect("atomic concat");
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&concat).unwrap().is_file());
        assert!(fs::read_to_string(&concat).unwrap().contains("chunk.mkv"));
        assert!(
            fs::read_to_string(&concat)
                .unwrap()
                .contains("duration 0.066666666667")
        );

        let source = temporary.path().join("source.png");
        fs::write(&source, b"frame").unwrap();
        let frame = temporary.path().join("frame.png");
        std::os::unix::fs::symlink(&victim, &frame).unwrap();
        copy_replace(&source, &frame).expect("atomic frame replacement");
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(&frame).unwrap(), b"frame");
        assert!(
            !fs::symlink_metadata(&frame)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn internal_publish_never_replaces_an_existing_symlink() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let source = temporary.path().join("source.mkv");
        let victim = temporary.path().join("victim");
        let destination = temporary.path().join("joined.mkv");
        fs::write(&source, b"video").unwrap();
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, &destination).unwrap();
        assert!(publish_no_replace(&source, &destination).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(&source).unwrap(), b"video");
    }

    #[test]
    #[cfg(unix)]
    fn quiet_upstream_emits_heartbeat_and_failure_is_structured() {
        let mut output = Vec::new();
        let mut events = EventWriter::new(&mut output, "job-heartbeat");
        run_command(
            Path::new("/bin/sleep"),
            &[OsString::from("3")],
            &mut events,
            CommandProgress {
                stage: "interpolating",
                chunk_id: "chunk-0".into(),
                completed: 0,
                total: 1,
                started: Instant::now(),
                deadline: Duration::from_secs(10),
                watch: None,
            },
        )
        .expect("sleep must succeed");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("chunk-0"));

        let mut output = Vec::new();
        let mut events = EventWriter::new(&mut output, "job-failure");
        let failure = run_command(
            Path::new("/usr/bin/false"),
            &[],
            &mut events,
            CommandProgress {
                stage: "encoding",
                chunk_id: "chunk-0".into(),
                completed: 0,
                total: 1,
                started: Instant::now(),
                deadline: Duration::from_secs(10),
                watch: None,
            },
        )
        .expect_err("false must fail");
        assert_eq!(failure.code, "UPSTREAM_FAILED");
    }

    #[test]
    #[cfg(unix)]
    fn phase_deadline_terminates_the_upstream_group() {
        let mut output = Vec::new();
        let mut events = EventWriter::new(&mut output, "job-deadline");
        let started = Instant::now();
        let failure = run_command(
            Path::new("/bin/sleep"),
            &[OsString::from("30")],
            &mut events,
            CommandProgress {
                stage: "interpolating",
                chunk_id: "chunk-0".into(),
                completed: 0,
                total: 1,
                started,
                deadline: Duration::from_millis(100),
                watch: None,
            },
        )
        .expect_err("deadline must fail");
        assert!(failure.message.contains("hard deadline"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[cfg(unix)]
    fn normal_and_failed_upstreams_leave_no_term_resistant_descendants() {
        for exit in [0, 7] {
            let temporary = tempfile::tempdir().expect("tempdir");
            let pid_file = temporary.path().join("descendant.pid");
            let script = format!(
                "(trap '' TERM; sleep 30) & echo $! > '{}'; exit {exit}",
                pid_file.display()
            );
            let mut output = Vec::new();
            let mut events = EventWriter::new(&mut output, "job-tree");
            let result = run_command(
                Path::new("/bin/sh"),
                &[OsString::from("-c"), OsString::from(script)],
                &mut events,
                CommandProgress {
                    stage: "interpolating",
                    chunk_id: "chunk-0".into(),
                    completed: 0,
                    total: 1,
                    started: Instant::now(),
                    deadline: Duration::from_secs(5),
                    watch: None,
                },
            );
            assert_eq!(result.is_ok(), exit == 0);
            let descendant: libc::pid_t = fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let gone = (0..40).any(|_| {
                if unsafe { libc::kill(descendant, 0) } != 0 {
                    true
                } else {
                    thread::sleep(Duration::from_millis(25));
                    false
                }
            });
            assert!(gone, "descendant {descendant} survived exit {exit}");
        }
    }

    #[test]
    fn exact_upstream_arguments_never_use_a_shell_or_path_lookup() {
        let request = fixture_request();
        let assets = Assets {
            ffmpeg: root().join("bin/ffmpeg"),
            ffprobe: root().join("bin/ffprobe"),
            engine: root().join("bin/rife-ncnn-vulkan"),
            models: root().join("models/rife-v4.6"),
        };
        let chunks = plan_chunks(
            request.parameters.frame_count,
            u64::from(request.parameters.chunk_frames),
        )
        .unwrap();
        let extract = ffmpeg_extract_args(&request, chunks[0]);
        let rife = rife_args(&request, &assets, 121);
        let encode = ffmpeg_encode_args(&request, &root().join("chunk.mkv"), 240);
        assert!(
            extract
                .iter()
                .any(|arg| arg == OsStr::new("select=between(n\\,0\\,120)"))
        );
        assert!(
            rife.windows(2)
                .any(|pair| pair == [OsStr::new("-g"), OsStr::new("0")])
        );
        assert!(
            rife.windows(2)
                .any(|pair| pair == [OsStr::new("-n"), OsStr::new("242")])
        );
        assert!(
            encode
                .windows(2)
                .any(|pair| pair == [OsStr::new("-start_number"), OsStr::new("1")])
        );
        assert!(
            encode
                .windows(2)
                .any(|pair| pair == [OsStr::new("-r"), OsStr::new("60/1")])
        );
        assert!(
            encode
                .windows(2)
                .any(|pair| pair == [OsStr::new("-fps_mode"), OsStr::new("cfr")])
        );
        for forbidden in ["sh", "bash", "zsh", "cmd.exe", "powershell"] {
            assert_ne!(assets.engine.file_name().unwrap(), OsStr::new(forbidden));
        }
    }

    fn fixture_request() -> VideoInterpolateJobRequest {
        use zoos_runner_protocol::{
            MuxPlan, MuxStreamPlan, RifeModel, VIDEO_PROTOCOL_VERSION, VideoInterpolateParameters,
            VideoRunnerInput, VideoRunnerOutput, VideoWorkPaths,
        };
        let root = root();
        VideoInterpolateJobRequest {
            protocol_version: VIDEO_PROTOCOL_VERSION,
            job_id: "video-job".into(),
            task: RunnerTask::VideoInterpolate,
            input: VideoRunnerInput {
                path: root.join("input.mkv"),
                sha256: "a".repeat(64),
                width: 64,
                height: 48,
                container: VideoContainer::Mkv,
            },
            output: VideoRunnerOutput {
                path: root.join("output.partial.mkv"),
                container: VideoContainer::Mkv,
            },
            work: VideoWorkPaths {
                root: root.join("work"),
                input_frames: root.join("work/input"),
                output_frames: root.join("work/output"),
            },
            parameters: VideoInterpolateParameters {
                source_rate: RationalRate {
                    numerator: 30,
                    denominator: 1,
                },
                target_rate: RationalRate {
                    numerator: 60,
                    denominator: 1,
                },
                frame_count: 301,
                chunk_frames: 120,
                scene_threshold_permille: 350,
                model: RifeModel::RifeV46,
                device: VideoDevice::Vulkan { index: 0 },
            },
            mux_plan: MuxPlan {
                streams: vec![
                    MuxStreamPlan {
                        input_index: 0,
                        kind: MuxStreamKind::Video,
                        action: MuxStreamAction::InterpolateVideo,
                    },
                    MuxStreamPlan {
                        input_index: 1,
                        kind: MuxStreamKind::Audio,
                        action: MuxStreamAction::Copy,
                    },
                ],
                copy_metadata: true,
                copy_chapters: true,
            },
        }
    }
}
