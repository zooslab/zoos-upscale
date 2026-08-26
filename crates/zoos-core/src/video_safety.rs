use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{RationalRate, VideoContainer};
use crate::image_safety::{no_replace_rename, sha256_file, sync_parent_directory};

const MAX_VIDEO_INPUT_BYTES: u64 = 512 * 1024 * 1024 * 1024;
const MIN_VIDEO_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedVideoFile {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub container: VideoContainer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoOutputPlan {
    pub job_id: String,
    pub input: ValidatedVideoFile,
    pub final_path: PathBuf,
    pub partial_path: PathBuf,
    pub private_output_path: PathBuf,
    pub required_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPipelineVerification {
    pub schema_version: u32,
    pub job_id: String,
    pub input_path: PathBuf,
    pub input_sha256_before: String,
    pub input_sha256_after: String,
    pub output_path: PathBuf,
    pub output_sha256: String,
    pub container: VideoContainer,
    pub width: u32,
    pub height: u32,
    pub source_rate: RationalRate,
    pub target_rate: RationalRate,
    pub source_frames: u64,
    pub output_frames: u64,
    pub duration_ms: u64,
    pub audio_streams: u32,
    pub subtitle_streams: u32,
    pub chapter_count: u32,
    pub scene_cut_count: u64,
    pub chunk_count: u32,
}

#[allow(clippy::too_many_arguments)]
pub fn plan_video_output(
    input_path: &Path,
    container: VideoContainer,
    job_id: &str,
    workspace_work_dir: &Path,
    estimated_output_bytes: u64,
    bounded_workspace_bytes: u64,
    reserved_outputs: &HashSet<PathBuf>,
) -> Result<VideoOutputPlan, VideoSafetyError> {
    let input = validate_video_file(input_path, container)?;
    let job_id = Uuid::parse_str(job_id)
        .map_err(|_| VideoSafetyError::InvalidJobId)?
        .to_string();
    require_absolute_private_work(workspace_work_dir)?;

    let parent = input
        .path
        .parent()
        .ok_or(VideoSafetyError::InvalidInputPath)?;
    let output_directory = ensure_output_directory(parent, "Interpolated")?;
    let required_free_bytes = MIN_VIDEO_FREE_BYTES.max(
        estimated_output_bytes
            .saturating_mul(2)
            .saturating_add(bounded_workspace_bytes),
    );
    let available = fs4::available_space(&output_directory)?;
    if available < required_free_bytes {
        return Err(VideoSafetyError::InsufficientDisk {
            required: required_free_bytes,
            available,
        });
    }

    let stem = input
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or(VideoSafetyError::InvalidInputPath)?;
    let extension = container_extension(container);
    let base = format!("{stem}_interpolated_2x");
    let final_path = first_available_output(&output_directory, &base, extension, reserved_outputs)?;
    let final_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(VideoSafetyError::InvalidInputPath)?;
    let partial_path =
        output_directory.join(format!(".{final_name}.zoos-{job_id}.partial.{extension}"));
    if path_exists_no_follow(&partial_path)? || reserved_outputs.contains(&partial_path) {
        return Err(VideoSafetyError::OutputExists(partial_path));
    }
    let private_output_path = workspace_work_dir.join(format!("interpolated.{extension}"));
    if private_output_path == input.path || private_output_path == final_path {
        return Err(VideoSafetyError::InvalidOutputPath);
    }

    Ok(VideoOutputPlan {
        job_id,
        input,
        final_path,
        partial_path,
        private_output_path,
        required_free_bytes,
    })
}

pub fn validate_video_file(
    path: &Path,
    container: VideoContainer,
) -> Result<ValidatedVideoFile, VideoSafetyError> {
    if !path.is_absolute() || !extension_matches(path, container) {
        return Err(VideoSafetyError::InvalidInputPath);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(VideoSafetyError::InvalidInputPath);
    }
    if metadata.len() > MAX_VIDEO_INPUT_BYTES {
        return Err(VideoSafetyError::InputTooLarge(metadata.len()));
    }
    Ok(ValidatedVideoFile {
        path: path.to_owned(),
        sha256: sha256_file(path).map_err(VideoSafetyError::from_image_safety)?,
        size_bytes: metadata.len(),
        container,
    })
}

pub fn recheck_video_input(input: &ValidatedVideoFile) -> Result<String, VideoSafetyError> {
    let current = sha256_file(&input.path).map_err(|_| VideoSafetyError::InputChanged)?;
    if current != input.sha256 {
        return Err(VideoSafetyError::InputChanged);
    }
    Ok(current)
}

pub fn stage_private_video_output(plan: &VideoOutputPlan) -> Result<String, VideoSafetyError> {
    require_regular_file(&plan.private_output_path)?;
    require_safe_planned_destination(plan)?;
    let source = File::open(&plan.private_output_path)?;
    let destination = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&plan.partial_path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                VideoSafetyError::OutputExists(plan.partial_path.clone())
            } else {
                error.into()
            }
        })?;
    let copy_result = copy_and_sync(source, destination);
    if let Err(error) = copy_result {
        let _ = remove_file_if_present(&plan.partial_path);
        return Err(error);
    }
    sha256_file(&plan.partial_path).map_err(VideoSafetyError::from_image_safety)
}

pub fn publish_staged_video_output(
    plan: &VideoOutputPlan,
    verification_path: &Path,
    verification: &VideoPipelineVerification,
) -> Result<(), VideoSafetyError> {
    publish_staged_video_output_with_hook(plan, verification_path, verification, || {})
}

fn publish_staged_video_output_with_hook(
    plan: &VideoOutputPlan,
    verification_path: &Path,
    verification: &VideoPipelineVerification,
    before_rename: impl FnOnce(),
) -> Result<(), VideoSafetyError> {
    validate_verification(plan, verification)?;
    require_safe_planned_destination(plan)?;
    let mut staged_file = open_regular_file_no_follow(&plan.partial_path)?;
    let staged_identity = FileIdentity::from_file(&staged_file)?;
    let staged_hash = sha256_open_file(&mut staged_file)?;
    if staged_hash != verification.output_sha256 {
        return Err(VideoSafetyError::InvalidOutput(
            "staged output hash does not match verification",
        ));
    }
    let input_after = recheck_video_input(&plan.input)?;
    if input_after != verification.input_sha256_after {
        return Err(VideoSafetyError::InputChanged);
    }

    write_json_atomic(verification_path, verification)?;
    before_rename();
    let current_identity = match open_regular_file_no_follow(&plan.partial_path)
        .and_then(|file| FileIdentity::from_file(&file))
    {
        Ok(identity) if identity == staged_identity => identity,
        Ok(_) | Err(_) => {
            let _ = remove_file_if_present(&plan.partial_path);
            let _ = remove_file_if_present(verification_path);
            return Err(VideoSafetyError::InvalidOutput(
                "staged output identity changed before publish",
            ));
        }
    };
    if let Err(error) = no_replace_rename(&plan.partial_path, &plan.final_path) {
        let _ = remove_file_if_present(verification_path);
        return Err(VideoSafetyError::from_image_safety(error));
    }
    let published = (|| {
        let mut final_file = open_regular_file_no_follow(&plan.final_path)?;
        let final_identity = FileIdentity::from_file(&final_file)?;
        if final_identity != current_identity || final_identity != staged_identity {
            return Err(VideoSafetyError::InvalidOutput(
                "published output identity does not match the verified staging file",
            ));
        }
        if sha256_open_file(&mut final_file)? != verification.output_sha256 {
            return Err(VideoSafetyError::InvalidOutput(
                "published output hash does not match verification",
            ));
        }
        Ok(())
    })();
    if let Err(error) = published {
        let _ = remove_file_if_present(&plan.final_path);
        let _ = remove_file_if_present(verification_path);
        return Err(error);
    }
    if let Err(error) = sync_parent_directory(&plan.final_path) {
        if sha256_file(&plan.final_path).ok().as_deref()
            == Some(verification.output_sha256.as_str())
        {
            let _ = fs::remove_file(&plan.final_path);
        }
        let _ = remove_file_if_present(verification_path);
        return Err(VideoSafetyError::from_image_safety(error));
    }
    Ok(())
}

pub(crate) fn validate_published_video_output(
    plan: &VideoOutputPlan,
    verification: &VideoPipelineVerification,
) -> Result<(), VideoSafetyError> {
    validate_verification(plan, verification)?;
    require_safe_planned_destination(plan)?;
    if path_exists_no_follow(&plan.partial_path)? {
        return Err(VideoSafetyError::InvalidOutput(
            "completed publish still has a partial output",
        ));
    }
    let mut final_file = open_regular_file_no_follow(&plan.final_path)?;
    if sha256_open_file(&mut final_file)? != verification.output_sha256 {
        return Err(VideoSafetyError::InvalidOutput(
            "completed output hash does not match verification",
        ));
    }
    Ok(())
}

pub fn cleanup_owned_video_output(
    plan: &VideoOutputPlan,
    verification_path: &Path,
) -> Result<(), VideoSafetyError> {
    let partial_was_present = path_exists_no_follow(&plan.partial_path)?;
    remove_file_if_present(&plan.partial_path)?;
    remove_file_if_present(&plan.private_output_path)?;
    let verification = match File::open(verification_path) {
        Ok(file) => {
            serde_json::from_reader::<_, VideoPipelineVerification>(BufReader::new(file)).ok()
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if !partial_was_present
        && let Some(verification) = verification
        && verification.job_id == plan.job_id
        && verification.output_path == plan.final_path
        && sha256_file(&plan.final_path).ok().as_deref()
            == Some(verification.output_sha256.as_str())
    {
        remove_file_if_present(&plan.final_path)?;
    }
    remove_file_if_present(verification_path)?;
    Ok(())
}

pub fn cleanup_video_work_directory(work: &Path) -> Result<(), VideoSafetyError> {
    if !work.is_absolute() || work.file_name().and_then(|name| name.to_str()) != Some("work") {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    match fs::symlink_metadata(work) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(VideoSafetyError::InvalidOutputPath)
        }
        Ok(_) => {
            fs::remove_dir_all(work)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_verification(
    plan: &VideoOutputPlan,
    verification: &VideoPipelineVerification,
) -> Result<(), VideoSafetyError> {
    let valid = verification.schema_version == 1
        && verification.job_id == plan.job_id
        && verification.input_path == plan.input.path
        && verification.input_sha256_before == plan.input.sha256
        && verification.input_sha256_after == plan.input.sha256
        && verification.output_path == plan.final_path
        && verification.container == plan.input.container
        && verification.width > 0
        && verification.height > 0
        && verification.source_rate.numerator > 0
        && verification.source_rate.denominator > 0
        && verification.target_rate.numerator > 0
        && verification.target_rate.denominator > 0
        && verification.source_frames > 0
        && verification.output_frames == verification.source_frames.saturating_mul(2)
        && verification.duration_ms > 0
        && verification.chunk_count > 0
        && verification.output_sha256.len() == 64
        && verification
            .output_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(VideoSafetyError::InvalidOutput(
            "verification does not match the planned video output",
        ))
    }
}

fn require_safe_planned_destination(plan: &VideoOutputPlan) -> Result<(), VideoSafetyError> {
    if plan.final_path == plan.input.path
        || plan.partial_path == plan.input.path
        || plan.final_path.parent() != plan.partial_path.parent()
    {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    let input_parent = plan
        .input
        .path
        .parent()
        .ok_or(VideoSafetyError::InvalidInputPath)?;
    let expected_directory = input_parent.join("Interpolated");
    if plan.final_path.parent() != Some(expected_directory.as_path()) {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    require_safe_directory(&expected_directory)
}

fn require_absolute_private_work(path: &Path) -> Result<(), VideoSafetyError> {
    if !path.is_absolute() || path.file_name().and_then(|name| name.to_str()) != Some("work") {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    Ok(())
}

fn ensure_output_directory(parent: &Path, child: &str) -> Result<PathBuf, VideoSafetyError> {
    require_safe_directory(parent)?;
    let output = parent.join(child);
    match fs::create_dir(&output) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    require_safe_directory(&output)?;
    Ok(output)
}

fn require_safe_directory(path: &Path) -> Result<(), VideoSafetyError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(VideoSafetyError::InvalidOutputPath);
    }
    Ok(())
}

fn require_regular_file(path: &Path) -> Result<(), VideoSafetyError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(VideoSafetyError::InvalidOutput(
            "output is not a regular file",
        ));
    }
    Ok(())
}

fn open_regular_file_no_follow(path: &Path) -> Result<File, VideoSafetyError> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.len() == 0
    {
        return Err(VideoSafetyError::InvalidOutput(
            "output is not a regular file",
        ));
    }
    let file = File::open(path)?;
    let file_metadata = file.metadata()?;
    if !file_metadata.is_file() || file_metadata.len() == 0 {
        return Err(VideoSafetyError::InvalidOutput(
            "output is not a regular file",
        ));
    }
    #[cfg(unix)]
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(VideoSafetyError::InvalidOutput(
            "output identity changed while it was opened",
        ));
    }
    Ok(file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn from_file(file: &File) -> Result<Self, VideoSafetyError> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(Self {})
        }
    }
}

fn sha256_open_file(file: &mut File) -> Result<String, VideoSafetyError> {
    use sha2::{Digest, Sha256};

    file.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn first_available_output(
    directory: &Path,
    base: &str,
    extension: &str,
    reserved: &HashSet<PathBuf>,
) -> Result<PathBuf, VideoSafetyError> {
    for suffix in 1..=999 {
        let name = if suffix == 1 {
            format!("{base}.{extension}")
        } else {
            format!("{base}_{suffix}.{extension}")
        };
        let candidate = directory.join(name);
        if !path_exists_no_follow(&candidate)? && !reserved.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(VideoSafetyError::NoOutputNameAvailable)
}

fn extension_matches(path: &Path, container: VideoContainer) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(container_extension(container)))
}

pub(crate) const fn container_extension(container: VideoContainer) -> &'static str {
    match container {
        VideoContainer::Mp4 => "mp4",
        VideoContainer::Mov => "mov",
        VideoContainer::Mkv => "mkv",
    }
}

fn copy_and_sync(source: File, destination: File) -> Result<(), VideoSafetyError> {
    let mut reader = BufReader::new(source);
    let mut writer = BufWriter::new(destination);
    io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn path_exists_no_follow(path: &Path) -> Result<bool, VideoSafetyError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), VideoSafetyError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), VideoSafetyError> {
    let mut file = AtomicWriteFile::open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.commit()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum VideoSafetyError {
    #[error("input path must be an absolute non-empty MP4, MOV, or MKV regular file")]
    InvalidInputPath,
    #[error("video input is larger than 512 GiB ({0} bytes)")]
    InputTooLarge(u64),
    #[error("job id must be a UUID")]
    InvalidJobId,
    #[error("planned output path is outside the managed destination")]
    InvalidOutputPath,
    #[error("insufficient disk space: need {required} bytes, have {available} bytes")]
    InsufficientDisk { required: u64, available: u64 },
    #[error("all output names through suffix _999 already exist")]
    NoOutputNameAvailable,
    #[error("output already exists: {0}")]
    OutputExists(PathBuf),
    #[error("input changed after planning")]
    InputChanged,
    #[error("invalid generated video: {0}")]
    InvalidOutput(&'static str),
    #[error("video I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("verification JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl VideoSafetyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInputPath | Self::InputTooLarge(_) => "UNSUPPORTED_MEDIA",
            Self::InsufficientDisk { .. } => "INSUFFICIENT_DISK",
            Self::NoOutputNameAvailable | Self::OutputExists(_) => "OUTPUT_EXISTS",
            Self::InputChanged => "INPUT_CHANGED",
            Self::InvalidJobId
            | Self::InvalidOutputPath
            | Self::InvalidOutput(_)
            | Self::Io(_)
            | Self::Json(_) => "VIDEO_VERIFICATION_FAILED",
        }
    }

    fn from_image_safety(error: crate::ImageSafetyError) -> Self {
        match error {
            crate::ImageSafetyError::InputChanged => Self::InputChanged,
            crate::ImageSafetyError::OutputExists(path) => Self::OutputExists(path),
            crate::ImageSafetyError::Io(error) => Self::Io(error),
            _ => Self::InvalidOutput("hash or atomic publication failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, b"video-fixture").expect("fixture must write");
        path
    }

    fn plan(directory: &Path) -> VideoOutputPlan {
        let input = fixture(directory, "clip.mp4");
        let workspace = directory.join("workspace").join("work");
        fs::create_dir_all(&workspace).expect("workspace must exist");
        plan_video_output(
            &input,
            VideoContainer::Mp4,
            "123e4567-e89b-12d3-a456-426614174000",
            &workspace,
            1,
            1,
            &HashSet::new(),
        )
        .expect("plan must succeed")
    }

    #[test]
    fn plans_same_container_destination_and_reserves_names() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let input = fixture(directory.path(), "한 글.mp4");
        let workspace = directory.path().join("job").join("work");
        fs::create_dir_all(&workspace).expect("workspace must exist");
        let first = plan_video_output(
            &input,
            VideoContainer::Mp4,
            "123e4567-e89b-12d3-a456-426614174000",
            &workspace,
            1,
            1,
            &HashSet::new(),
        )
        .expect("first plan");
        assert_eq!(
            first.final_path.file_name().and_then(|name| name.to_str()),
            Some("한 글_interpolated_2x.mp4")
        );
        let second = plan_video_output(
            &input,
            VideoContainer::Mp4,
            "223e4567-e89b-12d3-a456-426614174000",
            &workspace,
            1,
            1,
            &HashSet::from([first.final_path.clone()]),
        )
        .expect("reserved plan");
        assert_eq!(
            second.final_path.file_name().and_then(|name| name.to_str()),
            Some("한 글_interpolated_2x_2.mp4")
        );
    }

    #[test]
    fn rejects_relative_wrong_extension_empty_symlink_and_oversized_input() {
        assert!(matches!(
            validate_video_file(Path::new("clip.mp4"), VideoContainer::Mp4),
            Err(VideoSafetyError::InvalidInputPath)
        ));
        let directory = tempfile::tempdir().expect("temporary directory");
        let wrong = fixture(directory.path(), "clip.mkv");
        assert!(matches!(
            validate_video_file(&wrong, VideoContainer::Mp4),
            Err(VideoSafetyError::InvalidInputPath)
        ));
        let empty = directory.path().join("empty.mp4");
        File::create(&empty).expect("empty fixture");
        assert!(matches!(
            validate_video_file(&empty, VideoContainer::Mp4),
            Err(VideoSafetyError::InvalidInputPath)
        ));
        let oversized = directory.path().join("large.mp4");
        File::create(&oversized)
            .expect("large fixture")
            .set_len(MAX_VIDEO_INPUT_BYTES + 1)
            .expect("sparse fixture");
        assert!(matches!(
            validate_video_file(&oversized, VideoContainer::Mp4),
            Err(VideoSafetyError::InputTooLarge(_))
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&wrong, directory.path().join("link.mp4"))
                .expect("symlink fixture");
            assert!(matches!(
                validate_video_file(&directory.path().join("link.mp4"), VideoContainer::Mp4),
                Err(VideoSafetyError::InvalidInputPath)
            ));
        }
    }

    #[test]
    fn stages_and_publishes_only_verified_bytes_without_overwrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let plan = plan(directory.path());
        fs::write(&plan.private_output_path, b"verified-video").expect("private output");
        let output_hash = stage_private_video_output(&plan).expect("stage output");
        let verification_path = directory.path().join("verification.json");
        let verification = VideoPipelineVerification {
            schema_version: 1,
            job_id: plan.job_id.clone(),
            input_path: plan.input.path.clone(),
            input_sha256_before: plan.input.sha256.clone(),
            input_sha256_after: plan.input.sha256.clone(),
            output_path: plan.final_path.clone(),
            output_sha256: output_hash,
            container: VideoContainer::Mp4,
            width: 64,
            height: 64,
            source_rate: RationalRate {
                numerator: 30,
                denominator: 1,
            },
            target_rate: RationalRate {
                numerator: 60,
                denominator: 1,
            },
            source_frames: 2,
            output_frames: 4,
            duration_ms: 67,
            audio_streams: 0,
            subtitle_streams: 0,
            chapter_count: 0,
            scene_cut_count: 0,
            chunk_count: 1,
        };
        publish_staged_video_output(&plan, &verification_path, &verification)
            .expect("publish output");
        assert_eq!(
            fs::read(&plan.final_path).expect("final output"),
            b"verified-video"
        );
        assert!(!plan.partial_path.exists());

        fs::write(&plan.private_output_path, b"new-video").expect("replacement private output");
        assert!(stage_private_video_output(&plan).is_ok());
        assert!(matches!(
            publish_staged_video_output(&plan, &verification_path, &verification),
            Err(VideoSafetyError::InvalidOutput(_)) | Err(VideoSafetyError::OutputExists(_))
        ));
        assert_eq!(
            fs::read(&plan.final_path).expect("existing final"),
            b"verified-video"
        );
    }

    #[cfg(unix)]
    #[test]
    fn swapped_staging_inode_is_never_left_published() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let plan = plan(directory.path());
        fs::write(&plan.private_output_path, b"verified-video").expect("private output");
        let output_hash = stage_private_video_output(&plan).expect("stage output");
        let verification_path = directory.path().join("verification.json");
        let verification = VideoPipelineVerification {
            schema_version: 1,
            job_id: plan.job_id.clone(),
            input_path: plan.input.path.clone(),
            input_sha256_before: plan.input.sha256.clone(),
            input_sha256_after: plan.input.sha256.clone(),
            output_path: plan.final_path.clone(),
            output_sha256: output_hash,
            container: VideoContainer::Mp4,
            width: 64,
            height: 64,
            source_rate: RationalRate {
                numerator: 30,
                denominator: 1,
            },
            target_rate: RationalRate {
                numerator: 60,
                denominator: 1,
            },
            source_frames: 2,
            output_frames: 4,
            duration_ms: 67,
            audio_streams: 0,
            subtitle_streams: 0,
            chapter_count: 0,
            scene_cut_count: 0,
            chunk_count: 1,
        };
        let displaced = plan.partial_path.with_extension("displaced");
        let result =
            publish_staged_video_output_with_hook(&plan, &verification_path, &verification, || {
                fs::rename(&plan.partial_path, &displaced).expect("displace verified inode");
                // Identical bytes prove that the inode check, rather than a second hash alone,
                // rejects replacement between verification and rename.
                fs::write(&plan.partial_path, b"verified-video").expect("replacement inode");
            });
        assert!(matches!(result, Err(VideoSafetyError::InvalidOutput(_))));
        assert!(!plan.final_path.exists());
        assert!(!plan.partial_path.exists());
        assert!(!verification_path.exists());
        assert_eq!(
            fs::read(displaced).expect("verified inode remains"),
            b"verified-video"
        );
    }

    #[test]
    fn input_change_blocks_publish_and_cleanup_removes_only_owned_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let plan = plan(directory.path());
        fs::write(&plan.private_output_path, b"verified-video").expect("private output");
        let output_hash = stage_private_video_output(&plan).expect("stage output");
        fs::write(&plan.input.path, b"changed").expect("change input");
        let verification_path = directory.path().join("verification.json");
        let verification = VideoPipelineVerification {
            schema_version: 1,
            job_id: plan.job_id.clone(),
            input_path: plan.input.path.clone(),
            input_sha256_before: plan.input.sha256.clone(),
            input_sha256_after: plan.input.sha256.clone(),
            output_path: plan.final_path.clone(),
            output_sha256: output_hash,
            container: VideoContainer::Mp4,
            width: 1,
            height: 1,
            source_rate: RationalRate {
                numerator: 25,
                denominator: 1,
            },
            target_rate: RationalRate {
                numerator: 50,
                denominator: 1,
            },
            source_frames: 1,
            output_frames: 2,
            duration_ms: 40,
            audio_streams: 0,
            subtitle_streams: 0,
            chapter_count: 0,
            scene_cut_count: 0,
            chunk_count: 1,
        };
        assert!(matches!(
            publish_staged_video_output(&plan, &verification_path, &verification),
            Err(VideoSafetyError::InputChanged)
        ));
        cleanup_owned_video_output(&plan, &verification_path).expect("cleanup");
        assert!(!plan.partial_path.exists());
        assert!(!plan.private_output_path.exists());
        assert!(plan.input.path.exists());
    }

    #[test]
    fn public_error_codes_are_stable() {
        assert_eq!(VideoSafetyError::InputChanged.code(), "INPUT_CHANGED");
        assert_eq!(
            VideoSafetyError::OutputExists(PathBuf::from("out.mp4")).code(),
            "OUTPUT_EXISTS"
        );
        assert_eq!(
            VideoSafetyError::InsufficientDisk {
                required: 2,
                available: 1,
            }
            .code(),
            "INSUFFICIENT_DISK"
        );
        assert_eq!(MIN_VIDEO_FREE_BYTES, 5_368_709_120);
    }
}
