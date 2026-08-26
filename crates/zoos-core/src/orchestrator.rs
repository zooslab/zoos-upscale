use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex, mpsc, watch};
use zoos_runner_protocol::{FakeBehavior, RunnerEvent, RunnerEventPayload, RunnerTask};

use crate::domain::{ExecutionRequest, JobErrorView, JobKind, JobStatus, JobSummary};
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
        let backend = ProcessExecutionBackend::new(activity_timeout, termination_grace);
        let runners = RunnerRegistry::with_runner(
            JobKind::FakeValidation,
            RunnerLaunchSpec::new("zoos-runner-fake", runner_path)?,
        );
        let store = WorkspaceStore::new(workspace_root)?;
        store.recover_interrupted()?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                backend: Arc::new(backend),
                runners,
                active_jobs: Mutex::new(HashMap::new()),
                progress_updates: Mutex::new(()),
            }),
        })
    }

    pub fn create_fake_job(&self, behavior: FakeBehavior) -> Result<JobSummary, OrchestratorError> {
        Ok(self.inner.store.create_fake_job(behavior)?)
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

        self.inner.runners.resolve(stored.progress.summary.kind)?;

        let progress_guard = self.inner.progress_updates.lock().await;
        self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Probing;
            summary.stage = Some("probe".into());
            summary.message = "Checking the local validation engine".into();
            summary.error = None;
        })?;
        self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Planning;
            summary.stage = Some("plan".into());
            summary.message = "Preparing a safe execution plan".into();
        })?;
        let running = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Running;
            summary.stage = Some("starting".into());
            summary.message = "Starting the local validation engine".into();
        })?;
        drop(progress_guard);

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

        Ok(running)
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
        let launch = match self.inner.runners.resolve(stored.progress.summary.kind) {
            Ok(launch) => launch.clone(),
            Err(error) => {
                self.finish_with_internal_error(&job_id, error.to_string(), started_at_ms)
                    .await;
                return;
            }
        };
        let expected_task = match stored.progress.summary.kind {
            JobKind::FakeValidation => RunnerTask::FakeValidation,
            JobKind::ImageUpscale => RunnerTask::ImageUpscale,
        };
        if let Err(error) = self
            .inner
            .backend
            .probe(&launch)
            .await
            .and_then(|capabilities| {
                capabilities
                    .tasks
                    .contains(&expected_task)
                    .then_some(())
                    .ok_or_else(|| {
                        BackendError::ProbeFailed(format!(
                            "runner {} does not support {expected_task:?}",
                            launch.runner_id
                        ))
                    })
            })
        {
            let _progress_guard = self.inner.progress_updates.lock().await;
            if let Err(reporting_error) = self.finalize_failed(&job_id, &error, started_at_ms) {
                eprintln!("could not persist probe failure for job {job_id}: {reporting_error}");
            }
            return;
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
                self.finish_with_internal_error_locked(&job_id, error.to_string(), started_at_ms)
        {
            eprintln!("could not persist terminal state for job {job_id}: {reporting_error}");
        }
    }

    async fn apply_event(&self, job_id: &str, event: &RunnerEvent) -> Result<(), WorkspaceError> {
        let _progress_guard = self.inner.progress_updates.lock().await;
        self.inner.store.append_event(job_id, event)?;
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

    fn finalize_completed(
        &self,
        job_id: &str,
        exit_code: Option<i32>,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Completed;
            summary.progress_percent = 100;
            summary.stage = None;
            summary.message = "Validation completed successfully".into();
            summary.error = None;
        })?;
        self.inner
            .store
            .finish_manifest(job_id, "completed", exit_code, started_at_ms)
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
        let code = error.code().to_owned();
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
        let progress = self.inner.store.update_summary(job_id, |summary| {
            summary.status = JobStatus::Failed;
            summary.stage = None;
            summary.message = "Validation failed".into();
            summary.error = Some(JobErrorView {
                code: "INTERNAL_ERROR".into(),
                message: "An internal error interrupted the validation run.".into(),
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
