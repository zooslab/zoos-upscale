use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex, mpsc, watch};
use zoos_runner_protocol::{
    FakeBehavior, ImageDeviceV2, ImageModelId, ImageSemanticModelV2, RunnerCapabilities,
    RunnerEvent, RunnerEventPayload, RunnerTask,
};

use crate::domain::{
    ExecutionRequest, ImageBackend, ImageBatchMetadata, ImageOutputFormat, ImagePreset,
    ImageSettings, JobErrorView, JobKind, JobStatus, JobSummary, MetadataPolicy,
    StoredRunnerRequest,
};
use crate::process::{
    BackendError, ExecutionBackend, ProcessExecutionBackend, RunnerLaunchSpec, RunnerRegistry,
};
use crate::workspace::{WorkspaceError, WorkspaceStore, now_ms};

#[derive(Clone)]
pub struct JobOrchestrator {
    inner: Arc<Inner>,
}

struct Inner {
    store: WorkspaceStore,
    backend: Arc<dyn ExecutionBackend>,
    runners: RunnerRegistry,
    job_creation: StdMutex<()>,
    active_jobs: Mutex<HashMap<String, watch::Sender<bool>>>,
    progress_updates: Mutex<()>,
}

impl JobOrchestrator {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        runner_path: PathBuf,
        activity_timeout: Duration,
        termination_grace: Duration,
    ) -> Result<Self, OrchestratorError> {
        let runners = RunnerRegistry::with_runner(
            JobKind::FakeValidation,
            RunnerLaunchSpec::new("zoos-runner-fake", runner_path)?,
        );
        Self::with_runner_registry(workspace_root, runners, activity_timeout, termination_grace)
    }

    pub fn with_runner_registry(
        workspace_root: impl AsRef<Path>,
        runners: RunnerRegistry,
        activity_timeout: Duration,
        termination_grace: Duration,
    ) -> Result<Self, OrchestratorError> {
        let backend = ProcessExecutionBackend::new(activity_timeout, termination_grace);
        let store = WorkspaceStore::new(workspace_root)?;
        store.recover_interrupted()?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                backend: Arc::new(backend),
                runners,
                job_creation: StdMutex::new(()),
                active_jobs: Mutex::new(HashMap::new()),
                progress_updates: Mutex::new(()),
            }),
        })
    }

    pub fn create_fake_job(&self, behavior: FakeBehavior) -> Result<JobSummary, OrchestratorError> {
        Ok(self.inner.store.create_fake_job(behavior)?)
    }

    pub fn create_image_job(
        &self,
        input_path: impl AsRef<Path>,
        preset: ImagePreset,
        scale: u8,
    ) -> Result<JobSummary, OrchestratorError> {
        Ok(self.inner.store.create_image_job(
            input_path.as_ref(),
            ImageSettings {
                preset,
                scale,
                backend: ImageBackend::Auto,
                output_format: ImageOutputFormat::Png,
                metadata: MetadataPolicy::Preserve,
            },
        )?)
    }

    pub fn create_image_job_v2(
        &self,
        input_path: impl AsRef<Path>,
        settings: ImageSettings,
        selected_backend: ImageBackend,
        batch: Option<ImageBatchMetadata>,
    ) -> Result<JobSummary, OrchestratorError> {
        // Planning a no-replace destination and publishing the workspace must be one
        // process-local critical section so concurrent picker commands cannot reserve
        // the same filename.
        let _creation_guard = self
            .inner
            .job_creation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self.inner.store.create_image_job_v2(
            input_path.as_ref(),
            settings,
            selected_backend,
            batch,
        )?)
    }

    pub fn list_jobs(&self) -> Result<Vec<JobSummary>, OrchestratorError> {
        Ok(self.inner.store.list_jobs()?)
    }

    pub async fn start_job(&self, job_id: &str) -> Result<JobSummary, OrchestratorError> {
        let stored = self.inner.store.load_stored_job(job_id)?;
        if stored.progress.summary.status != JobStatus::Created {
            return Err(OrchestratorError::InvalidState {
                expected: JobStatus::Created,
                actual: stored.progress.summary.status,
            });
        }

        let mut active_jobs = self.inner.active_jobs.lock().await;
        if !active_jobs.is_empty() {
            return Err(OrchestratorError::AnotherJobActive);
        }

        self.inner.runners.resolve(&stored.runner_id)?;

        let _progress_guard = self.inner.progress_updates.lock().await;
        let probing = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Probing;
            summary.stage = Some("probe".into());
            summary.message = "Checking the local validation engine".into();
            summary.error = None;
        })?;

        let (cancel_sender, cancel_receiver) = watch::channel(false);
        active_jobs.insert(job_id.to_owned(), cancel_sender.clone());
        drop(active_jobs);

        let orchestrator = self.clone();
        let job_id = job_id.to_owned();
        tokio::spawn(async move {
            orchestrator
                .execute_job(job_id.clone(), cancel_sender, cancel_receiver)
                .await;
            orchestrator.inner.active_jobs.lock().await.remove(&job_id);
        });

        Ok(probing)
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<JobSummary, OrchestratorError> {
        let cancellation = self
            .inner
            .active_jobs
            .lock()
            .await
            .get(job_id)
            .cloned()
            .ok_or(OrchestratorError::JobNotActive)?;
        let _progress_guard = self.inner.progress_updates.lock().await;
        let current = self.inner.store.load_summary(job_id)?;
        if !current.status.is_active() {
            return Err(OrchestratorError::JobNotActive);
        }
        let cancelling = self.inner.store.update_summary(job_id, |summary| {
            summary.stage = Some("cancelling".into());
            summary.message = "Stopping the validation engine".into();
        })?;
        cancellation
            .send(true)
            .map_err(|_| OrchestratorError::JobNotActive)?;
        Ok(cancelling)
    }

    /// Cancels a job that has been created but has not started yet.
    ///
    /// Active jobs must continue to use [`Self::cancel_job`] so the runner process tree is
    /// terminated before the workspace reaches a terminal state.
    pub fn cancel_created_job(&self, job_id: &str) -> Result<JobSummary, OrchestratorError> {
        let current = self.inner.store.load_summary(job_id)?;
        if current.status != JobStatus::Created {
            return Err(OrchestratorError::InvalidState {
                expected: JobStatus::Created,
                actual: current.status,
            });
        }
        self.inner.store.cleanup_unverified_output(job_id)?;
        let cancelled = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Cancelled;
            summary.progress_percent = 0;
            summary.stage = None;
            summary.message = "Cancelled before execution".into();
            summary.error = None;
        })?;
        self.inner
            .store
            .finish_unstarted_manifest(job_id, "cancelled_before_start")?;
        Ok(cancelled)
    }

    async fn execute_job(
        &self,
        job_id: String,
        cancel_sender: watch::Sender<bool>,
        cancel_receiver: watch::Receiver<bool>,
    ) {
        let started_at_ms = now_ms();
        let stored = match self.inner.store.load_stored_job(&job_id) {
            Ok(stored) => stored,
            Err(error) => {
                self.finish_with_internal_error(&job_id, error.to_string(), started_at_ms)
                    .await;
                return;
            }
        };
        let launch = match self.inner.runners.resolve(&stored.runner_id) {
            Ok(launch) => launch.clone(),
            Err(error) => {
                self.finish_with_internal_error(&job_id, error.to_string(), started_at_ms)
                    .await;
                return;
            }
        };
        if let Err(error) = self.inner.store.recheck_image_input(&job_id) {
            self.finish_with_workspace_error(&job_id, &error, started_at_ms)
                .await;
            return;
        }
        if let Err(error) = self
            .inner
            .backend
            .probe(&launch)
            .await
            .and_then(|capabilities| validate_capabilities(&stored, &capabilities, &launch))
        {
            let _progress_guard = self.inner.progress_updates.lock().await;
            if let Err(reporting_error) = self.finalize_failed(&job_id, &error, started_at_ms) {
                eprintln!("could not persist probe failure for job {job_id}: {reporting_error}");
            }
            return;
        }
        if *cancel_receiver.borrow() {
            let _progress_guard = self.inner.progress_updates.lock().await;
            if let Err(error) = self.finalize_cancelled(&job_id, started_at_ms) {
                eprintln!("could not persist cancellation for job {job_id}: {error}");
            }
            return;
        }
        {
            let _progress_guard = self.inner.progress_updates.lock().await;
            if let Err(error) = self.inner.store.update_summary(&job_id, |summary| {
                summary.status = JobStatus::Planning;
                summary.stage = Some("plan".into());
                summary.message = "Preparing a safe execution plan".into();
            }) {
                self.finish_with_internal_error_locked(&job_id, error.to_string(), started_at_ms)
                    .ok();
                return;
            }
            if let Err(error) = self.inner.store.update_summary(&job_id, |summary| {
                summary.status = JobStatus::Running;
                summary.stage = Some("starting".into());
                summary.message = "Starting the local validation engine".into();
            }) {
                self.finish_with_internal_error_locked(&job_id, error.to_string(), started_at_ms)
                    .ok();
                return;
            }
        }
        let request = ExecutionRequest {
            job_id: job_id.clone(),
            runner_job_path: stored.runner_job_path,
            expected_output_path: stored.runner_request.output_path().clone(),
        };

        let (event_sender, mut event_receiver) = mpsc::channel(32);
        let backend = Arc::clone(&self.inner.backend);
        let backend_task = tokio::spawn(async move {
            backend
                .execute(&launch, request, event_sender, cancel_receiver)
                .await
        });

        let mut workspace_failed = None;
        while let Some(event) = event_receiver.recv().await {
            if let Err(error) = self.apply_event(&job_id, &event).await {
                workspace_failed = Some(error.to_string());
                let _ = cancel_sender.send(true);
                break;
            }
        }

        let result = backend_task.await;
        if let Some(message) = workspace_failed {
            self.finish_with_internal_error(&job_id, message, started_at_ms)
                .await;
            return;
        }

        let _progress_guard = self.inner.progress_updates.lock().await;
        let finalization = match result {
            Ok(Ok(report)) => self.finalize_completed(&job_id, report.exit_code, started_at_ms),
            Ok(Err(BackendError::Cancelled)) => self.finalize_cancelled(&job_id, started_at_ms),
            Ok(Err(error)) => self.finalize_failed(&job_id, &error, started_at_ms),
            Err(error) => {
                self.finish_with_internal_error_locked(&job_id, error.to_string(), started_at_ms)
            }
        };
        if let Err(error) = finalization
            && let Err(reporting_error) =
                self.finish_with_workspace_error_locked(&job_id, &error, started_at_ms)
        {
            eprintln!("could not persist terminal state for job {job_id}: {reporting_error}");
        }
    }

    async fn apply_event(&self, job_id: &str, event: &RunnerEvent) -> Result<(), WorkspaceError> {
        let _progress_guard = self.inner.progress_updates.lock().await;
        self.inner.store.append_event(job_id, event)?;
        if let RunnerEventPayload::Warning { code, message } = &event.payload {
            self.inner
                .store
                .record_runner_device(job_id, code, message)?;
        }
        self.inner
            .store
            .update_summary(job_id, |summary| match &event.payload {
                RunnerEventPayload::Started { stage } => {
                    summary.status = JobStatus::Running;
                    summary.stage = Some(stage.clone());
                    summary.message = "Validation engine started".into();
                }
                RunnerEventPayload::Progress {
                    stage,
                    completed_units,
                    total_units,
                    ..
                } => {
                    summary.status = JobStatus::Running;
                    summary.stage = Some(stage.clone());
                    let percent = completed_units.saturating_mul(100) / total_units;
                    summary.progress_percent = u8::try_from(percent.min(99)).unwrap_or(99);
                    summary.message = "Running local validation".into();
                }
                RunnerEventPayload::Warning { message, .. } => {
                    summary.message = message.clone();
                }
                RunnerEventPayload::Completed { .. } => {
                    summary.status = JobStatus::Verifying;
                    summary.progress_percent = 99;
                    summary.stage = Some("verify".into());
                    summary.message = "Verifying the output".into();
                }
                RunnerEventPayload::Failed { message, .. } => {
                    summary.message = message.clone();
                }
            })?;
        Ok(())
    }

    async fn finish_with_internal_error(&self, job_id: &str, detail: String, started_at_ms: u64) {
        let _progress_guard = self.inner.progress_updates.lock().await;
        if let Err(error) = self.finish_with_internal_error_locked(job_id, detail, started_at_ms) {
            eprintln!("could not persist internal failure for job {job_id}: {error}");
        }
    }

    async fn finish_with_workspace_error(
        &self,
        job_id: &str,
        error: &WorkspaceError,
        started_at_ms: u64,
    ) {
        let _progress_guard = self.inner.progress_updates.lock().await;
        if let Err(reporting_error) =
            self.finish_with_workspace_error_locked(job_id, error, started_at_ms)
        {
            eprintln!("could not persist image safety failure for job {job_id}: {reporting_error}");
        }
    }

    fn finish_with_workspace_error_locked(
        &self,
        job_id: &str,
        error: &WorkspaceError,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        let (code, message) = match error {
            WorkspaceError::Image(image_error) => {
                (image_error.code().to_owned(), image_error.to_string())
            }
            WorkspaceError::Pipeline(image_error) => {
                (image_error.code().to_owned(), image_error.to_string())
            }
            _ => {
                return self.finish_with_internal_error_locked(
                    job_id,
                    error.to_string(),
                    started_at_ms,
                );
            }
        };
        let cleanup = self.inner.store.cleanup_unverified_output(job_id);
        let progress = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Failed;
            summary.stage = None;
            summary.message = "Image upscale failed".into();
            summary.error = Some(JobErrorView { code, message });
        });
        let manifest = self.inner.store.finish_manifest(
            job_id,
            &format!("image_error: {error}"),
            None,
            started_at_ms,
        );
        cleanup?;
        progress?;
        manifest
    }

    fn finalize_completed(
        &self,
        job_id: &str,
        exit_code: Option<i32>,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        self.inner.store.publish_image_output(job_id)?;
        self.inner
            .store
            .finish_manifest(job_id, "completed", exit_code, started_at_ms)?;
        self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Completed;
            summary.progress_percent = 100;
            summary.stage = None;
            summary.message = "Validation completed successfully".into();
            summary.error = None;
        })?;
        Ok(())
    }

    fn finalize_cancelled(&self, job_id: &str, started_at_ms: u64) -> Result<(), WorkspaceError> {
        let cleanup = self.inner.store.cleanup_unverified_output(job_id);
        let progress = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Cancelled;
            summary.stage = None;
            summary.message = "Validation cancelled".into();
            summary.error = None;
        });
        let manifest = self
            .inner
            .store
            .finish_manifest(job_id, "cancelled", None, started_at_ms);
        cleanup?;
        progress?;
        manifest
    }

    fn finalize_failed(
        &self,
        job_id: &str,
        error: &BackendError,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        let cleanup = self.inner.store.cleanup_unverified_output(job_id);
        let kind = self.inner.store.load_summary(job_id)?.kind;
        let code = match kind {
            JobKind::FakeValidation => error.code(),
            JobKind::ImageUpscale => image_backend_error_code(error),
        }
        .to_owned();
        let message = error.user_message();
        let progress = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Failed;
            summary.stage = None;
            summary.message = "Validation failed".into();
            summary.error = Some(JobErrorView { code, message });
        });
        let manifest =
            self.inner
                .store
                .finish_manifest(job_id, "failed", error.exit_code(), started_at_ms);
        cleanup?;
        progress?;
        manifest
    }

    fn finish_with_internal_error_locked(
        &self,
        job_id: &str,
        detail: String,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        let cleanup = self.inner.store.cleanup_unverified_output(job_id);
        let image_job = self.inner.store.load_summary(job_id)?.kind == JobKind::ImageUpscale;
        let progress = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Failed;
            summary.stage = None;
            summary.message = "Validation failed".into();
            summary.error = Some(JobErrorView {
                code: if image_job {
                    "UPSTREAM_FAILED"
                } else {
                    "INTERNAL_ERROR"
                }
                .into(),
                message: if image_job {
                    "The local image engine failed unexpectedly."
                } else {
                    "An internal error interrupted the validation run."
                }
                .into(),
            });
        });
        let manifest = self.inner.store.finish_manifest(
            job_id,
            &format!("internal_error: {detail}"),
            None,
            started_at_ms,
        );
        cleanup?;
        progress?;
        manifest
    }
}

fn image_backend_error_code(error: &BackendError) -> &str {
    match error {
        BackendError::InvalidRunnerPath
        | BackendError::RunnerNotRegistered(_)
        | BackendError::SpawnFailed(_) => "ENGINE_NOT_INSTALLED",
        BackendError::ProbeFailed(_) => "ASSET_HASH_MISMATCH",
        BackendError::RunnerFailed { error_code, .. }
            if matches!(
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
            error_code
        }
        BackendError::Cancelled => "CANCELLED",
        _ => "UPSTREAM_FAILED",
    }
}

fn validate_capabilities(
    stored: &crate::domain::StoredJob,
    capabilities: &RunnerCapabilities,
    launch: &RunnerLaunchSpec,
) -> Result<(), BackendError> {
    let expected_task = match stored.progress.summary.kind {
        JobKind::FakeValidation => RunnerTask::FakeValidation,
        JobKind::ImageUpscale => RunnerTask::ImageUpscale,
    };
    if !capabilities.tasks.contains(&expected_task) {
        return Err(BackendError::ProbeFailed(format!(
            "runner {} does not support {expected_task:?}",
            launch.runner_id
        )));
    }

    let (model_ids, scale, device_backend, device_index): (&[&str], u8, &[&str], Option<u32>) =
        match &stored.runner_request {
            StoredRunnerRequest::ImageUpscale(request) => {
                let model_ids: &[&str] = match request.parameters.model_id {
                    ImageModelId::RealEsrganX4plus => &["realesrgan-x4plus"],
                    ImageModelId::RealEsrganX4plusAnime => &["realesrgan-x4plus-anime"],
                };
                (
                    model_ids,
                    request.parameters.scale,
                    &["vulkan"],
                    Some(request.parameters.gpu_id),
                )
            }
            StoredRunnerRequest::ImageUpscaleV2(request) => {
                let model_ids: &[&str] = match request.parameters.semantic_model {
                    ImageSemanticModelV2::Photo => &["photo", "realesrgan-x4plus"],
                    ImageSemanticModelV2::Anime => &["anime", "realesrgan-x4plus-anime"],
                };
                let (backends, index): (&[&str], Option<u32>) = match request.parameters.device {
                    ImageDeviceV2::Vulkan { index } => (&["vulkan"], Some(index)),
                    ImageDeviceV2::Cpu => (&["cpu", "ort_cpu", "onnxruntime"], None),
                };
                (model_ids, request.parameters.native_scale, backends, index)
            }
            StoredRunnerRequest::Fake(_) => return Ok(()),
        };
    let model_supported = capabilities
        .models
        .iter()
        .any(|model| model_ids.contains(&model.id.as_str()) && model.scales.contains(&scale));
    let scale_supported = capabilities.scales.contains(&scale);
    let device_supported = capabilities.devices.iter().any(|device| {
        device_backend
            .iter()
            .any(|backend| device.backend.eq_ignore_ascii_case(backend))
            && device_index.is_none_or(|index| device.index == index)
    });
    if !model_supported || !scale_supported || !device_supported {
        return Err(BackendError::ProbeFailed(format!(
            "runner {} does not support models {}, scale {scale}, backend {}, device {:?}",
            launch.runner_id,
            model_ids.join("/"),
            device_backend.join("/"),
            device_index
        )));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("another job is already active")]
    AnotherJobActive,
    #[error("job is not active")]
    JobNotActive,
    #[error("job state must be {expected:?}, but was {actual:?}")]
    InvalidState {
        expected: JobStatus,
        actual: JobStatus,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use zoos_runner_protocol::{DeviceCapability, ModelCapability, UpstreamInfo};

    #[test]
    fn image_safety_failure_keeps_its_structured_error_code() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("input.png");
        RgbImage::from_pixel(2, 2, Rgb([1, 2, 3]))
            .save_with_format(&input, ImageFormat::Png)
            .expect("input fixture must save");
        let orchestrator = JobOrchestrator::new(
            directory.path(),
            directory.path().join("missing-runner"),
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("orchestrator must be created");
        let job = orchestrator
            .create_image_job(&input, ImagePreset::Photo, 2)
            .expect("image job must be created");

        orchestrator
            .finish_with_workspace_error_locked(
                &job.job_id,
                &WorkspaceError::Image(crate::ImageSafetyError::InputChanged),
                now_ms(),
            )
            .expect("image failure must persist");

        let failed = orchestrator
            .list_jobs()
            .expect("jobs must list")
            .into_iter()
            .next()
            .expect("job must remain");
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(
            failed.error.expect("error must be structured").code,
            "INPUT_CHANGED"
        );
    }

    #[test]
    fn image_probe_requires_the_planned_model_scale_and_vulkan_device() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("input.png");
        RgbImage::from_pixel(2, 2, Rgb([1, 2, 3]))
            .save_with_format(&input, ImageFormat::Png)
            .expect("input fixture must save");
        let orchestrator = JobOrchestrator::new(
            directory.path(),
            directory.path().join("fake-runner"),
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("orchestrator must be created");
        let job = orchestrator
            .create_image_job(&input, ImagePreset::Photo, 2)
            .expect("image job must be created");
        let stored = orchestrator
            .inner
            .store
            .load_stored_job(&job.job_id)
            .expect("stored image job must load");
        let launch = RunnerLaunchSpec::new(
            "zoos-runner-realesrgan",
            directory.path().join("image-runner"),
        )
        .expect("absolute launch path must validate");
        let mut capabilities = RunnerCapabilities {
            protocol_version: 1,
            runner_id: "zoos-runner-realesrgan".into(),
            runner_version: "0.1.0".into(),
            tasks: vec![RunnerTask::ImageUpscale],
            upstream: Some(UpstreamInfo {
                name: "Real-ESRGAN-ncnn-vulkan".into(),
                version: "0.2.0".into(),
                source_commit: None,
            }),
            models: vec![ModelCapability {
                id: "realesrgan-x4plus".into(),
                scales: vec![2, 4],
            }],
            scales: vec![2, 4],
            devices: vec![DeviceCapability {
                index: 0,
                name: "Apple M5".into(),
                backend: "vulkan".into(),
            }],
            test_behaviors: Vec::new(),
        };

        validate_capabilities(&stored, &capabilities, &launch)
            .expect("matching capabilities must pass");
        capabilities.devices.clear();
        assert!(matches!(
            validate_capabilities(&stored, &capabilities, &launch),
            Err(BackendError::ProbeFailed(_))
        ));
    }

    #[test]
    fn image_backend_failures_map_to_the_public_error_contract() {
        assert_eq!(
            image_backend_error_code(&BackendError::SpawnFailed("missing".into())),
            "ENGINE_NOT_INSTALLED"
        );
        assert_eq!(
            image_backend_error_code(&BackendError::ProtocolViolation("bad event".into())),
            "UPSTREAM_FAILED"
        );
        assert_eq!(
            image_backend_error_code(&BackendError::RunnerFailed {
                error_code: "GPU_UNAVAILABLE".into(),
                message: "no device".into(),
                exit_code: Some(30),
            }),
            "GPU_UNAVAILABLE"
        );
        assert_eq!(
            image_backend_error_code(&BackendError::RunnerFailed {
                error_code: "INPUT_CHANGED".into(),
                message: "source changed".into(),
                exit_code: Some(10),
            }),
            "INPUT_CHANGED"
        );
    }

    #[test]
    fn goal1b_public_create_api_selects_backend_and_batch_metadata() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("input.png");
        RgbImage::from_pixel(2, 3, Rgb([1, 2, 3]))
            .save_with_format(&input, ImageFormat::Png)
            .expect("input fixture must save");
        let orchestrator = JobOrchestrator::new(
            directory.path(),
            directory.path().join("unused-runner"),
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("orchestrator must be created");

        let created = orchestrator
            .create_image_job_v2(
                &input,
                ImageSettings {
                    preset: ImagePreset::Anime,
                    scale: 2,
                    backend: ImageBackend::Auto,
                    output_format: ImageOutputFormat::Webp,
                    metadata: MetadataPolicy::Strip,
                },
                ImageBackend::OrtCpu,
                Some(ImageBatchMetadata {
                    batch_id: "batch-1".into(),
                    index: 1,
                    total: 2,
                }),
            )
            .expect("Goal 1B job must create");

        assert_eq!(created.selected_backend, Some(ImageBackend::OrtCpu));
        assert_eq!(created.batch_id.as_deref(), Some("batch-1"));
        assert_eq!(created.batch_index, Some(1));
        assert_eq!(created.batch_total, Some(2));
        assert_eq!(
            created
                .output_path
                .as_deref()
                .and_then(Path::extension)
                .and_then(|value| value.to_str()),
            Some("webp")
        );
        assert_eq!(
            orchestrator
                .inner
                .store
                .load_stored_job(&created.job_id)
                .expect("job must load")
                .runner_id,
            "zoos-runner-ort"
        );

        let stored = orchestrator
            .inner
            .store
            .load_stored_job(&created.job_id)
            .expect("Goal 1B job must load");
        let capabilities = RunnerCapabilities {
            protocol_version: 1,
            runner_id: "zoos-runner-ort".into(),
            runner_version: "0.1.0".into(),
            tasks: vec![RunnerTask::ImageUpscale],
            upstream: Some(UpstreamInfo {
                name: "ONNX Runtime".into(),
                version: "test".into(),
                source_commit: None,
            }),
            models: vec![ModelCapability {
                id: "anime".into(),
                scales: vec![4],
            }],
            scales: vec![4],
            devices: vec![DeviceCapability {
                index: 0,
                name: "CPU".into(),
                backend: "ort_cpu".into(),
            }],
            test_behaviors: Vec::new(),
        };
        let launch = RunnerLaunchSpec::new("zoos-runner-ort", directory.path().join("ort-runner"))
            .expect("launch must validate");
        validate_capabilities(&stored, &capabilities, &launch)
            .expect("native x4 CPU capability must satisfy an x2 Goal 1B request");

        let cancelled = orchestrator
            .cancel_created_job(&created.job_id)
            .expect("a pending batch member must cancel without starting a runner");
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(cancelled.stage, None);
        assert_eq!(cancelled.error, None);
        let manifest: crate::domain::JobManifest = serde_json::from_slice(
            &std::fs::read(directory.path().join(&created.job_id).join("manifest.json"))
                .expect("manifest must remain readable"),
        )
        .expect("manifest must remain valid");
        assert_eq!(manifest.result.as_deref(), Some("cancelled_before_start"));
        assert_eq!(manifest.started_at_ms, None);
        assert!(manifest.finished_at_ms.is_some());
        assert_eq!(
            std::fs::read_dir(directory.path().join(&created.job_id).join("work"))
                .expect("cancelled work directory must remain readable")
                .count(),
            0,
            "cancelling a pending batch member must remove normalized image data"
        );
        assert!(matches!(
            orchestrator.cancel_created_job(&created.job_id),
            Err(OrchestratorError::InvalidState { .. })
        ));
    }

    #[tokio::test]
    async fn missing_runner_becomes_a_structured_failure() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let missing_runner = directory.path().join("missing-runner");
        let orchestrator = JobOrchestrator::new(
            directory.path(),
            missing_runner,
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .expect("orchestrator must be created");
        let job = orchestrator
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");

        orchestrator
            .start_job(&job.job_id)
            .await
            .expect("job must start asynchronously");

        let failed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let current = orchestrator.list_jobs().expect("jobs must load").remove(0);
                if current.status == JobStatus::Failed {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("job must fail promptly");

        assert_eq!(failed.error.expect("structured error").code, "SPAWN_FAILED");
    }
}
