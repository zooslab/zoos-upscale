use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, ImageUpscaleJobRequest, ImageUpscaleJobRequestV2,
    VideoInterpolateJobRequest,
};
pub use zoos_runner_protocol::{RationalRate, VideoContainer};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    #[default]
    FakeValidation,
    ImageUpscale,
    VideoInterpolate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoBackend {
    #[default]
    Auto,
    VulkanGpu,
    NcnnCpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSettings {
    #[serde(default)]
    pub backend: VideoBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePreset {
    Photo,
    Anime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBackend {
    #[default]
    Auto,
    VulkanGpu,
    OrtCpu,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageOutputFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataPolicy {
    #[default]
    Preserve,
    Strip,
}

pub const JPEG_OUTPUT_QUALITY: u8 = 95;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBatchMetadata {
    pub batch_id: String,
    pub index: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSettings {
    pub preset: ImagePreset,
    pub scale: u8,
    #[serde(default)]
    pub backend: ImageBackend,
    #[serde(default)]
    pub output_format: ImageOutputFormat,
    #[serde(default)]
    pub metadata: MetadataPolicy,
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
    pub video_settings: Option<VideoSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rate: Option<RationalRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_rate: Option<RationalRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_container: Option<VideoContainer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_backend: Option<ImageBackend>,
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
    pub video_settings: Option<VideoSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rate: Option<RationalRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_rate: Option<RationalRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_container: Option<VideoContainer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_backend: Option<ImageBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<FakeBehavior>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobPlan {
    pub schema_version: u32,
    pub job_id: String,
    pub execution_backend: String,
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_backend: Option<ImageBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_param_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_bin_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_onnx_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intermediate_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icc_preserved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exif_preserved: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha_preserved: Option<bool>,
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
    pub runner_id: String,
}

#[derive(Debug, Clone)]
pub(crate) enum StoredRunnerRequest {
    Fake(FakeJobRequest),
    ImageUpscale(ImageUpscaleJobRequest),
    ImageUpscaleV2(ImageUpscaleJobRequestV2),
    VideoInterpolate(VideoInterpolateJobRequest),
}

impl StoredRunnerRequest {
    pub fn output_path(&self) -> &PathBuf {
        match self {
            Self::Fake(request) => &request.output.path,
            Self::ImageUpscale(request) => &request.output.path,
            Self::ImageUpscaleV2(request) => &request.output.path,
            Self::VideoInterpolate(request) => &request.output.path,
        }
    }

    pub fn expected_task(&self) -> zoos_runner_protocol::RunnerTask {
        match self {
            Self::Fake(_) => zoos_runner_protocol::RunnerTask::FakeValidation,
            Self::ImageUpscale(_) | Self::ImageUpscaleV2(_) => {
                zoos_runner_protocol::RunnerTask::ImageUpscale
            }
            Self::VideoInterpolate(_) => zoos_runner_protocol::RunnerTask::VideoInterpolate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_image_settings_receive_goal_1b_defaults() {
        let settings: ImageSettings = serde_json::from_value(serde_json::json!({
            "preset": "photo",
            "scale": 2
        }))
        .expect("Goal 1A settings must remain readable");

        assert_eq!(settings.backend, ImageBackend::Auto);
        assert_eq!(settings.output_format, ImageOutputFormat::Png);
        assert_eq!(settings.metadata, MetadataPolicy::Preserve);
        assert_eq!(JPEG_OUTPUT_QUALITY, 95);
    }

    #[test]
    fn legacy_manifest_receives_empty_execution_evidence() {
        let manifest: JobManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "job_id": "job-1",
            "runner_id": "zoos-runner-realesrgan",
            "runner_version": "0.1.0",
            "result": null,
            "exit_code": null,
            "started_at_ms": null,
            "finished_at_ms": null
        }))
        .expect("Goal 1A manifest must remain readable");

        assert_eq!(manifest.actual_backend, None);
        assert_eq!(manifest.actual_device, None);
        assert_eq!(manifest.runtime_sha256, None);
        assert_eq!(manifest.model_param_sha256, None);
        assert_eq!(manifest.model_bin_sha256, None);
        assert_eq!(manifest.model_onnx_sha256, None);
        assert_eq!(manifest.fallback_reason, None);
    }
}
