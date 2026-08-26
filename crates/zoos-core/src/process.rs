use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use zoos_runner_protocol::{RunnerCapabilities, RunnerEvent, RunnerEventPayload};

use crate::domain::{ExecutionReport, ExecutionRequest, JobKind};

const MAX_EVENT_LINE_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;

#[async_trait]
pub(crate) trait ExecutionBackend: Send + Sync {
    async fn probe(&self, launch: &RunnerLaunchSpec) -> Result<RunnerCapabilities, BackendError>;

    async fn execute(
        &self,
        launch: &RunnerLaunchSpec,
        request: ExecutionRequest,
        events: mpsc::Sender<RunnerEvent>,
        cancellation: watch::Receiver<bool>,
    ) -> Result<ExecutionReport, BackendError>;
}

#[derive(Debug, Clone)]
pub struct ProcessExecutionBackend {
    activity_timeout: Duration,
    termination_grace: Duration,
}

impl ProcessExecutionBackend {
    pub fn new(activity_timeout: Duration, termination_grace: Duration) -> Self {
        Self {
            activity_timeout,
            termination_grace,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunnerLaunchSpec {
    pub runner_id: String,
    pub executable: PathBuf,
}

impl RunnerLaunchSpec {
    pub fn new(runner_id: impl Into<String>, executable: PathBuf) -> Result<Self, BackendError> {
        let runner_id = runner_id.into();
        if runner_id.trim().is_empty() || !executable.is_absolute() {
            return Err(BackendError::InvalidRunnerPath);
        }
        Ok(Self {
            runner_id,
            executable,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunnerRegistry {
    runners: HashMap<JobKind, RunnerLaunchSpec>,
}

impl RunnerRegistry {
    pub fn with_runner(kind: JobKind, launch: RunnerLaunchSpec) -> Self {
        Self {
            runners: HashMap::from([(kind, launch)]),
        }
    }

    pub fn register(&mut self, kind: JobKind, launch: RunnerLaunchSpec) {
        self.runners.insert(kind, launch);
    }

    pub fn resolve(&self, kind: JobKind) -> Result<&RunnerLaunchSpec, BackendError> {
        self.runners
            .get(&kind)
            .ok_or(BackendError::RunnerNotRegistered(kind))
    }
}

#[async_trait]
impl ExecutionBackend for ProcessExecutionBackend {
    async fn probe(&self, launch: &RunnerLaunchSpec) -> Result<RunnerCapabilities, BackendError> {
        let output = timeout(
            self.activity_timeout,
            Command::new(&launch.executable)
                .args(["--capabilities", "--json"])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| BackendError::RunnerTimedOut)?
        .map_err(|error| BackendError::SpawnFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(BackendError::ProbeFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let capabilities: RunnerCapabilities = serde_json::from_slice(&output.stdout)
            .map_err(|error| BackendError::ProbeFailed(error.to_string()))?;
        capabilities
            .validate()
            .map_err(|error| BackendError::ProbeFailed(error.to_string()))?;
        if capabilities.runner_id != launch.runner_id {
            return Err(BackendError::ProbeFailed(format!(
                "expected runner {}, got {}",
                launch.runner_id, capabilities.runner_id
            )));
        }
        Ok(capabilities)
    }

    async fn execute(
        &self,
        launch: &RunnerLaunchSpec,
        request: ExecutionRequest,
        events: mpsc::Sender<RunnerEvent>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<ExecutionReport, BackendError> {
        if *cancellation.borrow() {
            return Err(BackendError::Cancelled);
        }

        let mut command = Command::new(&launch.executable);
        command
            .args(["run", "--job"])
            .arg(&request.runner_job_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| BackendError::SpawnFailed(error.to_string()))?;
        let child_id = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::SpawnFailed("runner stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BackendError::SpawnFailed("runner stderr was not piped".into()))?;
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.take(MAX_STDERR_BYTES).read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).trim().to_owned()
        });

        let mut reader = BufReader::new(stdout);
        let mut next_sequence = 1;
        let mut saw_started = false;
        let mut terminal: Option<RunnerEventPayload> = None;

        loop {
            let line = tokio::select! {
                changed = cancellation.changed() => {
                    if changed.is_err() || *cancellation.borrow() {
                        let _ = terminate_and_collect_stderr(
                            &mut child,
                            child_id,
                            self.termination_grace,
                            stderr_task,
                        ).await;
                        return Err(BackendError::Cancelled);
                    }
                    continue;
                }
                result = timeout(self.activity_timeout, read_bounded_line(&mut reader)) => {
                    match result {
                        Ok(Ok(line)) => line,
                        Ok(Err(error)) => {
                            let _ = terminate_and_collect_stderr(
                                &mut child,
                                child_id,
                                self.termination_grace,
                                stderr_task,
                            ).await;
                            return Err(match error {
                                ReadEventError::TooLong => BackendError::ProtocolViolation(
                                    "runner event exceeded 64 KiB".into(),
                                ),
                                ReadEventError::Io(error) => BackendError::Io(error),
                            });
                        }
                        Err(_) => {
                            let _ = terminate_and_collect_stderr(
                                &mut child,
                                child_id,
                                self.termination_grace,
                                stderr_task,
                            ).await;
                            return Err(BackendError::RunnerTimedOut);
                        }
                    }
                }
            };

            let Some(mut line) = line else {
                break;
            };

            while matches!(line.last(), Some(b'\r' | b'\n')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let trimmed = match std::str::from_utf8(&line) {
                Ok(trimmed) => trimmed,
                Err(error) => {
                    let _ = terminate_and_collect_stderr(
                        &mut child,
                        child_id,
                        self.termination_grace,
                        stderr_task,
                    )
                    .await;
                    return Err(BackendError::ProtocolViolation(format!(
                        "runner event was not UTF-8: {error}"
                    )));
                }
            };
            let event: RunnerEvent = match serde_json::from_str(trimmed) {
                Ok(event) => event,
                Err(error) => {
                    let _ = terminate_and_collect_stderr(
                        &mut child,
                        child_id,
                        self.termination_grace,
                        stderr_task,
                    )
                    .await;
                    return Err(BackendError::ProtocolViolation(format!(
                        "invalid NDJSON event: {error}"
                    )));
                }
            };
            if let Err(error) = event.validate(&request.job_id, next_sequence) {
                let _ = terminate_and_collect_stderr(
                    &mut child,
                    child_id,
                    self.termination_grace,
                    stderr_task,
                )
                .await;
                return Err(BackendError::ProtocolViolation(error.to_string()));
            }
            if terminal.is_some() {
                let _ = terminate_and_collect_stderr(
                    &mut child,
                    child_id,
                    self.termination_grace,
                    stderr_task,
                )
                .await;
                return Err(BackendError::ProtocolViolation(
                    "runner emitted an event after a terminal event".into(),
                ));
            }
            match &event.payload {
                RunnerEventPayload::Started { .. } if !saw_started && next_sequence == 1 => {
                    saw_started = true;
                }
                RunnerEventPayload::Started { .. } => {
                    let _ = terminate_and_collect_stderr(
                        &mut child,
                        child_id,
                        self.termination_grace,
                        stderr_task,
                    )
                    .await;
                    return Err(BackendError::ProtocolViolation(
                        "runner emitted duplicate or late started event".into(),
                    ));
                }
                _ if !saw_started => {
                    let _ = terminate_and_collect_stderr(
                        &mut child,
                        child_id,
                        self.termination_grace,
                        stderr_task,
                    )
                    .await;
                    return Err(BackendError::ProtocolViolation(
                        "first runner event must be started".into(),
                    ));
                }
                _ => {}
            }
            if event.is_terminal() {
                terminal = Some(event.payload.clone());
            }
            next_sequence += 1;
            tokio::select! {
                changed = cancellation.changed() => {
                    if changed.is_err() || *cancellation.borrow() {
                        let _ = terminate_and_collect_stderr(
                            &mut child,
                            child_id,
                            self.termination_grace,
                            stderr_task,
                        ).await;
                        return Err(BackendError::Cancelled);
                    }
                }
                result = events.send(event) => {
                    if result.is_err() {
                        let _ = terminate_and_collect_stderr(
                            &mut child,
                            child_id,
                            self.termination_grace,
                            stderr_task,
                        ).await;
                        return Err(BackendError::EventConsumerClosed);
                    }
                }
            }
        }

        let status = tokio::select! {
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    let _ = terminate_and_collect_stderr(
                        &mut child,
                        child_id,
                        self.termination_grace,
                        stderr_task,
                    ).await;
                    return Err(BackendError::Cancelled);
                }
                timeout(self.activity_timeout, child.wait()).await
            }
            result = timeout(self.activity_timeout, child.wait()) => result
        };
        let status = match status {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let _ = collect_stderr(stderr_task, self.termination_grace).await;
                return Err(BackendError::Io(error));
            }
            Err(_) => {
                let _ = terminate_and_collect_stderr(
                    &mut child,
                    child_id,
                    self.termination_grace,
                    stderr_task,
                )
                .await;
                return Err(BackendError::RunnerTimedOut);
            }
        };
        let stderr = collect_stderr(stderr_task, self.termination_grace).await;
        let exit_code = status.code();

        match terminal {
            Some(RunnerEventPayload::Completed { output }) if status.success() => {
                if output.path != request.expected_output_path {
                    return Err(BackendError::OutputVerificationFailed(
                        "runner reported an unexpected output path".into(),
                    ));
                }
                let metadata = std::fs::metadata(&output.path)
                    .map_err(|error| BackendError::OutputVerificationFailed(error.to_string()))?;
                if !metadata.is_file() || metadata.len() == 0 {
                    return Err(BackendError::OutputVerificationFailed(
                        "runner output is empty or not a file".into(),
                    ));
                }
                Ok(ExecutionReport { exit_code })
            }
            Some(RunnerEventPayload::Completed { .. }) => Err(BackendError::ProtocolViolation(
                format!("completed event conflicted with exit code {exit_code:?}"),
            )),
            Some(RunnerEventPayload::Failed {
                error_code,
                message,
            }) if !status.success() => Err(BackendError::RunnerFailed {
                error_code,
                message,
                exit_code,
            }),
            Some(RunnerEventPayload::Failed { .. }) => Err(BackendError::ProtocolViolation(
                "failed event conflicted with exit code 0".into(),
            )),
            Some(_) => Err(BackendError::ProtocolViolation(
                "runner did not emit a terminal event".into(),
            )),
            None if !status.success() => Err(BackendError::RunnerCrashed { exit_code, stderr }),
            None => Err(BackendError::ProtocolViolation(
                "runner exited successfully without a terminal event".into(),
            )),
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<Vec<u8>>, ReadEventError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_EVENT_LINE_BYTES {
            return Err(ReadEventError::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[derive(Debug)]
enum ReadEventError {
    TooLong,
    Io(std::io::Error),
}

impl From<std::io::Error> for ReadEventError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

async fn terminate_and_collect_stderr(
    child: &mut Child,
    child_id: Option<u32>,
    grace: Duration,
    stderr_task: tokio::task::JoinHandle<String>,
) -> String {
    terminate_process_tree(child, child_id, grace).await;
    collect_stderr(stderr_task, grace).await
}

async fn collect_stderr(
    mut stderr_task: tokio::task::JoinHandle<String>,
    grace: Duration,
) -> String {
    match timeout(grace, &mut stderr_task).await {
        Ok(Ok(stderr)) => stderr,
        Ok(Err(_)) => String::new(),
        Err(_) => {
            stderr_task.abort();
            String::new()
        }
    }
}

async fn terminate_process_tree(child: &mut Child, child_id: Option<u32>, grace: Duration) {
    #[cfg(unix)]
    {
        if let Some(child_id) = child_id {
            send_group_signal(child_id, libc::SIGTERM);
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let _ = child.try_wait();
                if !process_group_exists(child_id) {
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            send_group_signal(child_id, libc::SIGKILL);
        }
        let _ = child.start_kill();
        let _ = timeout(grace, child.wait()).await;
    }

    #[cfg(not(unix))]
    {
        let _ = child_id;
        let _ = child.start_kill();
        let _ = timeout(grace, child.wait()).await;
    }
}

#[cfg(unix)]
fn send_group_signal(child_id: u32, signal: libc::c_int) {
    if let Ok(process_group) = i32::try_from(child_id) {
        // SAFETY: the child was placed in its own process group before spawn. A negative pid
        // targets that group only, and errors (including an already-exited process) are harmless.
        unsafe {
            libc::kill(-process_group, signal);
        }
    }
}

#[cfg(unix)]
fn process_group_exists(child_id: u32) -> bool {
    let Ok(process_group) = i32::try_from(child_id) else {
        return false;
    };
    // SAFETY: signal 0 performs an existence/permission check and does not deliver a signal.
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("runner path must be absolute")]
    InvalidRunnerPath,
    #[error("no runner is registered for {0:?}")]
    RunnerNotRegistered(JobKind),
    #[error("runner capability probe failed: {0}")]
    ProbeFailed(String),
    #[error("runner could not start: {0}")]
    SpawnFailed(String),
    #[error("runner protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("runner crashed with exit code {exit_code:?}: {stderr}")]
    RunnerCrashed {
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("runner failed with {error_code}: {message}")]
    RunnerFailed {
        error_code: String,
        message: String,
        exit_code: Option<i32>,
    },
    #[error("runner stopped producing events before the timeout")]
    RunnerTimedOut,
    #[error("runner was cancelled")]
    Cancelled,
    #[error("runner output verification failed: {0}")]
    OutputVerificationFailed(String),
    #[error("runner event consumer closed unexpectedly")]
    EventConsumerClosed,
    #[error("runner I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl BackendError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidRunnerPath | Self::RunnerNotRegistered(_) | Self::SpawnFailed(_) => {
                "SPAWN_FAILED"
            }
            Self::ProbeFailed(_) => "CAPABILITY_PROBE_FAILED",
            Self::ProtocolViolation(_) => "PROTOCOL_VIOLATION",
            Self::RunnerCrashed { .. } => "RUNNER_CRASHED",
            Self::RunnerFailed { .. } => "RUNNER_FAILED",
            Self::RunnerTimedOut => "RUNNER_TIMED_OUT",
            Self::Cancelled => "CANCELLED",
            Self::OutputVerificationFailed(_) => "OUTPUT_VERIFICATION_FAILED",
            Self::EventConsumerClosed | Self::Io(_) => "INTERNAL_ERROR",
        }
    }

    pub(crate) fn user_message(&self) -> String {
        match self {
            Self::InvalidRunnerPath | Self::RunnerNotRegistered(_) | Self::SpawnFailed(_) => {
                "The local validation engine could not start.".into()
            }
            Self::ProbeFailed(_) => {
                "The local validation engine did not report valid capabilities.".into()
            }
            Self::ProtocolViolation(_) => {
                "The validation engine returned an unexpected response.".into()
            }
            Self::RunnerCrashed { .. } => "The validation engine stopped unexpectedly.".into(),
            Self::RunnerFailed { message, .. } => message.clone(),
            Self::RunnerTimedOut => "The validation engine stopped responding.".into(),
            Self::Cancelled => "The validation run was cancelled.".into(),
            Self::OutputVerificationFailed(_) => {
                "The validation output could not be verified.".into()
            }
            Self::EventConsumerClosed | Self::Io(_) => {
                "An internal error interrupted the validation run.".into()
            }
        }
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        match self {
            Self::RunnerCrashed { exit_code, .. } | Self::RunnerFailed { exit_code, .. } => {
                *exit_code
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, BufReader};

    use super::*;

    #[tokio::test]
    async fn event_reader_rejects_a_line_before_unbounded_allocation() {
        let (mut writer, reader) = tokio::io::duplex(MAX_EVENT_LINE_BYTES * 2);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_EVENT_LINE_BYTES + 1])
                .await
                .expect("test data must be written");
        });
        let mut reader = BufReader::new(reader);

        let result = read_bounded_line(&mut reader).await;

        assert!(matches!(result, Err(ReadEventError::TooLong)));
        writer_task.await.expect("writer task must finish");
    }
}
