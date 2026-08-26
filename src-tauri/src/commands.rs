use serde::Serialize;
use tauri::State;
use zoos_core::{FakeBehavior, JobOrchestrator, JobSummary, OrchestratorError};

#[derive(Debug, Serialize)]
pub struct CommandError {
    code: &'static str,
    message: &'static str,
}

#[tauri::command]
pub async fn create_fake_job(
    orchestrator: State<'_, JobOrchestrator>,
    scenario: FakeBehavior,
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
    job_id: String,
) -> Result<JobSummary, CommandError> {
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
            OrchestratorError::AnotherJobActive => Self {
                code: "JOB_BUSY",
                message: "Another validation job is already running.",
            },
            OrchestratorError::JobNotActive => Self {
                code: "JOB_NOT_ACTIVE",
                message: "This validation job is no longer running.",
            },
            OrchestratorError::InvalidState { .. } => Self {
                code: "INVALID_JOB_STATE",
                message: "This validation job cannot be started from its current state.",
            },
            OrchestratorError::Workspace(_) | OrchestratorError::Backend(_) => Self {
                code: "INTERNAL_ERROR",
                message: "The validation job could not be updated.",
            },
        }
    }
}
