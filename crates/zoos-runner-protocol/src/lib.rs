use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const IMAGE_PROTOCOL_VERSION_V2: u32 = 2;
pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FakeJobRequest {
    pub protocol_version: u32,
    pub job_id: String,
    pub task: FakeTask,
    pub input: RunnerInput,
    pub output: RunnerOutput,
    pub parameters: FakeParameters,
    pub test_behavior: FakeBehavior,
}

impl FakeJobRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ContractError::UnsupportedProtocol(self.protocol_version));
        }
        if self.job_id.trim().is_empty() {
            return Err(ContractError::EmptyJobId);
        }
        if self.parameters.steps == 0 || self.parameters.steps > 1_000 {
            return Err(ContractError::InvalidSteps(self.parameters.steps));
        }
        if self.parameters.step_delay_ms > 60_000 {
            return Err(ContractError::InvalidDelay(self.parameters.step_delay_ms));
        }
        if !self.input.path.is_absolute() {
            return Err(ContractError::RelativePath("input"));
        }
        if !self.output.path.is_absolute() {
            return Err(ContractError::RelativePath("output"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageUpscaleJobRequest {
    pub protocol_version: u32,
    pub job_id: String,
    pub task: ImageTask,
    pub input: ImageRunnerInput,
    pub output: ImageRunnerOutput,
    pub parameters: ImageUpscaleParameters,
}

impl ImageUpscaleJobRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ContractError::UnsupportedProtocol(self.protocol_version));
        }
        if self.job_id.trim().is_empty() {
            return Err(ContractError::EmptyJobId);
        }
        if !self.input.path.is_absolute() {
            return Err(ContractError::RelativePath("input"));
        }
        if !self.output.path.is_absolute() {
            return Err(ContractError::RelativePath("output"));
        }
        if self.input.sha256.len() != 64
            || !self
                .input
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContractError::InvalidImageParameter("input.sha256"));
        }
        if self.input.width == 0 || self.input.height == 0 {
            return Err(ContractError::InvalidImageParameter("input dimensions"));
        }
        if !matches!(self.parameters.scale, 2 | 4) {
            return Err(ContractError::InvalidScale(self.parameters.scale));
        }
        let mapped_model = match self.parameters.preset {
            ImagePreset::Photo => ImageModelId::RealEsrganX4plus,
            ImagePreset::Anime => ImageModelId::RealEsrganX4plusAnime,
        };
        if self.parameters.model_id != mapped_model {
            return Err(ContractError::InvalidImageParameter("model_id"));
        }
        if self.parameters.tile_size != 256 {
            return Err(ContractError::InvalidImageParameter("tile_size"));
        }
        if self.parameters.gpu_id != 0 {
            return Err(ContractError::InvalidImageParameter("gpu_id"));
        }
        if self.parameters.threads != "1:2:2" {
            return Err(ContractError::InvalidImageParameter("threads"));
        }
        Ok(())
    }
}

/// Backend-neutral image request produced by core after it has normalized the source image.
/// Runner events intentionally continue to use the stable v1 event protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageUpscaleJobRequestV2 {
    pub protocol_version: u32,
    pub job_id: String,
    pub task: ImageTask,
    pub input: ImageInferenceInputV2,
    pub output: ImageIntermediateOutputV2,
    pub parameters: ImageUpscaleParametersV2,
}

impl ImageUpscaleJobRequestV2 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != IMAGE_PROTOCOL_VERSION_V2 {
            return Err(ContractError::UnsupportedProtocol(self.protocol_version));
        }
        if self.job_id.trim().is_empty() {
            return Err(ContractError::EmptyJobId);
        }
        if !self.input.path.is_absolute() {
            return Err(ContractError::RelativePath("input"));
        }
        if !self.output.path.is_absolute() {
            return Err(ContractError::RelativePath("output"));
        }
        validate_sha256(&self.input.sha256)?;
        if self.input.width == 0 || self.input.height == 0 {
            return Err(ContractError::InvalidImageParameter("input dimensions"));
        }
        if !matches!(self.parameters.requested_scale, 2 | 4) {
            return Err(ContractError::InvalidScale(self.parameters.requested_scale));
        }
        if self.parameters.native_scale != 4 {
            return Err(ContractError::InvalidImageParameter("native_scale"));
        }
        match (&self.parameters.device, &self.parameters.backend_settings) {
            (
                ImageDeviceV2::Vulkan { index: 0 },
                ImageBackendSettingsV2::Vulkan { tile_size, threads },
            ) if *tile_size > 0 && !threads.trim().is_empty() => {}
            (
                ImageDeviceV2::Cpu,
                ImageBackendSettingsV2::OrtCpu {
                    tile_size,
                    intra_threads,
                    inter_threads,
                },
            ) if *tile_size > 0 && *intra_threads > 0 && *inter_threads > 0 => {}
            _ => return Err(ContractError::InvalidImageParameter("backend_settings")),
        }
        Ok(())
    }
}

fn validate_sha256(value: &str) -> Result<(), ContractError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ContractError::InvalidImageParameter("input.sha256"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageInferenceInputV2 {
    pub path: PathBuf,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    #[schemars(range(min = 1))]
    pub width: u32,
    #[schemars(range(min = 1))]
    pub height: u32,
    pub format: ImageInferenceFormatV2,
    pub pixel_format: ImagePixelFormatV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageIntermediateOutputV2 {
    pub path: PathBuf,
    pub format: ImageInferenceFormatV2,
    pub pixel_format: ImagePixelFormatV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageInferenceFormatV2 {
    Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImagePixelFormatV2 {
    Rgb8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageUpscaleParametersV2 {
    pub semantic_model: ImageSemanticModelV2,
    #[schemars(range(min = 2, max = 4))]
    pub requested_scale: u8,
    #[schemars(range(min = 4, max = 4))]
    pub native_scale: u8,
    pub device: ImageDeviceV2,
    pub backend_settings: ImageBackendSettingsV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageSemanticModelV2 {
    Photo,
    Anime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageDeviceV2 {
    Vulkan { index: u32 },
    Cpu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageBackendSettingsV2 {
    Vulkan {
        #[schemars(range(min = 1))]
        tile_size: u32,
        threads: String,
    },
    OrtCpu {
        #[schemars(range(min = 1))]
        tile_size: u32,
        #[schemars(range(min = 1))]
        intra_threads: u32,
        #[schemars(range(min = 1))]
        inter_threads: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRunnerInput {
    pub path: PathBuf,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
    #[schemars(range(min = 1))]
    pub width: u32,
    #[schemars(range(min = 1))]
    pub height: u32,
    pub format: ImageInputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageInputFormat {
    Png,
    Jpeg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRunnerOutput {
    pub path: PathBuf,
    pub format: ImageOutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageOutputFormat {
    Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageTask {
    ImageUpscale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImagePreset {
    Photo,
    Anime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageUpscaleParameters {
    pub preset: ImagePreset,
    pub model_id: ImageModelId,
    #[schemars(range(min = 2, max = 4))]
    pub scale: u8,
    #[schemars(range(min = 256, max = 256))]
    pub tile_size: u32,
    #[schemars(range(max = 0))]
    pub gpu_id: u32,
    #[schemars(regex(pattern = "^1:2:2$"))]
    pub threads: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ImageModelId {
    #[serde(rename = "realesrgan-x4plus")]
    #[schemars(rename = "realesrgan-x4plus")]
    RealEsrganX4plus,
    #[serde(rename = "realesrgan-x4plus-anime")]
    #[schemars(rename = "realesrgan-x4plus-anime")]
    RealEsrganX4plusAnime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FakeTask {
    Fake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerOutput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FakeParameters {
    pub steps: u32,
    pub step_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FakeBehavior {
    Success,
    Failed,
    MalformedNdjson,
    Crash,
    Hang,
    CompletedThenNonzero,
    SpawnGrandchildAndHang,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunnerEvent {
    pub protocol_version: u32,
    pub event_version: u32,
    pub sequence: u64,
    pub job_id: String,
    #[serde(flatten)]
    pub payload: RunnerEventPayload,
}

impl RunnerEvent {
    pub fn new(sequence: u64, job_id: impl Into<String>, payload: RunnerEventPayload) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            event_version: EVENT_VERSION,
            sequence,
            job_id: job_id.into(),
            payload,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.payload,
            RunnerEventPayload::Completed { .. } | RunnerEventPayload::Failed { .. }
        )
    }

    pub fn validate(
        &self,
        expected_job_id: &str,
        expected_sequence: u64,
    ) -> Result<(), EventContractError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(EventContractError::UnsupportedProtocol(
                self.protocol_version,
            ));
        }
        if self.event_version != EVENT_VERSION {
            return Err(EventContractError::UnsupportedEventVersion(
                self.event_version,
            ));
        }
        if self.job_id != expected_job_id {
            return Err(EventContractError::JobIdMismatch);
        }
        if self.sequence != expected_sequence {
            return Err(EventContractError::SequenceMismatch {
                expected: expected_sequence,
                actual: self.sequence,
            });
        }

        match &self.payload {
            RunnerEventPayload::Started { stage } if stage.trim().is_empty() => {
                Err(EventContractError::EmptyField("stage"))
            }
            RunnerEventPayload::Progress {
                stage,
                completed_units,
                total_units,
                unit,
                chunk_id,
                rate,
                rate_unit,
                ..
            } => {
                if stage.trim().is_empty() {
                    return Err(EventContractError::EmptyField("stage"));
                }
                if unit.trim().is_empty() {
                    return Err(EventContractError::EmptyField("unit"));
                }
                if *total_units == 0 || completed_units > total_units {
                    return Err(EventContractError::InvalidProgress);
                }
                if chunk_id
                    .as_deref()
                    .is_some_and(|chunk| chunk.trim().is_empty())
                {
                    return Err(EventContractError::EmptyField("chunk_id"));
                }
                if rate.is_some_and(|value| !value.is_finite() || value < 0.0)
                    || rate.is_some() != rate_unit.is_some()
                {
                    return Err(EventContractError::InvalidProgress);
                }
                if rate_unit
                    .as_deref()
                    .is_some_and(|unit| unit.trim().is_empty())
                {
                    return Err(EventContractError::EmptyField("rate_unit"));
                }
                Ok(())
            }
            RunnerEventPayload::Warning { code, message }
            | RunnerEventPayload::Failed {
                error_code: code,
                message,
            } => {
                if code.trim().is_empty() {
                    return Err(EventContractError::EmptyField("code"));
                }
                if message.trim().is_empty() {
                    return Err(EventContractError::EmptyField("message"));
                }
                Ok(())
            }
            RunnerEventPayload::Completed { output } if !output.path.is_absolute() => {
                Err(EventContractError::RelativeOutputPath)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerEventPayload {
    Started {
        stage: String,
    },
    Progress {
        stage: String,
        completed_units: u64,
        total_units: u64,
        unit: String,
        elapsed_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_unit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimated_remaining_ms: Option<u64>,
    },
    Warning {
        code: String,
        message: String,
    },
    Completed {
        output: RunnerOutput,
    },
    Failed {
        error_code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerCapabilities {
    pub protocol_version: u32,
    pub runner_id: String,
    pub runner_version: String,
    pub tasks: Vec<RunnerTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelCapability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scales: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceCapability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_behaviors: Vec<FakeBehavior>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunnerTask {
    #[serde(alias = "fake")]
    FakeValidation,
    ImageUpscale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelCapability {
    pub id: String,
    pub scales: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCapability {
    pub index: u32,
    pub name: String,
    pub backend: String,
}

impl RunnerCapabilities {
    pub fn fake() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            runner_id: "zoos-runner-fake".into(),
            runner_version: env!("CARGO_PKG_VERSION").into(),
            tasks: vec![RunnerTask::FakeValidation],
            upstream: None,
            models: Vec::new(),
            scales: Vec::new(),
            devices: Vec::new(),
            test_behaviors: vec![
                FakeBehavior::Success,
                FakeBehavior::Failed,
                FakeBehavior::MalformedNdjson,
                FakeBehavior::Crash,
                FakeBehavior::Hang,
                FakeBehavior::CompletedThenNonzero,
                FakeBehavior::SpawnGrandchildAndHang,
            ],
        }
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ContractError::UnsupportedProtocol(self.protocol_version));
        }
        if self.runner_id.trim().is_empty() {
            return Err(ContractError::EmptyRunnerId);
        }
        if self.runner_version.trim().is_empty() {
            return Err(ContractError::EmptyRunnerVersion);
        }
        if self.tasks.is_empty() {
            return Err(ContractError::EmptyTasks);
        }
        if self.tasks.contains(&RunnerTask::ImageUpscale)
            && (self.upstream.is_none()
                || self.models.is_empty()
                || self.scales.is_empty()
                || self.devices.is_empty())
        {
            return Err(ContractError::InvalidCapabilities("image_upscale"));
        }
        if self.upstream.as_ref().is_some_and(|upstream| {
            upstream.name.trim().is_empty() || upstream.version.trim().is_empty()
        }) {
            return Err(ContractError::InvalidCapabilities("upstream"));
        }
        if self.models.iter().any(|model| {
            model.id.trim().is_empty()
                || model.scales.is_empty()
                || model.scales.iter().any(|scale| !matches!(scale, 2 | 4))
        }) {
            return Err(ContractError::InvalidCapabilities("models"));
        }
        if self.scales.iter().any(|scale| !matches!(scale, 2 | 4)) {
            return Err(ContractError::InvalidCapabilities("scales"));
        }
        if self.models.iter().any(|model| {
            model
                .scales
                .iter()
                .any(|scale| !self.scales.contains(scale))
        }) {
            return Err(ContractError::InvalidCapabilities("model scales"));
        }
        if self
            .devices
            .iter()
            .any(|device| device.name.trim().is_empty() || device.backend.trim().is_empty())
        {
            return Err(ContractError::InvalidCapabilities("devices"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    UnsupportedProtocol(u32),
    EmptyJobId,
    InvalidSteps(u32),
    InvalidDelay(u64),
    InvalidScale(u8),
    InvalidImageParameter(&'static str),
    RelativePath(&'static str),
    EmptyRunnerId,
    EmptyRunnerVersion,
    EmptyTasks,
    InvalidCapabilities(&'static str),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedProtocol(version) => {
                write!(formatter, "unsupported protocol version: {version}")
            }
            Self::EmptyJobId => formatter.write_str("job_id must not be empty"),
            Self::InvalidSteps(steps) => write!(formatter, "steps must be in 1..=1000: {steps}"),
            Self::InvalidDelay(delay) => {
                write!(formatter, "step_delay_ms must not exceed 60000: {delay}")
            }
            Self::InvalidScale(scale) => write!(formatter, "scale must be 2 or 4: {scale}"),
            Self::InvalidImageParameter(field) => {
                write!(formatter, "image upscale parameter is invalid: {field}")
            }
            Self::RelativePath(field) => write!(formatter, "{field} path must be absolute"),
            Self::EmptyRunnerId => formatter.write_str("runner_id must not be empty"),
            Self::EmptyRunnerVersion => formatter.write_str("runner_version must not be empty"),
            Self::EmptyTasks => formatter.write_str("tasks must not be empty"),
            Self::InvalidCapabilities(field) => {
                write!(formatter, "runner capabilities field is invalid: {field}")
            }
        }
    }
}

impl std::error::Error for ContractError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventContractError {
    UnsupportedProtocol(u32),
    UnsupportedEventVersion(u32),
    JobIdMismatch,
    SequenceMismatch { expected: u64, actual: u64 },
    EmptyField(&'static str),
    InvalidProgress,
    RelativeOutputPath,
}

impl std::fmt::Display for EventContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedProtocol(version) => {
                write!(formatter, "unsupported protocol version: {version}")
            }
            Self::UnsupportedEventVersion(version) => {
                write!(formatter, "unsupported event version: {version}")
            }
            Self::JobIdMismatch => formatter.write_str("event job_id does not match the request"),
            Self::SequenceMismatch { expected, actual } => {
                write!(
                    formatter,
                    "event sequence mismatch: expected {expected}, got {actual}"
                )
            }
            Self::EmptyField(field) => write!(formatter, "event field must not be empty: {field}"),
            Self::InvalidProgress => formatter.write_str("event progress values are invalid"),
            Self::RelativeOutputPath => formatter.write_str("event output path must be absolute"),
        }
    }
}

impl std::error::Error for EventContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_keeps_common_and_tagged_fields() {
        let event = RunnerEvent::new(
            2,
            "job-1",
            RunnerEventPayload::Progress {
                stage: "fake".into(),
                completed_units: 1,
                total_units: 4,
                unit: "step".into(),
                elapsed_ms: 20,
                chunk_id: Some("tile-1".into()),
                rate: Some(50.0),
                rate_unit: Some("step/s".into()),
                estimated_remaining_ms: Some(60),
            },
        );

        let value = serde_json::to_value(&event).expect("event must serialize");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["event_version"], 1);
        assert_eq!(value["event"], "progress");
        assert_eq!(value["sequence"], 2);
        event.validate("job-1", 2).expect("event must validate");
    }

    #[test]
    fn event_validation_rejects_sequence_mismatch() {
        let event = RunnerEvent::new(
            3,
            "job-1",
            RunnerEventPayload::Started {
                stage: "fake".into(),
            },
        );

        assert_eq!(
            event.validate("job-1", 2),
            Err(EventContractError::SequenceMismatch {
                expected: 2,
                actual: 3,
            })
        );
    }

    #[test]
    fn image_job_round_trip_matches_fixed_wrapper_contract() {
        let root = std::env::current_dir().expect("current directory must be absolute");
        let request = ImageUpscaleJobRequest {
            protocol_version: PROTOCOL_VERSION,
            job_id: "image-job-1".into(),
            task: ImageTask::ImageUpscale,
            input: ImageRunnerInput {
                path: root.join("input.jpg"),
                sha256: "a".repeat(64),
                width: 640,
                height: 480,
                format: ImageInputFormat::Jpeg,
            },
            output: ImageRunnerOutput {
                path: root.join(".output.partial.png"),
                format: ImageOutputFormat::Png,
            },
            parameters: ImageUpscaleParameters {
                preset: ImagePreset::Anime,
                model_id: ImageModelId::RealEsrganX4plusAnime,
                scale: 4,
                tile_size: 256,
                gpu_id: 0,
                threads: "1:2:2".into(),
            },
        };

        request.validate().expect("image request must validate");
        let value = serde_json::to_value(&request).expect("request must serialize");
        assert_eq!(value["parameters"]["model_id"], "realesrgan-x4plus-anime");
        assert_eq!(value["parameters"]["tile_size"], 256);
        assert_eq!(value["parameters"]["gpu_id"], 0);
        assert_eq!(value["parameters"]["threads"], "1:2:2");
        let round_trip: ImageUpscaleJobRequest =
            serde_json::from_value(value).expect("request must deserialize");
        assert_eq!(round_trip, request);

        let schema = serde_json::to_value(schemars::schema_for!(ImageUpscaleJobRequest))
            .expect("schema must serialize");
        let schema_text = schema.to_string();
        for required in [
            "sha256",
            "width",
            "height",
            "format",
            "model_id",
            "scale",
            "tile_size",
            "gpu_id",
            "threads",
        ] {
            assert!(schema_text.contains(required), "schema omitted {required}");
        }
    }

    #[test]
    fn image_job_rejects_a_model_that_does_not_match_the_preset() {
        let root = std::env::current_dir().expect("current directory must be absolute");
        let request = ImageUpscaleJobRequest {
            protocol_version: PROTOCOL_VERSION,
            job_id: "image-job-1".into(),
            task: ImageTask::ImageUpscale,
            input: ImageRunnerInput {
                path: root.join("input.png"),
                sha256: "0".repeat(64),
                width: 1,
                height: 1,
                format: ImageInputFormat::Png,
            },
            output: ImageRunnerOutput {
                path: root.join("output.png"),
                format: ImageOutputFormat::Png,
            },
            parameters: ImageUpscaleParameters {
                preset: ImagePreset::Photo,
                model_id: ImageModelId::RealEsrganX4plusAnime,
                scale: 2,
                tile_size: 256,
                gpu_id: 0,
                threads: "1:2:2".into(),
            },
        };

        assert_eq!(
            request.validate(),
            Err(ContractError::InvalidImageParameter("model_id"))
        );
    }

    #[test]
    fn image_capabilities_cover_upstream_models_scales_and_device() {
        let capabilities = RunnerCapabilities {
            protocol_version: PROTOCOL_VERSION,
            runner_id: "zoos-runner-realesrgan".into(),
            runner_version: "0.1.0".into(),
            tasks: vec![RunnerTask::ImageUpscale],
            upstream: Some(UpstreamInfo {
                name: "Real-ESRGAN-ncnn-vulkan".into(),
                version: "0.2.0".into(),
                source_commit: Some("37026f4".into()),
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

        capabilities
            .validate()
            .expect("complete image capabilities must validate");
    }

    #[test]
    fn image_v2_contract_round_trips_for_vulkan_and_cpu() {
        let root = std::env::current_dir().expect("current directory must be absolute");
        let base = ImageUpscaleJobRequestV2 {
            protocol_version: IMAGE_PROTOCOL_VERSION_V2,
            job_id: "image-v2".into(),
            task: ImageTask::ImageUpscale,
            input: ImageInferenceInputV2 {
                path: root.join("normalized.png"),
                sha256: "a".repeat(64),
                width: 64,
                height: 48,
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            output: ImageIntermediateOutputV2 {
                path: root.join("native-x4.partial.png"),
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            parameters: ImageUpscaleParametersV2 {
                semantic_model: ImageSemanticModelV2::Photo,
                requested_scale: 2,
                native_scale: 4,
                device: ImageDeviceV2::Vulkan { index: 0 },
                backend_settings: ImageBackendSettingsV2::Vulkan {
                    tile_size: 256,
                    threads: "1:2:2".into(),
                },
            },
        };

        for request in [
            base.clone(),
            ImageUpscaleJobRequestV2 {
                parameters: ImageUpscaleParametersV2 {
                    semantic_model: ImageSemanticModelV2::Anime,
                    requested_scale: 4,
                    native_scale: 4,
                    device: ImageDeviceV2::Cpu,
                    backend_settings: ImageBackendSettingsV2::OrtCpu {
                        tile_size: 128,
                        intra_threads: 4,
                        inter_threads: 1,
                    },
                },
                ..base.clone()
            },
        ] {
            request.validate().expect("v2 request must validate");
            let json = serde_json::to_value(&request).expect("v2 request must serialize");
            assert_eq!(json["protocol_version"], 2);
            assert_eq!(json["input"]["format"], "png");
            assert_eq!(json["input"]["pixel_format"], "rgb8");
            let round_trip: ImageUpscaleJobRequestV2 =
                serde_json::from_value(json).expect("v2 request must deserialize");
            assert_eq!(round_trip, request);
        }
    }

    #[test]
    fn image_v2_contract_rejects_a_device_settings_mismatch() {
        let root = std::env::current_dir().expect("current directory must be absolute");
        let request = ImageUpscaleJobRequestV2 {
            protocol_version: IMAGE_PROTOCOL_VERSION_V2,
            job_id: "image-v2".into(),
            task: ImageTask::ImageUpscale,
            input: ImageInferenceInputV2 {
                path: root.join("normalized.png"),
                sha256: "0".repeat(64),
                width: 1,
                height: 1,
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            output: ImageIntermediateOutputV2 {
                path: root.join("native-x4.png"),
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            parameters: ImageUpscaleParametersV2 {
                semantic_model: ImageSemanticModelV2::Photo,
                requested_scale: 2,
                native_scale: 4,
                device: ImageDeviceV2::Cpu,
                backend_settings: ImageBackendSettingsV2::Vulkan {
                    tile_size: 256,
                    threads: "1:2:2".into(),
                },
            },
        };

        assert_eq!(
            request.validate(),
            Err(ContractError::InvalidImageParameter("backend_settings"))
        );
    }
}
