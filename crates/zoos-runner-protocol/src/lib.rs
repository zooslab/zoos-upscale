use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FakeTask {
    Fake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerOutput {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeParameters {
    pub steps: u32,
    pub step_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerCapabilities {
    pub protocol_version: u32,
    pub runner_id: String,
    pub runner_version: String,
    pub tasks: Vec<FakeTask>,
    pub test_behaviors: Vec<FakeBehavior>,
}

impl RunnerCapabilities {
    pub fn fake() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            runner_id: "zoos-runner-fake".into(),
            runner_version: env!("CARGO_PKG_VERSION").into(),
            tasks: vec![FakeTask::Fake],
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
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    UnsupportedProtocol(u32),
    EmptyJobId,
    InvalidSteps(u32),
    InvalidDelay(u64),
    RelativePath(&'static str),
    EmptyRunnerId,
    EmptyRunnerVersion,
    EmptyTasks,
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
            Self::RelativePath(field) => write!(formatter, "{field} path must be absolute"),
            Self::EmptyRunnerId => formatter.write_str("runner_id must not be empty"),
            Self::EmptyRunnerVersion => formatter.write_str("runner_version must not be empty"),
            Self::EmptyTasks => formatter.write_str("tasks must not be empty"),
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
}
