use std::fs::{self, File};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;
use zoos_core::{
    BackendError, ImageBackend, ImageBatchMetadata, ImageOutputFormat, ImagePreset, ImageSettings,
    JobKind, JobOrchestrator, JobStatus, JobSummary, MetadataPolicy, OrchestratorError,
    WorkspaceError,
};

const GPU_RUNTIME_ASSET_SUBDIRECTORY: &str = "realesrgan-ncnn-vulkan-macos/0.2.5.0/macos-universal";
const CPU_RUNTIME_ASSET_SUBDIRECTORY: &str = "onnxruntime-macos-arm64/1.29.0";
const CPU_MODEL_ASSET_SUBDIRECTORY: &str = "realesrgan-onnx/goal1b-v1";

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

const CPU_RUNTIME_ASSETS: [RuntimeAsset; 1] = [RuntimeAsset {
    relative_path: "lib/libonnxruntime.1.29.0.dylib",
    sha256: "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb",
    executable: false,
}];

const CPU_MODEL_ASSETS: [RuntimeAsset; 2] = [
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus-fp32-opset17.onnx",
        sha256: "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b",
        executable: false,
    },
    RuntimeAsset {
        relative_path: "models/realesrgan-x4plus-anime-6b-fp32-opset17.onnx",
        sha256: "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1",
        executable: false,
    },
];

#[derive(Debug, Clone)]
pub struct ImageRuntime {
    pub gpu_wrapper_path: PathBuf,
    pub gpu_install_directory: PathBuf,
    pub cpu_wrapper_path: PathBuf,
    pub cpu_runtime_directory: PathBuf,
    pub cpu_model_directory: PathBuf,
}

impl ImageRuntime {
    pub fn gpu_engine_path(&self) -> PathBuf {
        self.gpu_install_directory
            .join("bin/realesrgan-ncnn-vulkan")
    }

    pub fn gpu_models_path(&self) -> PathBuf {
        self.gpu_install_directory.join("models")
    }

    pub fn cpu_runtime_path(&self) -> PathBuf {
        self.cpu_runtime_directory
            .join("lib/libonnxruntime.1.29.0.dylib")
    }

    pub fn cpu_models_path(&self) -> PathBuf {
        self.cpu_model_directory.join("models")
    }

    pub fn status(&self) -> ImageEngineStatus {
        let gpu = backend_status(
            &self.gpu_wrapper_path,
            &[(&self.gpu_install_directory, &RUNTIME_ASSETS)],
            "0.2.5.0",
            "gpu:0",
        );
        let cpu = backend_status(
            &self.cpu_wrapper_path,
            &[
                (&self.cpu_runtime_directory, &CPU_RUNTIME_ASSETS),
                (&self.cpu_model_directory, &CPU_MODEL_ASSETS),
            ],
            "1.29.0",
            "cpu:0",
        );
        let recommended_backend = if gpu.state == ImageEngineState::Ready {
            Some(ImageBackend::VulkanGpu)
        } else if cpu.state == ImageEngineState::Ready {
            Some(ImageBackend::OrtCpu)
        } else {
            None
        };
        ImageEngineStatus {
            gpu,
            cpu,
            recommended_backend,
        }
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
pub struct BackendEngineStatus {
    pub state: ImageEngineState,
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl BackendEngineStatus {
    fn ready(engine_version: &str, device: &str) -> Self {
        Self {
            state: ImageEngineState::Ready,
            code: None,
            message: "The verified local image engine is ready.".into(),
            engine_version: Some(engine_version.into()),
            device: Some(device.into()),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageEngineStatus {
    pub gpu: BackendEngineStatus,
    pub cpu: BackendEngineStatus,
    pub recommended_backend: Option<ImageBackend>,
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
#[allow(clippy::too_many_arguments)] // Public command parameters intentionally mirror the UI contract.
pub async fn pick_and_create_image_job(
    app: AppHandle,
    orchestrator: State<'_, JobOrchestrator>,
    runtime: State<'_, ImageRuntime>,
    preset: ImagePreset,
    scale: u8,
    backend: ImageBackend,
    output_format: ImageOutputFormat,
    metadata: MetadataPolicy,
) -> Result<Option<JobSummary>, CommandError> {
    let settings = ImageSettings {
        preset,
        scale,
        backend,
        output_format,
        metadata,
    };
    let selected_backend = select_backend(&runtime.status(), backend)?;
    validate_settings(settings)?;
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
    create_image_job(&orchestrator, &input_path, settings, selected_backend, None).map(Some)
}

pub fn create_image_job(
    orchestrator: &JobOrchestrator,
    input_path: &Path,
    settings: ImageSettings,
    selected_backend: ImageBackend,
    batch: Option<ImageBatchMetadata>,
) -> Result<JobSummary, CommandError> {
    orchestrator
        .create_image_job_v2(input_path, settings, selected_backend, batch)
        .map_err(CommandError::from)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchRejectedInput {
    pub input_name: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchCreateResult {
    pub batch_id: String,
    pub jobs: Vec<JobSummary>,
    pub rejected: Vec<BatchRejectedInput>,
}

#[tauri::command]
#[allow(clippy::too_many_arguments)] // Public command parameters intentionally mirror the UI contract.
pub async fn pick_and_create_image_batch(
    app: AppHandle,
    orchestrator: State<'_, JobOrchestrator>,
    runtime: State<'_, ImageRuntime>,
    preset: ImagePreset,
    scale: u8,
    backend: ImageBackend,
    output_format: ImageOutputFormat,
    metadata: MetadataPolicy,
) -> Result<Option<BatchCreateResult>, CommandError> {
    let settings = ImageSettings {
        preset,
        scale,
        backend,
        output_format,
        metadata,
    };
    let selected_backend = select_backend(&runtime.status(), backend)?;
    validate_settings(settings)?;
    let selected = app.dialog().file().blocking_pick_folder();
    let Some(selected) = selected else {
        return Ok(None);
    };
    let directory = selected.into_path().map_err(|_| {
        CommandError::fixed("BATCH_EMPTY", "The selected item is not a local directory.")
    })?;
    create_image_batch(&orchestrator, &directory, settings, selected_backend).map(Some)
}

pub fn create_image_batch(
    orchestrator: &JobOrchestrator,
    directory: &Path,
    settings: ImageSettings,
    selected_backend: ImageBackend,
) -> Result<BatchCreateResult, CommandError> {
    let candidates = batch_candidates(directory)?;
    if candidates.is_empty() {
        return Err(CommandError::fixed(
            "BATCH_EMPTY",
            "The selected folder contains no top-level PNG or JPEG files.",
        ));
    }
    let batch_id = Uuid::new_v4().to_string();
    let total = u32::try_from(candidates.len())
        .map_err(|_| CommandError::fixed("BATCH_TOO_LARGE", "The folder has too many images."))?;
    let mut jobs = Vec::new();
    let mut rejected = Vec::new();
    for (offset, path) in candidates.into_iter().enumerate() {
        let input_name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        let batch = ImageBatchMetadata {
            batch_id: batch_id.clone(),
            index: u32::try_from(offset).expect("candidate count was checked") + 1,
            total,
        };
        match create_image_job(orchestrator, &path, settings, selected_backend, Some(batch)) {
            Ok(job) => jobs.push(job),
            Err(error) => rejected.push(BatchRejectedInput {
                input_name,
                code: error.code,
                message: error.message,
            }),
        }
    }
    Ok(BatchCreateResult {
        batch_id,
        jobs,
        rejected,
    })
}

fn batch_candidates(directory: &Path) -> Result<Vec<PathBuf>, CommandError> {
    let metadata = fs::symlink_metadata(directory).map_err(|_| {
        CommandError::fixed("BATCH_EMPTY", "The selected folder could not be read.")
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CommandError::fixed(
            "BATCH_EMPTY",
            "The selected item is not a safe local directory.",
        ));
    }
    let mut candidates = fs::read_dir(directory)
        .map_err(|_| CommandError::fixed("BATCH_EMPTY", "The selected folder could not be read."))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_file() {
                return None;
            }
            let path = entry.path();
            let extension = path.extension()?.to_str()?;
            if !matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg"
            ) {
                return None;
            }
            Some(path)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(candidates)
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
    if let Some(job) = orchestrator
        .list_jobs()
        .map_err(CommandError::from)?
        .iter()
        .find(|job| job.job_id == job_id && job.kind == JobKind::ImageUpscale)
    {
        let selected_backend = stored_backend(job.selected_backend);
        require_backend(&runtime.status(), selected_backend)?;
    }
    orchestrator
        .start_job(&job_id)
        .await
        .map_err(CommandError::from)
}

fn stored_backend(selected_backend: Option<ImageBackend>) -> ImageBackend {
    // Goal 1A workspaces predate the persisted backend field and always used Vulkan GPU.
    selected_backend.unwrap_or(ImageBackend::VulkanGpu)
}

#[tauri::command]
pub async fn cancel_batch(
    orchestrator: State<'_, JobOrchestrator>,
    batch_id: String,
) -> Result<(), CommandError> {
    cancel_batch_jobs(&orchestrator, &batch_id).await?;
    Ok(())
}

pub async fn cancel_batch_jobs(
    orchestrator: &JobOrchestrator,
    batch_id: &str,
) -> Result<Vec<JobSummary>, CommandError> {
    let matching = orchestrator
        .list_jobs()
        .map_err(CommandError::from)?
        .into_iter()
        .filter(|job| job.batch_id.as_deref() == Some(batch_id))
        .collect::<Vec<_>>();
    let mut results = Vec::with_capacity(matching.len());
    for job in matching {
        results.push(cancel_batch_member(orchestrator, job).await?);
    }
    results.sort_by_key(|job| job.batch_index.unwrap_or(u32::MAX));
    Ok(results)
}

async fn cancel_batch_member(
    orchestrator: &JobOrchestrator,
    mut current: JobSummary,
) -> Result<JobSummary, CommandError> {
    for _ in 0..3 {
        if current.status == JobStatus::Created {
            match orchestrator.cancel_created_job(&current.job_id) {
                Ok(cancelled) => return Ok(cancelled),
                Err(OrchestratorError::InvalidState { .. }) => {}
                Err(error) => return Err(CommandError::from(error)),
            }
        } else if current.status.is_active() {
            match orchestrator.cancel_job(&current.job_id).await {
                Ok(cancelling) => return Ok(cancelling),
                Err(OrchestratorError::JobNotActive) => {}
                Err(error) => return Err(CommandError::from(error)),
            }
        } else {
            return Ok(current);
        }
        current = orchestrator
            .list_jobs()
            .map_err(CommandError::from)?
            .into_iter()
            .find(|job| job.job_id == current.job_id)
            .ok_or_else(|| CommandError::fixed("UPSTREAM_FAILED", "The batch job disappeared."))?;
    }
    Err(CommandError::fixed(
        "UPSTREAM_FAILED",
        "The batch job changed state while cancellation was requested.",
    ))
}

fn validate_settings(settings: ImageSettings) -> Result<(), CommandError> {
    if !matches!(settings.scale, 2 | 4) {
        return Err(CommandError::fixed(
            "UNSUPPORTED_IMAGE_MODE",
            "Scale must be 2 or 4.",
        ));
    }
    Ok(())
}

fn select_backend(
    status: &ImageEngineStatus,
    requested: ImageBackend,
) -> Result<ImageBackend, CommandError> {
    match requested {
        ImageBackend::Auto => status.recommended_backend.ok_or_else(|| {
            if status.gpu.state == ImageEngineState::Invalid
                || status.cpu.state == ImageEngineState::Invalid
            {
                CommandError::fixed(
                    "ASSET_HASH_MISMATCH",
                    "A local image backend cache failed integrity verification.",
                )
            } else {
                CommandError::fixed(
                    "ENGINE_NOT_INSTALLED",
                    "No verified local image backend is installed.",
                )
            }
        }),
        backend @ (ImageBackend::VulkanGpu | ImageBackend::OrtCpu) => {
            require_backend(status, backend)?;
            Ok(backend)
        }
    }
}

fn require_backend(status: &ImageEngineStatus, backend: ImageBackend) -> Result<(), CommandError> {
    match backend {
        ImageBackend::VulkanGpu => status.gpu.clone().into_result(),
        ImageBackend::OrtCpu => status.cpu.clone().into_result(),
        ImageBackend::Auto => Err(CommandError::fixed(
            "UPSTREAM_FAILED",
            "Auto must be resolved before an image job is stored.",
        )),
    }
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
            OrchestratorError::Workspace(WorkspaceError::Pipeline(error)) => Self {
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
                    | "INPUT_CHANGED"
                    | "UNSUPPORTED_IMAGE_MODE"
                    | "OUTPUT_TOO_LARGE"
                    | "INSUFFICIENT_DISK"
                    | "OUTPUT_EXISTS"
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

fn backend_status(
    wrapper_path: &Path,
    asset_roots: &[(&PathBuf, &[RuntimeAsset])],
    version: &str,
    device: &str,
) -> BackendEngineStatus {
    match validate_backend(wrapper_path, asset_roots) {
        Ok(()) => BackendEngineStatus::ready(version, device),
        Err(RuntimeValidationError::NotInstalled) => BackendEngineStatus::not_installed(),
        Err(RuntimeValidationError::Invalid) => BackendEngineStatus::invalid(),
    }
}

fn validate_backend(
    wrapper_path: &Path,
    asset_roots: &[(&PathBuf, &[RuntimeAsset])],
) -> Result<(), RuntimeValidationError> {
    if !wrapper_path.exists() || asset_roots.iter().any(|(root, _)| !root.exists()) {
        return Err(RuntimeValidationError::NotInstalled);
    }
    validate_regular_file(wrapper_path, true)?;
    for (root, assets) in asset_roots {
        validate_directory(root)?;
        for asset in *assets {
            let relative_path = Path::new(asset.relative_path);
            validate_relative_asset_path(root, relative_path)?;
            let path = root.join(relative_path);
            if !path.exists() {
                return Err(RuntimeValidationError::Invalid);
            }
            validate_regular_file(&path, asset.executable)?;
            if sha256_file(&path).map_err(|_| RuntimeValidationError::Invalid)? != asset.sha256 {
                return Err(RuntimeValidationError::Invalid);
            }
        }
    }
    Ok(())
}

fn validate_relative_asset_path(
    root: &Path,
    relative_path: &Path,
) -> Result<(), RuntimeValidationError> {
    let parent = relative_path
        .parent()
        .ok_or(RuntimeValidationError::Invalid)?;
    let mut current = root.to_owned();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Err(RuntimeValidationError::Invalid);
        };
        current.push(component);
        validate_directory(&current)?;
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
    cache_root.join(GPU_RUNTIME_ASSET_SUBDIRECTORY)
}

pub fn cpu_runtime_asset_directory(cache_root: &Path) -> PathBuf {
    cache_root.join(CPU_RUNTIME_ASSET_SUBDIRECTORY)
}

pub fn cpu_model_asset_directory(cache_root: &Path) -> PathBuf {
    cache_root.join(CPU_MODEL_ASSET_SUBDIRECTORY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use zoos_core::{RunnerLaunchSpec, RunnerRegistry};

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

    fn settings(backend: ImageBackend) -> ImageSettings {
        ImageSettings {
            preset: ImagePreset::Photo,
            scale: 2,
            backend,
            output_format: ImageOutputFormat::Png,
            metadata: MetadataPolicy::Preserve,
        }
    }

    fn fixture_orchestrator(root: &Path) -> JobOrchestrator {
        let launch = RunnerLaunchSpec::new(
            "zoos-runner-realesrgan",
            root.join("unused-absolute-runner"),
        )
        .expect("absolute runner");
        let registry = RunnerRegistry::with_runner(JobKind::ImageUpscale, launch);
        JobOrchestrator::with_runner_registry(
            root.join("workspace"),
            registry,
            Duration::from_secs(1),
            Duration::from_millis(50),
        )
        .expect("orchestrator")
    }

    fn status(state: ImageEngineState) -> BackendEngineStatus {
        match state {
            ImageEngineState::Ready => BackendEngineStatus::ready("test", "test"),
            ImageEngineState::NotInstalled => BackendEngineStatus::not_installed(),
            ImageEngineState::Invalid => BackendEngineStatus::invalid(),
        }
    }

    #[test]
    fn backend_status_reports_missing_corrupt_and_ready() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let wrapper = directory.path().join("wrapper");
        let assets = directory.path().join("assets");
        let roots = [(&assets, TEST_ASSETS.as_slice())];
        let missing = backend_status(&wrapper, &roots, "test", "test");
        assert_eq!(missing.state, ImageEngineState::NotInstalled);

        fs::create_dir_all(assets.join("bin")).expect("bin directory");
        fs::create_dir_all(assets.join("models")).expect("models directory");
        fs::write(&wrapper, b"wrapper").expect("wrapper");
        fs::write(assets.join("bin/engine"), b"hello").expect("engine");
        fs::write(assets.join("models/model.bin"), b"world").expect("model");
        #[cfg(unix)]
        for path in [wrapper.clone(), assets.join("bin/engine")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("permissions");
        }
        let ready = backend_status(&wrapper, &roots, "test", "test");
        assert_eq!(ready.state, ImageEngineState::Ready);
        fs::write(assets.join("models/model.bin"), b"corrupt").expect("corrupt fixture");
        let corrupt = backend_status(&wrapper, &roots, "test", "test");
        assert_eq!(corrupt.state, ImageEngineState::Invalid);
    }

    #[test]
    fn auto_prefers_gpu_then_cpu_and_explicit_requires_requested_cache() {
        assert_eq!(stored_backend(None), ImageBackend::VulkanGpu);
        assert_eq!(
            stored_backend(Some(ImageBackend::OrtCpu)),
            ImageBackend::OrtCpu
        );
        let both = ImageEngineStatus {
            gpu: status(ImageEngineState::Ready),
            cpu: status(ImageEngineState::Ready),
            recommended_backend: Some(ImageBackend::VulkanGpu),
        };
        assert_eq!(
            select_backend(&both, ImageBackend::Auto).expect("auto backend"),
            ImageBackend::VulkanGpu
        );
        let cpu_only = ImageEngineStatus {
            gpu: status(ImageEngineState::NotInstalled),
            cpu: status(ImageEngineState::Ready),
            recommended_backend: Some(ImageBackend::OrtCpu),
        };
        assert_eq!(
            select_backend(&cpu_only, ImageBackend::Auto).expect("CPU fallback"),
            ImageBackend::OrtCpu
        );
        assert_eq!(
            select_backend(&cpu_only, ImageBackend::VulkanGpu)
                .expect_err("explicit GPU must fail")
                .code,
            "ENGINE_NOT_INSTALLED"
        );
    }

    #[test]
    fn goal1b_pipeline_rejections_keep_their_public_error_code() {
        let error = CommandError::from(OrchestratorError::Workspace(WorkspaceError::Pipeline(
            zoos_core::Goal1bImageError::AlphaJpegUnsupported,
        )));
        assert_eq!(error.code, "UNSUPPORTED_IMAGE_MODE");
        assert!(error.message.contains("JPEG"));
    }

    #[test]
    fn cpu_cache_contract_uses_the_catalog_hashes() {
        assert_eq!(
            CPU_RUNTIME_ASSETS[0].sha256,
            "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb"
        );
        assert_eq!(
            CPU_MODEL_ASSETS.map(|asset| asset.sha256),
            [
                "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b",
                "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1",
            ]
        );
    }

    #[tokio::test]
    async fn batch_is_sorted_partially_rejected_and_cancelled_before_start() {
        let root = tempfile::tempdir().expect("root");
        let input = root.path().join("input");
        fs::create_dir_all(input.join("nested")).expect("input folders");
        let valid = input.join("b.png");
        RgbImage::from_pixel(2, 2, Rgb([1, 2, 3]))
            .save_with_format(&valid, ImageFormat::Png)
            .expect("input fixture");
        fs::write(input.join("a.png"), b"not an image").expect("bad image");
        fs::write(input.join("ignored.txt"), b"ignored").expect("ignored file");
        RgbImage::from_pixel(2, 2, Rgb([4, 5, 6]))
            .save_with_format(input.join("nested/ignored.jpg"), ImageFormat::Jpeg)
            .expect("nested image");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&valid, input.join("symlink.jpg")).expect("symlink fixture");
        let orchestrator = fixture_orchestrator(root.path());
        let result = create_image_batch(
            &orchestrator,
            &input,
            settings(ImageBackend::Auto),
            ImageBackend::OrtCpu,
        )
        .expect("partial batch");
        assert_eq!(result.jobs.len(), 1);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.rejected[0].input_name, "a.png");
        assert_eq!(result.jobs[0].batch_index, Some(2));
        assert_eq!(result.jobs[0].batch_total, Some(2));
        assert_eq!(result.jobs[0].selected_backend, Some(ImageBackend::OrtCpu));

        let cancelled = cancel_batch_jobs(&orchestrator, &result.batch_id)
            .await
            .expect("cancel batch");
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].status, JobStatus::Cancelled);
        assert_eq!(cancelled[0].stage, None);
        assert_eq!(cancelled[0].error, None);
    }

    #[test]
    fn empty_directory_has_no_batch_candidates() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("notes.txt"), b"no images").expect("fixture");
        assert!(batch_candidates(root.path()).expect("scan").is_empty());
    }
}
