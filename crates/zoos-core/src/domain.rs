use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zoos_runner_protocol::{FakeBehavior, FakeJobRequest};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    #[default]
    FakeValidation,
    ImageUpscale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePreset {
    Photo,
    Anime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSettings {
    pub preset: ImagePreset,
    pub scale: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobStatus {
    Created,
    Probing,
    Planning,
    Running,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Probing | Self::Planning | Self::Running | Self::Verifying
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobErrorView {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    #[serde(default)]
    pub kind: JobKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_settings: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<FakeBehavior>,
    pub status: JobStatus,
    pub progress_percent: u8,
    pub stage: Option<String>,
    pub message: String,
    pub error: Option<JobErrorView>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProductJobSpec {
    pub schema_version: u32,
    pub job_id: String,
    #[serde(default)]
    pub kind: JobKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_settings: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<FakeBehavior>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobPlan {
    pub schema_version: u32,
    pub job_id: String,
    pub execution_backend: String,
    pub runner_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobProgress {
    pub schema_version: u32,
    #[serde(flatten)]
    pub summary: JobSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobManifest {
    pub schema_version: u32,
    pub job_id: String,
    pub runner_id: String,
    pub runner_version: String,
    pub result: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionRequest {
    pub job_id: String,
    pub runner_job_path: PathBuf,
    pub expected_output_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionReport {
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub(crate) struct StoredJob {
    pub progress: JobProgress,
    pub runner_request: StoredRunnerRequest,
    pub runner_job_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) enum StoredRunnerRequest {
    Fake(FakeJobRequest),
}

impl StoredRunnerRequest {
    pub fn output_path(&self) -> &PathBuf {
        match self {
            Self::Fake(request) => &request.output.path,
        }
    }
}
