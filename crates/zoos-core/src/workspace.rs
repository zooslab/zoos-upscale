use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use uuid::Uuid;
use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, FakeParameters, FakeTask, RunnerEvent, RunnerInput, RunnerOutput,
};

use crate::domain::{
    JobManifest, JobPlan, JobProgress, JobStatus, JobSummary, ProductJobSpec, StoredJob,
};

const JOB_SPEC_FILE: &str = "job-spec.json";
const PLAN_FILE: &str = "plan.json";
const RUNNER_JOB_FILE: &str = "runner-job.json";
const PROGRESS_FILE: &str = "progress.json";
const MANIFEST_FILE: &str = "manifest.json";
const LOGS_FILE: &str = "logs.jsonl";
const PLAN_REVISIONS_FILE: &str = "plan-revisions.jsonl";

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceStore {
    root: PathBuf,
}

impl WorkspaceStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(WorkspaceError::RootMustBeAbsolute);
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        Ok(Self { root })
    }

    pub fn create_fake_job(&self, behavior: FakeBehavior) -> Result<JobSummary, WorkspaceError> {
        let job_id = Uuid::new_v4().to_string();
        let job_dir = self.root.join(&job_id);
        fs::create_dir(&job_dir)?;
        fs::create_dir(job_dir.join("final"))?;

        let created_at_ms = now_ms();
        let input_path = job_dir.join("input.txt");
        let output_path = job_dir.join("final/result.txt");
        fs::write(&input_path, b"Zoos Upscale fake input\n")?;

        let job_spec = ProductJobSpec {
            schema_version: 1,
            job_id: job_id.clone(),
            kind: "fake_validation".into(),
            scenario: behavior,
            created_at_ms,
        };
        let plan = JobPlan {
            schema_version: 1,
            job_id: job_id.clone(),
            execution_backend: "process".into(),
            runner_id: "zoos-runner-fake".into(),
        };
        let runner_request = FakeJobRequest {
            protocol_version: 1,
            job_id: job_id.clone(),
            task: FakeTask::Fake,
            input: RunnerInput { path: input_path },
            output: RunnerOutput { path: output_path },
            parameters: FakeParameters {
                steps: 20,
                step_delay_ms: 80,
            },
            test_behavior: behavior,
        };
        runner_request
            .validate()
            .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;

        let summary = JobSummary {
            job_id: job_id.clone(),
            scenario: behavior,
            status: JobStatus::Created,
            progress_percent: 0,
            stage: None,
            message: "Ready to start".into(),
            error: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
        };
        let progress = JobProgress {
            schema_version: 1,
            summary: summary.clone(),
        };
        let manifest = JobManifest {
            schema_version: 1,
            job_id: job_id.clone(),
            runner_id: "zoos-runner-fake".into(),
            runner_version: env!("CARGO_PKG_VERSION").into(),
            result: None,
            exit_code: None,
            started_at_ms: None,
            finished_at_ms: None,
        };

        write_json_atomic(&job_dir.join(JOB_SPEC_FILE), &job_spec)?;
        write_json_atomic(&job_dir.join(PLAN_FILE), &plan)?;
        write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &runner_request)?;
        write_json_atomic(&job_dir.join(PROGRESS_FILE), &progress)?;
        write_json_atomic(&job_dir.join(MANIFEST_FILE), &manifest)?;
        create_empty_file(&job_dir.join(LOGS_FILE))?;
        create_empty_file(&job_dir.join(PLAN_REVISIONS_FILE))?;

        Ok(summary)
    }

    pub fn list_jobs(&self) -> Result<Vec<JobSummary>, WorkspaceError> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let progress_path = entry.path().join(PROGRESS_FILE);
            if !progress_path.exists() {
                continue;
            }
            let progress: JobProgress = read_json(&progress_path)?;
            if entry.file_name().to_string_lossy() != progress.summary.job_id {
                return Err(WorkspaceError::UnsafeRunnerRequest);
            }
            jobs.push(progress.summary);
        }
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at_ms));
        Ok(jobs)
    }

    pub fn load_summary(&self, job_id: &str) -> Result<JobSummary, WorkspaceError> {
        let progress: JobProgress = read_json(&self.job_dir(job_id)?.join(PROGRESS_FILE))?;
        Ok(progress.summary)
    }

    pub fn load_stored_job(&self, job_id: &str) -> Result<StoredJob, WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        let progress: JobProgress = read_json(&job_dir.join(PROGRESS_FILE))?;
        let runner_request: FakeJobRequest = read_json(&job_dir.join(RUNNER_JOB_FILE))?;
        runner_request
            .validate()
            .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;

        let expected_output = job_dir.join("final/result.txt");
        if runner_request.job_id != job_id || runner_request.output.path != expected_output {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }

        Ok(StoredJob {
            progress,
            runner_request,
            runner_job_path: job_dir.join(RUNNER_JOB_FILE),
        })
    }

    pub fn update_summary(
        &self,
        job_id: &str,
        update: impl FnOnce(&mut JobSummary),
    ) -> Result<JobSummary, WorkspaceError> {
        let path = self.job_dir(job_id)?.join(PROGRESS_FILE);
        let mut progress: JobProgress = read_json(&path)?;
        update(&mut progress.summary);
        progress.summary.updated_at_ms = now_ms();
        write_json_atomic(&path, &progress)?;
        Ok(progress.summary)
    }

    pub fn append_event(&self, job_id: &str, event: &RunnerEvent) -> Result<(), WorkspaceError> {
        append_json_line(&self.job_dir(job_id)?.join(LOGS_FILE), event)
    }

    pub fn finish_manifest(
        &self,
        job_id: &str,
        result: &str,
        exit_code: Option<i32>,
        started_at_ms: u64,
    ) -> Result<(), WorkspaceError> {
        let path = self.job_dir(job_id)?.join(MANIFEST_FILE);
        let mut manifest: JobManifest = read_json(&path)?;
        manifest.result = Some(result.into());
        manifest.exit_code = exit_code;
        manifest.started_at_ms = Some(started_at_ms);
        manifest.finished_at_ms = Some(now_ms());
        write_json_atomic(&path, &manifest)
    }

    pub fn cleanup_unverified_output(&self, job_id: &str) -> Result<(), WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        remove_if_exists(&job_dir.join("final/result.txt"))?;
        remove_if_exists(&job_dir.join("final").join(format!(".{job_id}.partial")))?;
        Ok(())
    }

    pub fn recover_interrupted(&self) -> Result<(), WorkspaceError> {
        for job in self.list_jobs()? {
            if job.status.is_active() {
                self.update_summary(&job.job_id, |summary| {
                    summary.status = JobStatus::Interrupted;
                    summary.stage = None;
                    summary.message = "Interrupted during the previous app session".into();
                })?;
            }
        }
        Ok(())
    }

    fn job_dir(&self, job_id: &str) -> Result<PathBuf, WorkspaceError> {
        Uuid::parse_str(job_id).map_err(|_| WorkspaceError::InvalidJobId)?;
        let job_dir = self.root.join(job_id);
        let metadata = match fs::symlink_metadata(&job_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(WorkspaceError::JobNotFound(job_id.into()));
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        if !metadata.is_dir() {
            return Err(WorkspaceError::JobNotFound(job_id.into()));
        }
        let canonical = fs::canonicalize(job_dir)?;
        if canonical.parent() != Some(self.root.as_path()) {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        Ok(canonical)
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), WorkspaceError> {
    let mut file = AtomicWriteFile::open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.commit()?;
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, WorkspaceError> {
    let file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkspaceError::JobNotFound(path.display().to_string())
        } else {
            WorkspaceError::Io(error)
        }
    })?;
    Ok(serde_json::from_reader(BufReader::new(file))?)
}

fn create_empty_file(path: &Path) -> Result<(), WorkspaceError> {
    OpenOptions::new().create_new(true).write(true).open(path)?;
    Ok(())
}

fn append_json_line(path: &Path, value: &impl Serialize) -> Result<(), WorkspaceError> {
    let file = OpenOptions::new().append(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)?;
    writeln!(writer)?;
    writer.flush()?;
    writer.get_ref().sync_data()?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), WorkspaceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace root must be absolute")]
    RootMustBeAbsolute,
    #[error("invalid job id")]
    InvalidJobId,
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("runner job points outside its managed workspace")]
    UnsafeRunnerRequest,
    #[error("invalid runner contract: {0}")]
    InvalidRunnerContract(String),
    #[error("workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_update_remains_valid_json() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");

        store
            .update_summary(&created.job_id, |summary| {
                summary.status = JobStatus::Running;
                summary.progress_percent = 45;
            })
            .expect("progress must update");

        let loaded = store
            .load_summary(&created.job_id)
            .expect("progress must load");
        assert_eq!(loaded.status, JobStatus::Running);
        assert_eq!(loaded.progress_percent, 45);
    }

    #[test]
    fn startup_recovery_marks_active_job_interrupted() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Hang)
            .expect("job must be created");
        store
            .update_summary(&created.job_id, |summary| {
                summary.status = JobStatus::Running;
            })
            .expect("progress must update");

        store.recover_interrupted().expect("recovery must complete");

        assert_eq!(
            store
                .load_summary(&created.job_id)
                .expect("progress must load")
                .status,
            JobStatus::Interrupted
        );
    }
}
