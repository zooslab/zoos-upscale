use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zoos_runner_protocol::{
    MuxPlan, MuxStreamAction, MuxStreamKind, MuxStreamPlan, RationalRate, VideoContainer,
};

pub const MAX_FFPROBE_STDOUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FFPROBE_STDERR_BYTES: usize = 64 * 1024;
pub const MAX_VIDEO_SIDE: u32 = 8_192;
pub const MAX_VIDEO_PIXELS: u64 = 33_177_600;
pub const MAX_VIDEO_FRAMES: u64 = 1_000_000;
pub const MAX_INTERPOLATED_VIDEO_FRAMES: u64 = MAX_VIDEO_FRAMES * 2;
pub const MAX_VIDEO_DURATION_MS: u64 = 6 * 60 * 60 * 1_000;

const FFPROBE_ARGUMENTS: [&str; 8] = [
    "-v",
    "error",
    "-print_format",
    "json",
    "-show_format",
    "-show_streams",
    "-show_chapters",
    "-count_frames",
];

#[derive(Debug, Clone)]
pub struct Ffprobe {
    executable: PathBuf,
    timeout: Duration,
    termination_grace: Duration,
}

impl Ffprobe {
    pub fn new(
        executable: impl Into<PathBuf>,
        timeout_duration: Duration,
        termination_grace: Duration,
    ) -> Result<Self, MediaError> {
        let executable = executable.into();
        if !executable.is_absolute() || timeout_duration.is_zero() || termination_grace.is_zero() {
            return Err(MediaError::InvalidConfiguration);
        }
        Ok(Self {
            executable,
            timeout: timeout_duration,
            termination_grace,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn probe(&self, input: &Path) -> Result<MediaDescriptor, MediaError> {
        self.probe_with_mode(input, ProbeMode::Source).await
    }

    pub async fn probe_output(&self, output: &Path) -> Result<MediaDescriptor, MediaError> {
        self.probe_with_mode(output, ProbeMode::InterpolatedOutput)
            .await
    }

    async fn probe_with_mode(
        &self,
        input: &Path,
        mode: ProbeMode,
    ) -> Result<MediaDescriptor, MediaError> {
        validate_input_path(input)?;

        let mut command = Command::new(&self.executable);
        command
            .args(FFPROBE_ARGUMENTS)
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| MediaError::Spawn(error.to_string()))?;
        let child_id = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MediaError::Spawn("ffprobe stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MediaError::Spawn("ffprobe stderr was not piped".into()))?;
        let stdout_task = tokio::spawn(read_bounded(stdout, MAX_FFPROBE_STDOUT_BYTES));
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_FFPROBE_STDERR_BYTES));
        let started = Instant::now();

        let status = match timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                abort_readers(stdout_task, stderr_task);
                return Err(MediaError::Spawn(error.to_string()));
            }
            Err(_) => {
                terminate_process_tree(&mut child, child_id, self.termination_grace).await;
                abort_readers(stdout_task, stderr_task);
                return Err(MediaError::TimedOut);
            }
        };

        let remaining = self.timeout.saturating_sub(started.elapsed());
        let outputs = timeout(remaining, async {
            let (stdout, stderr) = tokio::join!(stdout_task, stderr_task);
            Ok::<_, MediaError>((
                collect_reader_result(stdout, StreamName::Stdout)?,
                collect_reader_result(stderr, StreamName::Stderr)?,
            ))
        })
        .await;
        let (stdout, stderr) = match outputs {
            Ok(Ok(outputs)) => outputs,
            Ok(Err(error)) => {
                terminate_remaining_process_group(child_id, self.termination_grace).await;
                return Err(error);
            }
            Err(_) => {
                terminate_remaining_process_group(child_id, self.termination_grace).await;
                return Err(MediaError::TimedOut);
            }
        };
        if !status.success() {
            return Err(MediaError::ProbeFailed {
                exit_code: status.code(),
                message: String::from_utf8_lossy(&stderr).trim().to_owned(),
            });
        }
        parse_descriptor(input, &stdout, mode)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDescriptor {
    pub input_path: PathBuf,
    pub container: VideoContainer,
    pub format_name: String,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    pub frame_rate: RationalRate,
    pub video_stream_index: u32,
    pub streams: Vec<MediaStreamDescriptor>,
    pub chapters: Vec<MediaChapterDescriptor>,
}

impl MediaDescriptor {
    pub fn mux_plan(&self) -> MuxPlan {
        MuxPlan {
            streams: self
                .streams
                .iter()
                .map(|stream| MuxStreamPlan {
                    input_index: stream.input_index,
                    kind: stream.kind,
                    action: match stream.kind {
                        MuxStreamKind::Video => MuxStreamAction::InterpolateVideo,
                        MuxStreamKind::Audio => MuxStreamAction::Copy,
                        MuxStreamKind::Subtitle => match self.container {
                            VideoContainer::Mkv => MuxStreamAction::Copy,
                            VideoContainer::Mp4 | VideoContainer::Mov => {
                                MuxStreamAction::TranscodeMovText
                            }
                        },
                    },
                })
                .collect(),
            copy_metadata: true,
            copy_chapters: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStreamDescriptor {
    pub input_index: u32,
    pub kind: MuxStreamKind,
    pub codec_name: String,
    pub duration_ms: u64,
    pub duration_from_format: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaChapterDescriptor {
    pub id: i64,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    streams: Vec<RawStream>,
    #[serde(default)]
    chapters: Vec<RawChapter>,
    format: RawFormat,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    format_name: String,
    duration: String,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    index: u32,
    codec_name: String,
    codec_type: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    pix_fmt: Option<String>,
    #[serde(default)]
    field_order: Option<String>,
    #[serde(default)]
    r_frame_rate: Option<String>,
    #[serde(default)]
    avg_frame_rate: Option<String>,
    #[serde(default)]
    nb_read_frames: Option<String>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    color_primaries: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChapter {
    id: i64,
    start_time: String,
    end_time: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    Source,
    InterpolatedOutput,
}

fn parse_descriptor(
    input: &Path,
    bytes: &[u8],
    mode: ProbeMode,
) -> Result<MediaDescriptor, MediaError> {
    let raw: RawProbe =
        serde_json::from_slice(bytes).map_err(|error| MediaError::Malformed(error.to_string()))?;
    let container = validate_container(input, &raw.format.format_name)?;
    let duration_ms = parse_duration_ms(&raw.format.duration, "format.duration")?;
    if duration_ms == 0 || duration_ms > MAX_VIDEO_DURATION_MS {
        return Err(MediaError::LimitExceeded("duration"));
    }

    if raw.streams.is_empty() {
        return Err(MediaError::Unsupported("media has no streams"));
    }
    let mut indices = HashSet::new();
    let mut video = None;
    let mut streams = Vec::with_capacity(raw.streams.len());
    for stream in raw.streams {
        if !indices.insert(stream.index) {
            return Err(MediaError::Malformed("duplicate stream index".into()));
        }
        let kind = match stream.codec_type.as_str() {
            "video" => {
                if video.is_some() {
                    return Err(MediaError::Unsupported("multiple video streams"));
                }
                validate_video_stream(&stream, mode)?;
                video = Some(VideoFacts::try_from(&stream)?);
                MuxStreamKind::Video
            }
            "audio" => MuxStreamKind::Audio,
            "subtitle" if is_text_subtitle(&stream.codec_name) => MuxStreamKind::Subtitle,
            "subtitle" => return Err(MediaError::Unsupported("bitmap subtitle stream")),
            "attachment" | "data" => {
                return Err(MediaError::Unsupported("data or attachment stream"));
            }
            _ => return Err(MediaError::Unsupported("unknown stream type")),
        };
        let (stream_duration_ms, duration_from_format) = match stream.duration.as_deref() {
            Some(duration) => (parse_duration_ms(duration, "stream.duration")?, false),
            None => (duration_ms, true),
        };
        streams.push(MediaStreamDescriptor {
            input_index: stream.index,
            kind,
            codec_name: stream.codec_name,
            duration_ms: stream_duration_ms,
            duration_from_format,
        });
    }
    streams.sort_by_key(|stream| stream.input_index);
    let video = video.ok_or(MediaError::Unsupported("media has no video stream"))?;

    let mut chapters = Vec::with_capacity(raw.chapters.len());
    for chapter in raw.chapters {
        let start_ms = parse_duration_ms(&chapter.start_time, "chapter.start_time")?;
        let end_ms = parse_duration_ms(&chapter.end_time, "chapter.end_time")?;
        if end_ms <= start_ms || end_ms > duration_ms.saturating_add(1_000) {
            return Err(MediaError::Malformed("invalid chapter range".into()));
        }
        chapters.push(MediaChapterDescriptor {
            id: chapter.id,
            start_ms,
            end_ms,
        });
    }

    Ok(MediaDescriptor {
        input_path: input.to_owned(),
        container,
        format_name: raw.format.format_name,
        duration_ms,
        width: video.width,
        height: video.height,
        frame_count: video.frame_count,
        frame_rate: video.frame_rate,
        video_stream_index: video.index,
        streams,
        chapters,
    })
}

struct VideoFacts {
    index: u32,
    width: u32,
    height: u32,
    frame_count: u64,
    frame_rate: RationalRate,
}

impl TryFrom<&RawStream> for VideoFacts {
    type Error = MediaError;

    fn try_from(stream: &RawStream) -> Result<Self, Self::Error> {
        let width = stream
            .width
            .ok_or_else(|| MediaError::Malformed("video width is missing".into()))?;
        let height = stream
            .height
            .ok_or_else(|| MediaError::Malformed("video height is missing".into()))?;
        let frame_count = stream
            .nb_read_frames
            .as_deref()
            .ok_or_else(|| MediaError::Malformed("video frame count is missing".into()))?
            .parse::<u64>()
            .map_err(|_| MediaError::Malformed("invalid video frame count".into()))?;
        let frame_rate = parse_rate(
            stream
                .avg_frame_rate
                .as_deref()
                .ok_or_else(|| MediaError::Malformed("average frame rate is missing".into()))?,
        )?;
        Ok(Self {
            index: stream.index,
            width,
            height,
            frame_count,
            frame_rate,
        })
    }
}

fn validate_video_stream(stream: &RawStream, mode: ProbeMode) -> Result<(), MediaError> {
    let width = stream
        .width
        .ok_or_else(|| MediaError::Malformed("video width is missing".into()))?;
    let height = stream
        .height
        .ok_or_else(|| MediaError::Malformed("video height is missing".into()))?;
    if width == 0
        || height == 0
        || width > MAX_VIDEO_SIDE
        || height > MAX_VIDEO_SIDE
        || u64::from(width) * u64::from(height) > MAX_VIDEO_PIXELS
    {
        return Err(MediaError::LimitExceeded("dimensions"));
    }
    let frame_count = stream
        .nb_read_frames
        .as_deref()
        .ok_or_else(|| MediaError::Malformed("video frame count is missing".into()))?
        .parse::<u64>()
        .map_err(|_| MediaError::Malformed("invalid video frame count".into()))?;
    let frame_limit = match mode {
        ProbeMode::Source => MAX_VIDEO_FRAMES,
        ProbeMode::InterpolatedOutput => MAX_INTERPOLATED_VIDEO_FRAMES,
    };
    if frame_count == 0 || frame_count > frame_limit {
        return Err(MediaError::LimitExceeded("frame_count"));
    }
    if stream.field_order.as_deref() != Some("progressive") {
        return Err(MediaError::Unsupported("interlaced or unknown field order"));
    }
    if stream.pix_fmt.as_deref().is_none_or(str::is_empty) {
        return Err(MediaError::Malformed("pixel format is missing".into()));
    }
    if is_hdr(stream) {
        return Err(MediaError::Unsupported("HDR video"));
    }
    let average = parse_rate(
        stream
            .avg_frame_rate
            .as_deref()
            .ok_or_else(|| MediaError::Malformed("average frame rate is missing".into()))?,
    )?;
    let real = parse_rate(
        stream
            .r_frame_rate
            .as_deref()
            .ok_or_else(|| MediaError::Malformed("real frame rate is missing".into()))?,
    )?;
    if average != real {
        return Err(MediaError::Unsupported("variable frame rate"));
    }
    let supported = match mode {
        ProbeMode::Source => matches!(
            (average.numerator, average.denominator),
            (25, 1) | (30, 1) | (30_000, 1_001)
        ),
        ProbeMode::InterpolatedOutput => matches!(
            (average.numerator, average.denominator),
            (50, 1) | (60, 1) | (60_000, 1_001)
        ),
    };
    if !supported {
        return Err(MediaError::Unsupported("unsupported frame rate"));
    }
    Ok(())
}

pub fn verify_interpolated_output(
    source: &MediaDescriptor,
    output: &MediaDescriptor,
    plan: &MuxPlan,
) -> Result<(), MediaError> {
    if plan != &source.mux_plan() {
        return Err(MediaError::Verification(
            "MuxPlan does not exactly match the source streams",
        ));
    }
    if source.container != output.container {
        return Err(MediaError::Verification("container changed"));
    }
    if (source.width, source.height) != (output.width, output.height) {
        return Err(MediaError::Verification("dimensions changed"));
    }
    let target_numerator = u64::from(source.frame_rate.numerator)
        .checked_mul(2)
        .ok_or(MediaError::Verification("target frame rate overflow"))?;
    if u64::from(output.frame_rate.numerator) * u64::from(source.frame_rate.denominator)
        != target_numerator * u64::from(output.frame_rate.denominator)
    {
        return Err(MediaError::Verification(
            "frame rate is not exactly doubled",
        ));
    }
    if output.frame_count != source.frame_count.saturating_mul(2) {
        return Err(MediaError::Verification(
            "frame count is not exactly doubled",
        ));
    }
    let frame_tolerance_ms = frame_duration_ceiling_ms(output.frame_rate);
    if source.duration_ms.abs_diff(output.duration_ms) > frame_tolerance_ms {
        return Err(MediaError::Verification(
            "duration changed by more than one frame",
        ));
    }
    if source.chapters.len() != output.chapters.len() {
        return Err(MediaError::Verification("chapter count changed"));
    }

    let mut planned = plan.streams.clone();
    planned.sort_by_key(|stream| stream.input_index);
    if planned.len() != source.streams.len()
        || planned
            .iter()
            .zip(&source.streams)
            .any(|(planned, stream)| {
                planned.input_index != stream.input_index || planned.kind != stream.kind
            })
    {
        return Err(MediaError::Verification(
            "MuxPlan does not cover every source stream",
        ));
    }
    let source_kinds = stream_kind_counts(&source.streams);
    let output_kinds = stream_kind_counts(&output.streams);
    if source_kinds != output_kinds {
        return Err(MediaError::Verification("stream kind counts changed"));
    }

    let source_non_video = source
        .streams
        .iter()
        .filter(|stream| stream.kind != MuxStreamKind::Video);
    let output_non_video = output
        .streams
        .iter()
        .filter(|stream| stream.kind != MuxStreamKind::Video);
    for (source_stream, output_stream) in source_non_video.zip(output_non_video) {
        if source_stream.kind != output_stream.kind {
            return Err(MediaError::Verification("stream ordering changed"));
        }
        let action = plan
            .streams
            .iter()
            .find(|stream| stream.input_index == source_stream.input_index)
            .map(|stream| stream.action)
            .ok_or(MediaError::Verification("stream is missing from MuxPlan"))?;
        match action {
            MuxStreamAction::Copy if source_stream.codec_name != output_stream.codec_name => {
                return Err(MediaError::Verification("copied stream codec changed"));
            }
            MuxStreamAction::TranscodeMovText
                if output_stream.codec_name != "mov_text"
                    || output_stream.kind != MuxStreamKind::Subtitle =>
            {
                return Err(MediaError::Verification(
                    "subtitle was not transcoded to mov_text",
                ));
            }
            MuxStreamAction::Copy | MuxStreamAction::TranscodeMovText => {}
            MuxStreamAction::InterpolateVideo => {
                return Err(MediaError::Verification("invalid non-video MuxPlan action"));
            }
        }
    }

    let output_video_duration = output
        .streams
        .iter()
        .find(|stream| stream.kind == MuxStreamKind::Video)
        .map(|stream| stream.duration_ms)
        .ok_or(MediaError::Verification("output video stream is missing"))?;
    if output.streams.iter().any(|stream| {
        stream.kind == MuxStreamKind::Audio
            && stream.duration_ms.abs_diff(output_video_duration) > frame_tolerance_ms
    }) {
        return Err(MediaError::Verification("audio and video duration differ"));
    }
    Ok(())
}

fn stream_kind_counts(streams: &[MediaStreamDescriptor]) -> [usize; 3] {
    let mut counts = [0; 3];
    for stream in streams {
        counts[match stream.kind {
            MuxStreamKind::Video => 0,
            MuxStreamKind::Audio => 1,
            MuxStreamKind::Subtitle => 2,
        }] += 1;
    }
    counts
}

fn frame_duration_ceiling_ms(rate: RationalRate) -> u64 {
    (1_000_u64 * u64::from(rate.denominator)).div_ceil(u64::from(rate.numerator))
}

fn is_hdr(stream: &RawStream) -> bool {
    stream.color_transfer.as_deref().is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "smpte2084" | "arib-std-b67" | "hlg" | "pq"
        )
    }) || stream
        .color_primaries
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("bt2020"))
}

fn parse_rate(value: &str) -> Result<RationalRate, MediaError> {
    let (numerator, denominator) = value
        .split_once('/')
        .ok_or_else(|| MediaError::Malformed("frame rate is not rational".into()))?;
    let numerator = numerator
        .parse::<u32>()
        .map_err(|_| MediaError::Malformed("invalid frame rate numerator".into()))?;
    let denominator = denominator
        .parse::<u32>()
        .map_err(|_| MediaError::Malformed("invalid frame rate denominator".into()))?;
    if numerator == 0 || denominator == 0 || greatest_common_divisor(numerator, denominator) != 1 {
        return Err(MediaError::Malformed(
            "frame rate must be non-zero and reduced".into(),
        ));
    }
    Ok(RationalRate {
        numerator,
        denominator,
    })
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn parse_duration_ms(value: &str, field: &'static str) -> Result<u64, MediaError> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| MediaError::Malformed(format!("invalid {field}")))?;
    if !seconds.is_finite() || seconds < 0.0 || seconds > (u64::MAX / 1_000) as f64 {
        return Err(MediaError::Malformed(format!("invalid {field}")));
    }
    Ok((seconds * 1_000.0).round() as u64)
}

fn validate_container(input: &Path, format_name: &str) -> Result<VideoContainer, MediaError> {
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or(MediaError::Unsupported("missing container extension"))?;
    let names = format_name.split(',').collect::<HashSet<_>>();
    match extension.as_str() {
        "mp4" if names.contains("mp4") || names.contains("mov") => Ok(VideoContainer::Mp4),
        "mov" if names.contains("mov") => Ok(VideoContainer::Mov),
        "mkv" if names.contains("matroska") => Ok(VideoContainer::Mkv),
        "mp4" | "mov" | "mkv" => Err(MediaError::Malformed(
            "container signature does not match the extension".into(),
        )),
        _ => Err(MediaError::Unsupported("unsupported container")),
    }
}

fn is_text_subtitle(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "ass" | "ssa" | "subrip" | "text" | "webvtt" | "mov_text"
    )
}

fn validate_input_path(input: &Path) -> Result<(), MediaError> {
    if !input.is_absolute() {
        return Err(MediaError::InvalidInput);
    }
    let metadata = std::fs::symlink_metadata(input).map_err(|_| MediaError::InvalidInput)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MediaError::InvalidInput);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StreamName {
    Stdout,
    Stderr,
}

async fn read_bounded(
    mut stream: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, io::Error> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(read) > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "ffprobe stream exceeded its byte limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

fn collect_reader_result(
    result: Result<Result<Vec<u8>, io::Error>, tokio::task::JoinError>,
    stream: StreamName,
) -> Result<Vec<u8>, MediaError> {
    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) if error.kind() == io::ErrorKind::FileTooLarge => match stream {
            StreamName::Stdout => Err(MediaError::StdoutTooLarge),
            StreamName::Stderr => Err(MediaError::StderrTooLarge),
        },
        Ok(Err(error)) => Err(MediaError::Spawn(error.to_string())),
        Err(error) => Err(MediaError::Spawn(error.to_string())),
    }
}

fn abort_readers(
    stdout: JoinHandle<Result<Vec<u8>, io::Error>>,
    stderr: JoinHandle<Result<Vec<u8>, io::Error>>,
) {
    stdout.abort();
    stderr.abort();
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_process_tree(child: &mut Child, child_id: Option<u32>, grace: Duration) {
    if let Some(id) = child_id.and_then(|value| i32::try_from(value).ok()) {
        unsafe {
            libc::kill(-id, libc::SIGTERM);
        }
    }
    let _ = timeout(grace, child.wait()).await;
    terminate_remaining_process_group(child_id, grace).await;
    let _ = child.wait().await;
}

#[cfg(unix)]
async fn terminate_remaining_process_group(child_id: Option<u32>, grace: Duration) {
    if let Some(id) = child_id.and_then(|value| i32::try_from(value).ok()) {
        unsafe {
            libc::kill(-id, libc::SIGTERM);
        }
        let group_alive = unsafe { libc::kill(-id, 0) == 0 };
        if group_alive {
            tokio::time::sleep(grace).await;
        }
        if unsafe { libc::kill(-id, 0) == 0 } {
            unsafe {
                libc::kill(-id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
async fn terminate_remaining_process_group(_child_id: Option<u32>, _grace: Duration) {}

#[cfg(not(unix))]
async fn terminate_process_tree(child: &mut Child, _child_id: Option<u32>, grace: Duration) {
    let _ = child.start_kill();
    let _ = timeout(grace, child.wait()).await;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("ffprobe configuration must use an absolute executable and non-zero timeouts")]
    InvalidConfiguration,
    #[error("media input must be an absolute regular file and not a symlink")]
    InvalidInput,
    #[error("could not start or read ffprobe: {0}")]
    Spawn(String),
    #[error("ffprobe timed out")]
    TimedOut,
    #[error("ffprobe stdout exceeded 8 MiB")]
    StdoutTooLarge,
    #[error("ffprobe stderr exceeded 64 KiB")]
    StderrTooLarge,
    #[error("ffprobe failed with exit code {exit_code:?}: {message}")]
    ProbeFailed {
        exit_code: Option<i32>,
        message: String,
    },
    #[error("ffprobe returned malformed media data: {0}")]
    Malformed(String),
    #[error("unsupported media: {0}")]
    Unsupported(&'static str),
    #[error("media exceeds the {0} limit")]
    LimitExceeded(&'static str),
    #[error("interpolated output verification failed: {0}")]
    Verification(&'static str),
}

impl MediaError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfiguration | Self::Spawn(_) => "ENGINE_NOT_INSTALLED",
            Self::InvalidInput | Self::Malformed(_) | Self::Unsupported(_) => "UNSUPPORTED_MEDIA",
            Self::LimitExceeded(_) => "MEDIA_TOO_LARGE",
            Self::Verification(_) => "INVALID_OUTPUT",
            Self::TimedOut
            | Self::StdoutTooLarge
            | Self::StderrTooLarge
            | Self::ProbeFailed { .. } => "FFPROBE_FAILED",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fixture(name: &str, extension: &str) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join(format!("input.{extension}"));
        fs::write(&input, b"fixture").expect("input fixture");
        let json = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .expect("probe fixture");
        (directory, input, json)
    }

    #[test]
    fn video_only_plan_contains_the_video_and_copy_policies() {
        let (_directory, input, bytes) = fixture("video-only.json", "mp4");
        let descriptor =
            parse_descriptor(&input, &bytes, ProbeMode::Source).expect("valid video-only probe");
        assert_eq!(
            descriptor.frame_rate,
            RationalRate {
                numerator: 30,
                denominator: 1
            }
        );
        assert_eq!(descriptor.mux_plan().streams.len(), 1);
        assert_eq!(
            descriptor.mux_plan().streams[0].action,
            MuxStreamAction::InterpolateVideo
        );
        assert!(descriptor.mux_plan().copy_metadata);
        assert!(descriptor.mux_plan().copy_chapters);
    }

    #[test]
    fn single_and_multiple_audio_streams_are_all_copied() {
        for (name, expected) in [("single-audio.json", 2), ("multi-audio.json", 3)] {
            let (_directory, input, bytes) = fixture(name, "mkv");
            let descriptor =
                parse_descriptor(&input, &bytes, ProbeMode::Source).expect("valid audio probe");
            let plan = descriptor.mux_plan();
            assert_eq!(plan.streams.len(), expected);
            assert_eq!(
                plan.streams
                    .iter()
                    .filter(|stream| stream.action == MuxStreamAction::Copy)
                    .count(),
                expected - 1
            );
        }
    }

    #[test]
    fn text_subtitle_is_complete_and_container_specific() {
        let (_directory, input, bytes) = fixture("text-subtitle.json", "mp4");
        let descriptor =
            parse_descriptor(&input, &bytes, ProbeMode::Source).expect("valid subtitle probe");
        let plan = descriptor.mux_plan();
        assert_eq!(plan.streams.len(), descriptor.streams.len());
        assert_eq!(plan.streams[2].kind, MuxStreamKind::Subtitle);
        assert_eq!(plan.streams[2].action, MuxStreamAction::TranscodeMovText);
        assert_eq!(descriptor.chapters.len(), 1);

        let mut mkv_json: serde_json::Value = serde_json::from_slice(&bytes).expect("fixture JSON");
        mkv_json["format"]["format_name"] = "matroska,webm".into();
        let mkv_input = input.with_extension("mkv");
        let mkv = parse_descriptor(
            &mkv_input,
            &serde_json::to_vec(&mkv_json).expect("MKV probe JSON"),
            ProbeMode::Source,
        )
        .expect("valid MKV subtitle probe");
        assert_eq!(mkv.mux_plan().streams[2].action, MuxStreamAction::Copy);
    }

    #[test]
    fn bitmap_subtitle_is_rejected_before_planning() {
        let (_directory, input, bytes) = fixture("bitmap-subtitle.json", "mkv");
        assert_eq!(
            parse_descriptor(&input, &bytes, ProbeMode::Source),
            Err(MediaError::Unsupported("bitmap subtitle stream"))
        );
    }

    #[test]
    fn rejects_vfr_hdr_interlaced_oversized_and_malformed_media() {
        let (_directory, input, base) = fixture("video-only.json", "mp4");
        let mutate = |field: &str, value: serde_json::Value| {
            let mut json: serde_json::Value = serde_json::from_slice(&base).expect("fixture JSON");
            json["streams"][0][field] = value;
            serde_json::to_vec(&json).expect("mutated JSON")
        };
        assert!(matches!(
            parse_descriptor(
                &input,
                &mutate("avg_frame_rate", "24000/1001".into()),
                ProbeMode::Source
            ),
            Err(MediaError::Unsupported("variable frame rate"))
        ));
        assert!(matches!(
            parse_descriptor(
                &input,
                &mutate("color_transfer", "smpte2084".into()),
                ProbeMode::Source
            ),
            Err(MediaError::Unsupported("HDR video"))
        ));
        assert!(matches!(
            parse_descriptor(
                &input,
                &mutate("field_order", "tt".into()),
                ProbeMode::Source
            ),
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            parse_descriptor(&input, &mutate("width", 9000.into()), ProbeMode::Source),
            Err(MediaError::LimitExceeded("dimensions"))
        ));
        assert!(matches!(
            parse_descriptor(
                &input,
                &mutate("nb_read_frames", "1000001".into()),
                ProbeMode::Source
            ),
            Err(MediaError::LimitExceeded("frame_count"))
        ));
        let mut duration: serde_json::Value = serde_json::from_slice(&base).expect("fixture JSON");
        duration["format"]["duration"] = "21600.001".into();
        assert!(matches!(
            parse_descriptor(
                &input,
                &serde_json::to_vec(&duration).expect("duration JSON"),
                ProbeMode::Source
            ),
            Err(MediaError::LimitExceeded("duration"))
        ));
        assert!(matches!(
            parse_descriptor(&input, b"{not-json", ProbeMode::Source),
            Err(MediaError::Malformed(_))
        ));
        assert!(matches!(
            parse_descriptor(
                &input,
                &mutate("r_frame_rate", "50/2".into()),
                ProbeMode::Source
            ),
            Err(MediaError::Malformed(_))
        ));
    }

    #[test]
    fn supports_mov_and_rejects_multiple_video_data_and_attachment_streams() {
        let (_directory, input, base) = fixture("video-only.json", "mp4");
        let mov_input = input.with_extension("mov");
        assert_eq!(
            parse_descriptor(&mov_input, &base, ProbeMode::Source)
                .expect("MOV uses the validated mov demuxer family")
                .container,
            VideoContainer::Mov
        );

        for codec_type in ["video", "data", "attachment"] {
            let mut json: serde_json::Value = serde_json::from_slice(&base).expect("fixture JSON");
            let mut extra = json["streams"][0].clone();
            extra["index"] = 4.into();
            extra["codec_type"] = codec_type.into();
            json["streams"]
                .as_array_mut()
                .expect("stream array")
                .push(extra);
            assert!(matches!(
                parse_descriptor(
                    &input,
                    &serde_json::to_vec(&json).expect("probe JSON"),
                    ProbeMode::Source
                ),
                Err(MediaError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn output_probe_and_verification_require_an_exact_two_x_result() {
        let (_directory, input, source_bytes) = fixture("multi-audio.json", "mkv");
        let source =
            parse_descriptor(&input, &source_bytes, ProbeMode::Source).expect("source descriptor");
        assert!(
            source
                .streams
                .iter()
                .all(|stream| stream.duration_from_format)
        );

        let mut output_json: serde_json::Value =
            serde_json::from_slice(&source_bytes).expect("fixture JSON");
        output_json["streams"][0]["r_frame_rate"] = "60000/1001".into();
        output_json["streams"][0]["avg_frame_rate"] = "60000/1001".into();
        output_json["streams"][0]["nb_read_frames"] = "600".into();
        let output = parse_descriptor(
            &input,
            &serde_json::to_vec(&output_json).expect("output JSON"),
            ProbeMode::InterpolatedOutput,
        )
        .expect("output descriptor");
        let plan = source.mux_plan();
        verify_interpolated_output(&source, &output, &plan).expect("exact 2x output");

        let mut wrong_frames = output.clone();
        wrong_frames.frame_count -= 1;
        assert_eq!(
            verify_interpolated_output(&source, &wrong_frames, &plan),
            Err(MediaError::Verification(
                "frame count is not exactly doubled"
            ))
        );
        let mut wrong_duration = output.clone();
        wrong_duration.duration_ms += 18;
        assert_eq!(
            verify_interpolated_output(&source, &wrong_duration, &plan),
            Err(MediaError::Verification(
                "duration changed by more than one frame"
            ))
        );
        let mut missing_audio = output.clone();
        missing_audio
            .streams
            .retain(|stream| stream.input_index != 2);
        assert_eq!(
            verify_interpolated_output(&source, &missing_audio, &plan),
            Err(MediaError::Verification("stream kind counts changed"))
        );
        let mut wrong_audio_duration = output.clone();
        wrong_audio_duration.streams[1].duration_ms -= 18;
        assert_eq!(
            verify_interpolated_output(&source, &wrong_audio_duration, &plan),
            Err(MediaError::Verification("audio and video duration differ"))
        );
        let mut incomplete_plan = plan.clone();
        incomplete_plan.streams.pop();
        assert!(matches!(
            verify_interpolated_output(&source, &output, &incomplete_plan),
            Err(MediaError::Verification(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_spawn_uses_the_exact_fixed_arguments() {
        let (_directory, input, bytes) = fixture("video-only.json", "mp4");
        let tool_directory = tempfile::tempdir().expect("tool tempdir");
        let capture = tool_directory.path().join("args.txt");
        let json = tool_directory.path().join("probe.json");
        fs::write(&json, bytes).expect("probe JSON");
        let script = tool_directory.path().join("ffprobe fixture");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n: > '{}'\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\n/bin/cat '{}'\n",
                capture.display(), capture.display(), json.display()
            ),
        )
        .expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("permissions");

        let descriptor = Ffprobe::new(&script, Duration::from_secs(2), Duration::from_millis(100))
            .expect("ffprobe config")
            .probe(&input)
            .await
            .expect("probe succeeds");
        assert_eq!(descriptor.input_path, input);
        let arguments = fs::read_to_string(capture).expect("captured arguments");
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            vec![
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-count_frames",
                input.to_str().expect("UTF-8 fixture path"),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_the_ffprobe_process_group() {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join("input.mp4");
        fs::write(&input, b"fixture").expect("input");
        let pid_file = directory.path().join("child.pid");
        let script = directory.path().join("slow-ffprobe");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n/bin/sleep 30 &\necho $! > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("permissions");

        let result = Ffprobe::new(&script, Duration::from_secs(2), Duration::from_millis(100))
            .expect("ffprobe config")
            .probe(&input)
            .await;
        assert_eq!(result, Err(MediaError::TimedOut));
        let pid = fs::read_to_string(pid_file)
            .expect("child pid")
            .trim()
            .parse::<i32>()
            .expect("numeric pid");
        for _ in 0..20 {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ffprobe grandchild {pid} survived timeout cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_and_stderr_are_bounded() {
        let directory = tempfile::tempdir().expect("tempdir");
        let input = directory.path().join("input.mp4");
        fs::write(&input, b"fixture").expect("input");
        for (name, command, expected) in [
            (
                "large-stdout",
                format!(
                    "/usr/bin/yes x | /usr/bin/head -c {}",
                    MAX_FFPROBE_STDOUT_BYTES + 1
                ),
                MediaError::StdoutTooLarge,
            ),
            (
                "large-stderr",
                format!(
                    "/usr/bin/yes x | /usr/bin/head -c {} >&2",
                    MAX_FFPROBE_STDERR_BYTES + 1
                ),
                MediaError::StderrTooLarge,
            ),
        ] {
            let script = directory.path().join(name);
            fs::write(&script, format!("#!/bin/sh\n{command}\n")).expect("script");
            let mut permissions = fs::metadata(&script).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script, permissions).expect("permissions");
            let result = Ffprobe::new(&script, Duration::from_secs(2), Duration::from_millis(100))
                .expect("config")
                .probe(&input)
                .await;
            assert_eq!(result, Err(expected));
        }
    }
}
