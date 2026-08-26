use std::fs::{self, File};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use zoos_core::{
    BackendError, ImagePreset, JobKind, JobOrchestrator, JobSummary, OrchestratorError,
    WorkspaceError,
};

const RUNTIME_ASSET_SUBDIRECTORY: &str = "realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal";

const RUNTIME_ASSETS: [RuntimeAsset; 5] = [
    RuntimeAsset {
        relative_path: "bin/realesrgan-ncnn-vulkan",
        sha256: "c1c35d92079085de96b9d547fd7e4464bc8a2e9ccf28d7b8c712d72ade91b7cc",
        executable: true,
    },
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus.param",
        sha256: "35330ececcea33b6c397a72548e788d5d53becee4734c50b7fada36e89f10a86",
        executable: false,
    },
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus.bin",
        sha256: "713ee713b0353afaa27976f0563a64a5043bd70b9bd8936c2e26e25ebcdbcddf",
        executable: false,
    },
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus-anime.param",
        sha256: "2b8fb6e0ae4d2d85704ca08c119a2f5ea40add4f2ecd512eb7f4cd44b6127ed4",
        executable: false,
    },
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus-anime.bin",
        sha256: "fe01c269cfd10cdef8e018ab66ebe750cf79c7af4d1f9c16c737e1295229bacc",
        executable: false,
    },
];

#[derive(Debug, Clone)]
pub struct ImageRuntime {
    pub wrapper_path: PathBuf,
    pub install_directory: PathBuf,
}

impl ImageRuntime {
    pub fn engine_path(&self) -> PathBuf {
        self.install_directory.join("bin/realesrgan-ncnn-vulkan")
    }

    pub fn models_path(&self) -> PathBuf {
        self.install_directory.join("models")
    }

    pub fn status(&self) -> ImageEngineStatus {
        runtime_status(self, &RUNTIME_ASSETS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ImageEngineState {
    Ready,
    NotInstalled,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageEngineStatus {
    pub state: ImageEngineState,
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl ImageEngineStatus {
    fn ready() -> Self {
        Self {
            state: ImageEngineState::Ready,
            code: None,
            message: "The verified local image engine is ready.".into(),
            engine_version: Some("0.2.5.0".into()),
            device: Some("gpu:0".into()),
        }
    }

    fn not_installed() -> Self {
        Self {
            state: ImageEngineState::NotInstalled,
            code: Some("ENGINE_NOT_INSTALLED".into()),
            message: "The verified local image engine is not installed.".into(),
            engine_version: None,
            device: None,
        }
    }

    fn invalid() -> Self {
        Self {
            state: ImageEngineState::Invalid,
            code: Some("ASSET_HASH_MISMATCH".into()),
            message: "The local image engine cache failed integrity verification.".into(),
            engine_version: None,
            device: None,
        }
    }

    fn into_result(self) -> Result<(), CommandError> {
        match self.state {
            ImageEngineState::Ready => Ok(()),
            ImageEngineState::NotInstalled | ImageEngineState::Invalid => Err(CommandError {
                code: self
                    .code
                    .expect("non-ready engine status has an error code"),
                message: self.message,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
}

#[tauri::command]
pub fn get_image_engine_status(runtime: State<'_, ImageRuntime>) -> ImageEngineStatus {
    runtime.status()
}

#[tauri::command]
pub async fn pick_and_create_image_job(
    app: AppHandle,
    orchestrator: State<'_, JobOrchestrator>,
    runtime: State<'_, ImageRuntime>,
    preset: ImagePreset,
    scale: u8,
) -> Result<Option<JobSummary>, CommandError> {
    runtime.status().into_result()?;
    if !matches!(scale, 2 | 4) {
        return Err(CommandError::fixed(
            "UNSUPPORTED_IMAGE_MODE",
            "Scale must be 2 or 4.",
        ));
    }
    let selected = app
        .dialog()
        .file()
        .add_filter("RGB8 image", &["png", "jpg", "jpeg"])
        .blocking_pick_file();
    let Some(selected) = selected else {
        return Ok(None);
    };
    let input_path = selected.into_path().map_err(|_| CommandError {
        code: "UNSUPPORTED_IMAGE_MODE".into(),
        message: "The selected item is not a local image file.".into(),
    })?;
    create_image_job(&orchestrator, &input_path, preset, scale).map(Some)
}

pub fn create_image_job(
    orchestrator: &JobOrchestrator,
    input_path: &Path,
    preset: ImagePreset,
    scale: u8,
) -> Result<JobSummary, CommandError> {
    orchestrator
        .create_image_job(input_path, preset, scale)
        .map_err(CommandError::from)
}

#[cfg(debug_assertions)]
#[tauri::command]
pub async fn create_fake_job(
    orchestrator: State<'_, JobOrchestrator>,
    scenario: zoos_core::FakeBehavior,
) -> Result<JobSummary, CommandError> {
    orchestrator
        .create_fake_job(scenario)
        .map_err(CommandError::from)
}

#[tauri::command]
pub async fn list_jobs(
    orchestrator: State<'_, JobOrchestrator>,
) -> Result<Vec<JobSummary>, CommandError> {
    orchestrator.list_jobs().map_err(CommandError::from)
}

#[tauri::command]
pub async fn start_job(
    orchestrator: State<'_, JobOrchestrator>,
    runtime: State<'_, ImageRuntime>,
    job_id: String,
) -> Result<JobSummary, CommandError> {
    if orchestrator
        .list_jobs()
        .map_err(CommandError::from)?
        .iter()
        .any(|job| job.job_id == job_id && job.kind == JobKind::ImageUpscale)
    {
        runtime.status().into_result()?;
    }
    orchestrator
        .start_job(&job_id)
        .await
        .map_err(CommandError::from)
}

#[tauri::command]
pub async fn cancel_job(
    orchestrator: State<'_, JobOrchestrator>,
    job_id: String,
) -> Result<JobSummary, CommandError> {
    orchestrator
        .cancel_job(&job_id)
        .await
        .map_err(CommandError::from)
}

impl From<OrchestratorError> for CommandError {
    fn from(error: OrchestratorError) -> Self {
        match error {
            OrchestratorError::AnotherJobActive => {
                Self::fixed("JOB_BUSY", "Another image job is already running.")
            }
            OrchestratorError::JobNotActive => {
                Self::fixed("JOB_NOT_ACTIVE", "This job is no longer running.")
            }
            OrchestratorError::InvalidState { .. } => Self::fixed(
                "INVALID_JOB_STATE",
                "This job cannot be started from its current state.",
            ),
            OrchestratorError::Workspace(WorkspaceError::Image(error)) => Self {
                code: error.code().into(),
                message: error.to_string(),
            },
            OrchestratorError::Backend(error) => Self::from_backend(error),
            OrchestratorError::Workspace(_) => Self::fixed(
                "UPSTREAM_FAILED",
                "The image job could not be updated safely.",
            ),
        }
    }
}

impl CommandError {
    fn fixed(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    fn from_backend(error: BackendError) -> Self {
        match error {
            BackendError::InvalidRunnerPath
            | BackendError::RunnerNotRegistered(_)
            | BackendError::SpawnFailed(_) => Self::fixed(
                "ENGINE_NOT_INSTALLED",
                "The verified local image engine is not installed.",
            ),
            BackendError::ProbeFailed(_) => Self::fixed(
                "ASSET_HASH_MISMATCH",
                "The local image engine cache failed integrity verification.",
            ),
            BackendError::RunnerFailed {
                error_code,
                message,
                ..
            } if matches!(
                error_code.as_str(),
                "ENGINE_NOT_INSTALLED"
                    | "ASSET_HASH_MISMATCH"
                    | "GPU_UNAVAILABLE"
                    | "UPSTREAM_FAILED"
                    | "CANCELLED"
            ) =>
            {
                Self {
                    code: error_code,
                    message,
                }
            }
            BackendError::Cancelled => Self::fixed("CANCELLED", "The image upscale was cancelled."),
            _ => Self::fixed(
                "UPSTREAM_FAILED",
                "The local image engine failed unexpectedly.",
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct RuntimeAsset {
    relative_path: &'static str,
    sha256: &'static str,
    executable: bool,
}

enum RuntimeValidationError {
    NotInstalled,
    Invalid,
}

fn runtime_status(runtime: &ImageRuntime, assets: &[RuntimeAsset]) -> ImageEngineStatus {
    match validate_runtime(runtime, assets) {
        Ok(()) => ImageEngineStatus::ready(),
        Err(RuntimeValidationError::NotInstalled) => ImageEngineStatus::not_installed(),
        Err(RuntimeValidationError::Invalid) => ImageEngineStatus::invalid(),
    }
}

fn validate_runtime(
    runtime: &ImageRuntime,
    assets: &[RuntimeAsset],
) -> Result<(), RuntimeValidationError> {
    if !runtime.wrapper_path.exists() || !runtime.install_directory.exists() {
        return Err(RuntimeValidationError::NotInstalled);
    }
    validate_directory(&runtime.install_directory)?;
    validate_regular_file(&runtime.wrapper_path, true)?;
    validate_directory(&runtime.install_directory.join("bin"))?;
    validate_directory(&runtime.install_directory.join("models"))?;
    for asset in assets {
        let path = runtime.install_directory.join(asset.relative_path);
        if !path.exists() {
            return Err(RuntimeValidationError::Invalid);
        }
        validate_regular_file(&path, asset.executable)?;
        if sha256_file(&path).map_err(|_| RuntimeValidationError::Invalid)? != asset.sha256 {
            return Err(RuntimeValidationError::Invalid);
        }
    }
    Ok(())
}

fn validate_directory(path: &Path) -> Result<(), RuntimeValidationError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RuntimeValidationError::Invalid)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RuntimeValidationError::Invalid);
    }
    Ok(())
}

fn validate_regular_file(path: &Path, executable: bool) -> Result<(), RuntimeValidationError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RuntimeValidationError::Invalid)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RuntimeValidationError::Invalid);
    }
    #[cfg(unix)]
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(RuntimeValidationError::Invalid);
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn runtime_asset_directory(cache_root: &Path) -> PathBuf {
    cache_root.join(RUNTIME_ASSET_SUBDIRECTORY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use zoos_core::{JobStatus, RunnerLaunchSpec, RunnerRegistry};

    const TEST_ASSETS: [RuntimeAsset; 2] = [
        RuntimeAsset {
            relative_path: "bin/engine",
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            executable: true,
        },
        RuntimeAsset {
            relative_path: "models/model.bin",
            sha256: "486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7",
            executable: false,
        },
    ];

    fn fixture_runtime() -> (tempfile::TempDir, ImageRuntime) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let runtime = ImageRuntime {
            wrapper_path: directory.path().join("wrapper"),
            install_directory: directory.path().join("install"),
        };
        (directory, runtime)
    }

    fn make_ready(runtime: &ImageRuntime) {
        fs::create_dir_all(runtime.install_directory.join("bin")).expect("bin directory");
        fs::create_dir_all(runtime.install_directory.join("models")).expect("models directory");
        fs::write(&runtime.wrapper_path, b"wrapper").expect("wrapper");
        fs::write(runtime.install_directory.join("bin/engine"), b"hello").expect("engine");
        fs::write(runtime.install_directory.join("models/model.bin"), b"world").expect("model");
        #[cfg(unix)]
        for path in [
            runtime.wrapper_path.clone(),
            runtime.install_directory.join("bin/engine"),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("permissions");
        }
    }

    #[test]
    fn status_reports_missing_corrupt_and_ready() {
        let (_directory, runtime) = fixture_runtime();
        let missing = runtime_status(&runtime, &TEST_ASSETS);
        assert_eq!(missing.state, ImageEngineState::NotInstalled);
        assert_eq!(missing.code.as_deref(), Some("ENGINE_NOT_INSTALLED"));
        make_ready(&runtime);
        let ready = runtime_status(&runtime, &TEST_ASSETS);
        assert_eq!(ready.state, ImageEngineState::Ready);
        assert_eq!(ready.engine_version.as_deref(), Some("0.2.5.0"));
        assert_eq!(ready.device.as_deref(), Some("gpu:0"));
        fs::write(
            runtime.install_directory.join("models/model.bin"),
            b"corrupt",
        )
        .expect("corrupt fixture");
        let corrupt = runtime_status(&runtime, &TEST_ASSETS);
        assert_eq!(corrupt.state, ImageEngineState::Invalid);
        assert_eq!(corrupt.code.as_deref(), Some("ASSET_HASH_MISMATCH"));
    }

    #[test]
    fn picker_independent_orchestrator_create_api_builds_an_image_job() {
        let workspace = tempfile::tempdir().expect("workspace");
        let input_directory = tempfile::tempdir().expect("input directory");
        let input = input_directory.path().join("input.png");
        RgbImage::from_pixel(2, 2, Rgb([1, 2, 3]))
            .save_with_format(&input, ImageFormat::Png)
            .expect("input fixture");
        let (_runtime_directory, runtime) = fixture_runtime();
        make_ready(&runtime);
        let image_launch =
            RunnerLaunchSpec::new("zoos-runner-realesrgan", runtime.wrapper_path.clone())
                .expect("absolute runner");
        let registry = RunnerRegistry::with_runner(JobKind::ImageUpscale, image_launch);
        let orchestrator = JobOrchestrator::with_runner_registry(
            workspace.path(),
            registry,
            Duration::from_secs(1),
            Duration::from_millis(50),
        )
        .expect("orchestrator");

        let created =
            create_image_job(&orchestrator, &input, ImagePreset::Photo, 2).expect("image job");
        assert_eq!(created.status, JobStatus::Created);
        assert_eq!(created.kind, JobKind::ImageUpscale);
    }
}
