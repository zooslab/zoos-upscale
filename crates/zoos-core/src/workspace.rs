use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;
use fs4::{FileExt, TryLockError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, FakeParameters, FakeTask, ImageModelId, ImageOutputFormat,
    ImagePreset as ProtocolImagePreset, ImageRunnerInput, ImageRunnerOutput, ImageTask,
    ImageUpscaleJobRequest, ImageUpscaleJobRequestV2, ImageUpscaleParameters, RunnerEvent,
    RunnerInput, RunnerOutput,
};

use crate::domain::{
    ImagePreset, ImageSettings, JobKind, JobManifest, JobPlan, JobProgress, JobStatus, JobSummary,
    ProductJobSpec, StoredJob, StoredRunnerRequest,
};
use crate::image_safety::{
    ImageOutputPlan, ImageSafetyError, ImageVerification, cleanup_owned_output, plan_image_output,
    publish_verified_output, recheck_input,
};

const JOB_SPEC_FILE: &str = "job-spec.json";
const PLAN_FILE: &str = "plan.json";
const RUNNER_JOB_FILE: &str = "runner-job.json";
const PROGRESS_FILE: &str = "progress.json";
const MANIFEST_FILE: &str = "manifest.json";
const LOGS_FILE: &str = "logs.jsonl";
const PLAN_REVISIONS_FILE: &str = "plan-revisions.jsonl";
const VERIFICATION_FILE: &str = "verification.json";
const LOCK_FILE: &str = ".workspace.lock";
const STAGING_DIR: &str = "staging";
const QUARANTINE_DIR: &str = "quarantine";
const DIAGNOSTIC_SUFFIX: &str = ".diagnostic.json";

#[derive(Deserialize)]
struct RunnerRequestVersion {
    protocol_version: u32,
}

#[derive(Debug)]
pub(crate) struct WorkspaceStore {
    root: PathBuf,
    // Keeping this descriptor alive holds the process-wide workspace lease.
    _lock_file: File,
}

impl WorkspaceStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(WorkspaceError::RootMustBeAbsolute);
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(root.join(LOCK_FILE))?;
        FileExt::try_lock(&lock_file).map_err(|error| match error {
            TryLockError::WouldBlock => WorkspaceError::WorkspaceLocked,
            TryLockError::Error(error) => WorkspaceError::Io(error),
        })?;

        let store = Self {
            root,
            _lock_file: lock_file,
        };
        fs::create_dir_all(store.staging_dir())?;
        fs::create_dir_all(store.quarantine_dir())?;
        store.recover_staging()?;
        store.recover_corrupt_jobs()?;
        Ok(store)
    }

    pub fn create_fake_job(&self, behavior: FakeBehavior) -> Result<JobSummary, WorkspaceError> {
        let job_id = Uuid::new_v4().to_string();
        let job_dir = self.staging_dir().join(&job_id);
        let published_dir = self.root.join(&job_id);
        fs::create_dir(&job_dir)?;
        fs::create_dir(job_dir.join("final"))?;

        let created_at_ms = now_ms();
        let input_path = published_dir.join("input.txt");
        let output_path = published_dir.join("final/result.txt");
        write_file_synced(&job_dir.join("input.txt"), b"Zoos Upscale fake input\n")?;

        let job_spec = ProductJobSpec {
            schema_version: 2,
            job_id: job_id.clone(),
            kind: JobKind::FakeValidation,
            input_name: Some("input.txt".into()),
            output_path: Some(output_path.clone()),
            image_settings: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            scenario: Some(behavior),
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
            output: RunnerOutput {
                path: output_path.clone(),
            },
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
            kind: JobKind::FakeValidation,
            input_name: Some("input.txt".into()),
            output_path: Some(output_path),
            image_settings: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            scenario: Some(behavior),
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
            actual_backend: None,
            actual_device: None,
            runtime_sha256: None,
            model_param_sha256: None,
            model_bin_sha256: None,
            model_onnx_sha256: None,
            fallback_reason: None,
        };

        write_json_atomic(&job_dir.join(JOB_SPEC_FILE), &job_spec)?;
        write_json_atomic(&job_dir.join(PLAN_FILE), &plan)?;
        write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &runner_request)?;
        write_json_atomic(&job_dir.join(PROGRESS_FILE), &progress)?;
        write_json_atomic(&job_dir.join(MANIFEST_FILE), &manifest)?;
        create_empty_file(&job_dir.join(LOGS_FILE))?;
        create_empty_file(&job_dir.join(PLAN_REVISIONS_FILE))?;

        sync_directory(&job_dir.join("final"))?;
        sync_directory(&job_dir)?;
        fs::rename(&job_dir, &published_dir)?;
        sync_directory(&self.staging_dir())?;
        sync_directory(&self.root)?;

        Ok(summary)
    }

    pub fn create_image_job(
        &self,
        input_path: &Path,
        settings: ImageSettings,
    ) -> Result<JobSummary, WorkspaceError> {
        let job_id = Uuid::new_v4().to_string();
        let output_plan = plan_image_output(input_path, settings.scale, &job_id)?;
        let job_dir = self.staging_dir().join(&job_id);
        let published_dir = self.root.join(&job_id);
        fs::create_dir(&job_dir)?;

        let created_at_ms = now_ms();
        let input_name = output_plan
            .input
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ImageSafetyError::InvalidInputPath)?
            .to_owned();
        let job_spec = ProductJobSpec {
            schema_version: 2,
            job_id: job_id.clone(),
            kind: JobKind::ImageUpscale,
            input_name: Some(input_name.clone()),
            output_path: Some(output_plan.final_path.clone()),
            image_settings: Some(settings),
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            scenario: None,
            created_at_ms,
        };
        let plan = JobPlan {
            schema_version: 1,
            job_id: job_id.clone(),
            execution_backend: "process".into(),
            runner_id: "zoos-runner-realesrgan".into(),
        };
        let (preset, model_id) = match settings.preset {
            ImagePreset::Photo => (ProtocolImagePreset::Photo, ImageModelId::RealEsrganX4plus),
            ImagePreset::Anime => (
                ProtocolImagePreset::Anime,
                ImageModelId::RealEsrganX4plusAnime,
            ),
        };
        let runner_request = ImageUpscaleJobRequest {
            protocol_version: 1,
            job_id: job_id.clone(),
            task: ImageTask::ImageUpscale,
            input: ImageRunnerInput {
                path: output_plan.input.path.clone(),
                sha256: output_plan.input.sha256.clone(),
                width: output_plan.input.width,
                height: output_plan.input.height,
                format: output_plan.input.format,
            },
            output: ImageRunnerOutput {
                path: output_plan.partial_path.clone(),
                format: ImageOutputFormat::Png,
            },
            parameters: ImageUpscaleParameters {
                preset,
                model_id,
                scale: settings.scale,
                tile_size: 256,
                gpu_id: 0,
                threads: "1:2:2".into(),
            },
        };
        runner_request
            .validate()
            .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;

        let summary = JobSummary {
            job_id: job_id.clone(),
            kind: JobKind::ImageUpscale,
            input_name: Some(input_name),
            output_path: Some(output_plan.final_path),
            image_settings: Some(settings),
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            scenario: None,
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
            runner_id: "zoos-runner-realesrgan".into(),
            runner_version: env!("CARGO_PKG_VERSION").into(),
            result: None,
            exit_code: None,
            started_at_ms: None,
            finished_at_ms: None,
            actual_backend: None,
            actual_device: None,
            runtime_sha256: None,
            model_param_sha256: None,
            model_bin_sha256: None,
            model_onnx_sha256: None,
            fallback_reason: None,
        };

        write_json_atomic(&job_dir.join(JOB_SPEC_FILE), &job_spec)?;
        write_json_atomic(&job_dir.join(PLAN_FILE), &plan)?;
        write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &runner_request)?;
        write_json_atomic(&job_dir.join(PROGRESS_FILE), &progress)?;
        write_json_atomic(&job_dir.join(MANIFEST_FILE), &manifest)?;
        create_empty_file(&job_dir.join(LOGS_FILE))?;
        create_empty_file(&job_dir.join(PLAN_REVISIONS_FILE))?;
        sync_directory(&job_dir)?;
        fs::rename(&job_dir, &published_dir)?;
        sync_directory(&self.staging_dir())?;
        sync_directory(&self.root)?;
        Ok(summary)
    }

    pub fn list_jobs(&self) -> Result<Vec<JobSummary>, WorkspaceError> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("could not inspect workspace entry: {error}");
                    continue;
                }
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if Uuid::parse_str(&name).is_err() {
                continue;
            }
            if let Err(error) = self.validate_job_directory(&entry.path(), &name) {
                if let Err(quarantine_error) = self.quarantine(&entry.path(), &error.to_string()) {
                    eprintln!(
                        "could not quarantine corrupt job {name}: {quarantine_error}; original error: {error}"
                    );
                }
                continue;
            }
            let progress: JobProgress = read_json(&entry.path().join(PROGRESS_FILE))?;
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
        let plan: JobPlan = read_json(&job_dir.join(PLAN_FILE))?;
        let runner_request = self.read_runner_request(&job_dir, &progress.summary)?;
        let runner_id = if plan.runner_id.trim().is_empty() {
            default_runner_id(progress.summary.kind).into()
        } else {
            plan.runner_id
        };

        Ok(StoredJob {
            progress,
            runner_request,
            runner_job_path: job_dir.join(RUNNER_JOB_FILE),
            runner_id,
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
        let stored = self.load_stored_job(job_id)?;
        match stored.runner_request {
            StoredRunnerRequest::Fake(_) => {
                remove_if_exists(&job_dir.join("final/result.txt"))?;
                remove_if_exists(&job_dir.join("final").join(format!(".{job_id}.partial")))?;
            }
            StoredRunnerRequest::ImageUpscale(request) => {
                let plan = image_plan_from_request(job_id, &stored.progress.summary, request)?;
                cleanup_owned_output(&plan, &job_dir.join(VERIFICATION_FILE))?;
            }
            StoredRunnerRequest::ImageUpscaleV2(request) => {
                remove_if_exists(&request.output.path)?;
                remove_if_exists(&job_dir.join(VERIFICATION_FILE))?;
            }
        }
        Ok(())
    }

    pub fn recheck_image_input(&self, job_id: &str) -> Result<(), WorkspaceError> {
        let stored = self.load_stored_job(job_id)?;
        match stored.runner_request {
            StoredRunnerRequest::ImageUpscale(request) => {
                let plan = image_plan_from_request(job_id, &stored.progress.summary, request)?;
                recheck_input(&plan.input)?;
                Ok(())
            }
            StoredRunnerRequest::ImageUpscaleV2(request) => {
                let actual = crate::image_safety::sha256_file(&request.input.path)?;
                if actual == request.input.sha256 {
                    Ok(())
                } else {
                    Err(ImageSafetyError::InputChanged.into())
                }
            }
            StoredRunnerRequest::Fake(_) => Ok(()),
        }
    }

    pub fn publish_image_output(
        &self,
        job_id: &str,
    ) -> Result<Option<ImageVerification>, WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        let stored = self.load_stored_job(job_id)?;
        match stored.runner_request {
            StoredRunnerRequest::ImageUpscale(request) => {
                let plan = image_plan_from_request(job_id, &stored.progress.summary, request)?;
                Ok(Some(publish_verified_output(
                    &plan,
                    &job_dir.join(VERIFICATION_FILE),
                )?))
            }
            StoredRunnerRequest::ImageUpscaleV2(_) => Err(WorkspaceError::UnsafeRunnerRequest),
            StoredRunnerRequest::Fake(_) => Ok(None),
        }
    }

    pub fn recover_interrupted(&self) -> Result<(), WorkspaceError> {
        for job in self.list_jobs()? {
            if job.status.is_active() {
                self.cleanup_unverified_output(&job.job_id)?;
                self.update_summary(&job.job_id, |summary| {
                    summary.status = JobStatus::Interrupted;
                    summary.stage = None;
                    summary.message = "Interrupted during the previous app session".into();
                })?;
            }
        }
        Ok(())
    }

    fn recover_staging(&self) -> Result<(), WorkspaceError> {
        for entry in fs::read_dir(self.staging_dir())? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("could not inspect staging workspace: {error}");
                    continue;
                }
            };
            if let Err(error) = self.quarantine(
                &entry.path(),
                "incomplete staging workspace found during startup",
            ) {
                eprintln!(
                    "could not quarantine incomplete staging workspace {}: {error}",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }

    fn recover_corrupt_jobs(&self) -> Result<(), WorkspaceError> {
        for entry in fs::read_dir(&self.root)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("could not inspect workspace entry during recovery: {error}");
                    continue;
                }
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if Uuid::parse_str(&name).is_err() {
                continue;
            }
            if let Err(error) = self.validate_job_directory(&entry.path(), &name)
                && let Err(quarantine_error) = self.quarantine(&entry.path(), &error.to_string())
            {
                eprintln!(
                    "could not quarantine corrupt job {name}: {quarantine_error}; original error: {error}"
                );
            }
        }
        Ok(())
    }

    fn validate_job_directory(&self, job_dir: &Path, job_id: &str) -> Result<(), WorkspaceError> {
        let metadata = fs::symlink_metadata(job_dir)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }

        for required_file in [
            JOB_SPEC_FILE,
            PLAN_FILE,
            PROGRESS_FILE,
            MANIFEST_FILE,
            RUNNER_JOB_FILE,
            LOGS_FILE,
            PLAN_REVISIONS_FILE,
        ] {
            require_regular_file(&job_dir.join(required_file))?;
        }

        let spec: ProductJobSpec = read_json(&job_dir.join(JOB_SPEC_FILE))?;
        let plan: JobPlan = read_json(&job_dir.join(PLAN_FILE))?;
        let progress: JobProgress = read_json(&job_dir.join(PROGRESS_FILE))?;
        let manifest: JobManifest = read_json(&job_dir.join(MANIFEST_FILE))?;
        let runner_request = self.read_runner_request(job_dir, &progress.summary)?;

        if spec.job_id != job_id
            || plan.job_id != job_id
            || progress.summary.job_id != job_id
            || manifest.job_id != job_id
            || spec.kind != progress.summary.kind
        {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        match runner_request {
            StoredRunnerRequest::Fake(request)
                if request.job_id == job_id
                    && request.output.path == job_dir.join("final/result.txt") => {}
            StoredRunnerRequest::ImageUpscale(request)
                if request.job_id == job_id
                    && progress.summary.output_path.as_ref() != Some(&request.output.path)
                    && is_safe_image_request(&progress.summary, &request) => {}
            StoredRunnerRequest::ImageUpscaleV2(request)
                if request.job_id == job_id
                    && progress.summary.output_path.as_ref() != Some(&request.output.path)
                    && is_safe_image_request_v2(&progress.summary, &request) => {}
            _ => return Err(WorkspaceError::UnsafeRunnerRequest),
        }

        recover_jsonl(&job_dir.join(LOGS_FILE))?;
        recover_jsonl(&job_dir.join(PLAN_REVISIONS_FILE))?;
        Ok(())
    }

    fn read_runner_request(
        &self,
        job_dir: &Path,
        summary: &JobSummary,
    ) -> Result<StoredRunnerRequest, WorkspaceError> {
        match summary.kind {
            JobKind::FakeValidation => {
                let request: FakeJobRequest = read_json(&job_dir.join(RUNNER_JOB_FILE))?;
                request
                    .validate()
                    .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;
                if request.job_id != summary.job_id
                    || request.output.path != job_dir.join("final/result.txt")
                {
                    return Err(WorkspaceError::UnsafeRunnerRequest);
                }
                Ok(StoredRunnerRequest::Fake(request))
            }
            JobKind::ImageUpscale => {
                let version: RunnerRequestVersion = read_json(&job_dir.join(RUNNER_JOB_FILE))?;
                match version.protocol_version {
                    1 => {
                        let request: ImageUpscaleJobRequest =
                            read_json(&job_dir.join(RUNNER_JOB_FILE))?;
                        request.validate().map_err(|error| {
                            WorkspaceError::InvalidRunnerContract(error.to_string())
                        })?;
                        if request.job_id != summary.job_id
                            || summary.image_settings.is_none()
                            || !is_safe_image_request(summary, &request)
                        {
                            return Err(WorkspaceError::UnsafeRunnerRequest);
                        }
                        Ok(StoredRunnerRequest::ImageUpscale(request))
                    }
                    2 => {
                        let request: ImageUpscaleJobRequestV2 =
                            read_json(&job_dir.join(RUNNER_JOB_FILE))?;
                        request.validate().map_err(|error| {
                            WorkspaceError::InvalidRunnerContract(error.to_string())
                        })?;
                        if request.job_id != summary.job_id
                            || summary.image_settings.is_none()
                            || !is_safe_image_request_v2(summary, &request)
                        {
                            return Err(WorkspaceError::UnsafeRunnerRequest);
                        }
                        Ok(StoredRunnerRequest::ImageUpscaleV2(request))
                    }
                    version => Err(WorkspaceError::InvalidRunnerContract(format!(
                        "unsupported protocol version: {version}"
                    ))),
                }
            }
        }
    }

    fn quarantine(&self, path: &Path, reason: &str) -> Result<(), WorkspaceError> {
        let original_name = path
            .file_name()
            .ok_or(WorkspaceError::UnsafeRunnerRequest)?
            .to_string_lossy();
        let quarantine_name = format!("{original_name}-{}", Uuid::new_v4());
        let destination = self.quarantine_dir().join(&quarantine_name);
        fs::rename(path, &destination)?;

        let diagnostic = QuarantineDiagnostic {
            schema_version: 1,
            original_name: original_name.into_owned(),
            quarantined_name: quarantine_name.clone(),
            reason: reason.into(),
            quarantined_at_ms: now_ms(),
        };
        write_json_atomic(
            &self
                .quarantine_dir()
                .join(format!("{quarantine_name}{DIAGNOSTIC_SUFFIX}")),
            &diagnostic,
        )?;
        sync_directory(&self.quarantine_dir())?;
        Ok(())
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root.join(QUARANTINE_DIR)
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

fn image_plan_from_request(
    job_id: &str,
    summary: &JobSummary,
    request: ImageUpscaleJobRequest,
) -> Result<ImageOutputPlan, WorkspaceError> {
    let final_path = summary
        .output_path
        .clone()
        .ok_or(WorkspaceError::UnsafeRunnerRequest)?;
    let scale = request.parameters.scale;
    let output_width = request
        .input
        .width
        .checked_mul(u32::from(scale))
        .ok_or(WorkspaceError::UnsafeRunnerRequest)?;
    let output_height = request
        .input
        .height
        .checked_mul(u32::from(scale))
        .ok_or(WorkspaceError::UnsafeRunnerRequest)?;
    Ok(ImageOutputPlan {
        job_id: job_id.into(),
        input: crate::image_safety::ValidatedImageInput {
            path: request.input.path,
            sha256: request.input.sha256,
            format: request.input.format,
            width: request.input.width,
            height: request.input.height,
        },
        scale,
        output_width,
        output_height,
        final_path,
        partial_path: request.output.path,
    })
}

fn default_runner_id(kind: JobKind) -> &'static str {
    match kind {
        JobKind::FakeValidation => "zoos-runner-fake",
        JobKind::ImageUpscale => "zoos-runner-realesrgan",
    }
}

fn is_safe_image_request(summary: &JobSummary, request: &ImageUpscaleJobRequest) -> bool {
    let Some(final_path) = summary.output_path.as_deref() else {
        return false;
    };
    let Some(input_parent) = request.input.path.parent() else {
        return false;
    };
    if final_path.parent() != Some(input_parent.join("Upscaled").as_path()) {
        return false;
    }
    let Some(stem) = request
        .input
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
    else {
        return false;
    };
    let Some(final_name) = final_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let base = format!("{stem}_upscaled_{}x", request.parameters.scale);
    let name_is_valid = final_name == format!("{base}.png")
        || (2..=999).any(|suffix| final_name == format!("{base}_{suffix}.png"));
    if !name_is_valid {
        return false;
    }
    request.output.path
        == final_path.with_file_name(format!(".{final_name}.zoos-{}.partial.png", summary.job_id))
}

fn is_safe_image_request_v2(summary: &JobSummary, request: &ImageUpscaleJobRequestV2) -> bool {
    let Some(final_path) = summary.output_path.as_deref() else {
        return false;
    };
    let Some(final_name) = final_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    request.input.path != request.output.path
        && request.output.path
            == final_path.with_file_name(format!(
                ".{final_name}.zoos-{}.native-x4.partial.png",
                summary.job_id
            ))
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
    require_regular_file(path)?;
    let file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkspaceError::JobNotFound(path.display().to_string())
        } else {
            WorkspaceError::Io(error)
        }
    })?;
    Ok(serde_json::from_reader(BufReader::new(file))?)
}

fn require_regular_file(path: &Path) -> Result<(), WorkspaceError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkspaceError::JobNotFound(path.display().to_string())
        } else {
            WorkspaceError::Io(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorkspaceError::UnsafeRunnerRequest);
    }
    Ok(())
}

fn create_empty_file(path: &Path) -> Result<(), WorkspaceError> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

fn write_file_synced(path: &Path, contents: &[u8]) -> Result<(), WorkspaceError> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
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

fn recover_jsonl(path: &Path) -> Result<(), WorkspaceError> {
    require_regular_file(path)?;
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(());
    }

    let ends_with_newline = bytes.ends_with(b"\n");
    let complete_end = if ends_with_newline {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    };

    if complete_end > 0 {
        for line in bytes[..complete_end - 1].split(|byte| *byte == b'\n') {
            serde_json::from_slice::<serde_json::Value>(line)?;
        }
    }

    if ends_with_newline {
        return Ok(());
    }

    let tail = &bytes[complete_end..];
    if serde_json::from_slice::<serde_json::Value>(tail).is_ok() {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
    } else {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(complete_end as u64)?;
        file.sync_data()?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), WorkspaceError> {
    File::open(path)?.sync_all()?;
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

#[derive(Debug, Serialize, Deserialize)]
struct QuarantineDiagnostic {
    schema_version: u32,
    original_name: String,
    quarantined_name: String,
    reason: String,
    quarantined_at_ms: u64,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace root must be absolute")]
    RootMustBeAbsolute,
    #[error("workspace is already open by another process")]
    WorkspaceLocked,
    #[error("invalid job id")]
    InvalidJobId,
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("runner job points outside its managed workspace")]
    UnsafeRunnerRequest,
    #[error("invalid runner contract: {0}")]
    InvalidRunnerContract(String),
    #[error(transparent)]
    Image(#[from] ImageSafetyError),
    #[error("workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    fn quarantine_entries(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root.join(QUARANTINE_DIR))
            .expect("quarantine directory must be readable")
            .map(|entry| entry.expect("quarantine entry must be readable").path())
            .collect()
    }

    fn rgb_png(path: &Path, width: u32, height: u32, pixel: [u8; 3]) {
        RgbImage::from_pixel(width, height, Rgb(pixel))
            .save_with_format(path, ImageFormat::Png)
            .expect("RGB PNG fixture must save");
    }

    #[test]
    fn image_workspace_round_trips_and_publishes_verified_output() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("원본 사진.png");
        rgb_png(&input, 2, 3, [1, 2, 3]);
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_image_job(
                &input,
                ImageSettings {
                    preset: ImagePreset::Photo,
                    scale: 2,
                    backend: crate::domain::ImageBackend::Auto,
                    output_format: crate::domain::ImageOutputFormat::Png,
                    metadata: crate::domain::MetadataPolicy::Preserve,
                },
            )
            .expect("image job must be created");

        assert_eq!(created.kind, JobKind::ImageUpscale);
        assert_eq!(created.input_name.as_deref(), Some("원본 사진.png"));
        assert_eq!(created.scenario, None);
        let stored = store
            .load_stored_job(&created.job_id)
            .expect("image job must load");
        let StoredRunnerRequest::ImageUpscale(request) = stored.runner_request else {
            panic!("image request must retain its kind")
        };
        assert_eq!(request.input.path, input);
        assert_eq!(request.parameters.scale, 2);
        assert_eq!(request.output.format, ImageOutputFormat::Png);
        let final_path = created
            .output_path
            .as_ref()
            .expect("summary must expose final output");
        assert_ne!(&request.output.path, final_path);
        let final_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("final name must be UTF-8");
        assert_eq!(
            request.output.path,
            final_path.with_file_name(format!(".{final_name}.zoos-{}.partial.png", created.job_id))
        );
        rgb_png(&request.output.path, 4, 6, [8, 9, 10]);

        let verification = store
            .publish_image_output(&created.job_id)
            .expect("publish must succeed")
            .expect("image publish must return verification");
        assert_eq!(verification.output_path.as_path(), final_path.as_path());
        assert!(verification.output_path.is_file());
        assert!(
            directory
                .path()
                .join(&created.job_id)
                .join(VERIFICATION_FILE)
                .is_file()
        );
        assert_eq!(store.list_jobs().expect("jobs must list").len(), 1);
    }

    #[test]
    fn image_v2_runner_request_and_selected_plan_runner_load() {
        use zoos_runner_protocol::{
            IMAGE_PROTOCOL_VERSION_V2, ImageBackendSettingsV2, ImageDeviceV2,
            ImageInferenceFormatV2, ImageInferenceInputV2, ImageIntermediateOutputV2,
            ImagePixelFormatV2, ImageSemanticModelV2, ImageUpscaleJobRequestV2,
            ImageUpscaleParametersV2,
        };

        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("input.png");
        rgb_png(&input, 2, 3, [1, 2, 3]);
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_image_job(
                &input,
                ImageSettings {
                    preset: ImagePreset::Photo,
                    scale: 2,
                    backend: crate::domain::ImageBackend::OrtCpu,
                    output_format: crate::domain::ImageOutputFormat::Png,
                    metadata: crate::domain::MetadataPolicy::Preserve,
                },
            )
            .expect("image job must be created");
        let job_dir = directory.path().join(&created.job_id);
        let StoredRunnerRequest::ImageUpscale(v1) = store
            .load_stored_job(&created.job_id)
            .expect("v1 image job must load")
            .runner_request
        else {
            panic!("new job must initially use v1")
        };
        let final_path = created.output_path.as_ref().expect("final path must exist");
        let final_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("final name must be UTF-8");
        let v2 = ImageUpscaleJobRequestV2 {
            protocol_version: IMAGE_PROTOCOL_VERSION_V2,
            job_id: created.job_id.clone(),
            task: ImageTask::ImageUpscale,
            input: ImageInferenceInputV2 {
                path: v1.input.path,
                sha256: v1.input.sha256,
                width: v1.input.width,
                height: v1.input.height,
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            output: ImageIntermediateOutputV2 {
                path: final_path.with_file_name(format!(
                    ".{final_name}.zoos-{}.native-x4.partial.png",
                    created.job_id
                )),
                format: ImageInferenceFormatV2::Png,
                pixel_format: ImagePixelFormatV2::Rgb8,
            },
            parameters: ImageUpscaleParametersV2 {
                semantic_model: ImageSemanticModelV2::Photo,
                requested_scale: 2,
                native_scale: 4,
                device: ImageDeviceV2::Cpu,
                backend_settings: ImageBackendSettingsV2::OrtCpu {
                    tile_size: 128,
                    intra_threads: 4,
                    inter_threads: 1,
                },
            },
        };
        write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &v2)
            .expect("v2 runner request must save");
        let mut plan: JobPlan = read_json(&job_dir.join(PLAN_FILE)).expect("plan must deserialize");
        plan.runner_id = "zoos-runner-ort".into();
        write_json_atomic(&job_dir.join(PLAN_FILE), &plan).expect("plan must save");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("v2 workspace must open");
        let stored = reopened
            .load_stored_job(&created.job_id)
            .expect("v2 image job must load");
        assert_eq!(stored.runner_id, "zoos-runner-ort");
        assert!(matches!(
            stored.runner_request,
            StoredRunnerRequest::ImageUpscaleV2(_)
        ));
    }

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
    fn v1_fake_progress_is_upgraded_in_memory() {
        let progress: JobProgress = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "job_id": "fcd11d64-7a52-4aa2-915f-0dd0ea345868",
            "scenario": "success",
            "status": "CREATED",
            "progress_percent": 0,
            "stage": null,
            "message": "Ready to start",
            "error": null,
            "created_at_ms": 1,
            "updated_at_ms": 1
        }))
        .expect("v1 fake progress must deserialize");

        assert_eq!(progress.summary.kind, JobKind::FakeValidation);
        assert_eq!(progress.summary.scenario, Some(FakeBehavior::Success));
        assert_eq!(progress.summary.image_settings, None);
    }

    #[test]
    fn complete_v1_fake_workspace_loads_without_being_rewritten() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");
        let job_dir = directory.path().join(&created.job_id);
        drop(store);

        let v1_spec = serde_json::json!({
            "schema_version": 1,
            "job_id": created.job_id.clone(),
            "kind": "fake_validation",
            "scenario": "success",
            "created_at_ms": created.created_at_ms
        });
        let v1_progress = serde_json::json!({
            "schema_version": 1,
            "job_id": created.job_id.clone(),
            "scenario": "success",
            "status": "CREATED",
            "progress_percent": 0,
            "stage": null,
            "message": "Ready to start",
            "error": null,
            "created_at_ms": created.created_at_ms,
            "updated_at_ms": created.updated_at_ms
        });
        fs::write(
            job_dir.join(JOB_SPEC_FILE),
            serde_json::to_vec_pretty(&v1_spec).expect("v1 spec must serialize"),
        )
        .expect("v1 spec must be written");
        let v1_progress_bytes =
            serde_json::to_vec_pretty(&v1_progress).expect("v1 progress must serialize");
        fs::write(job_dir.join(PROGRESS_FILE), &v1_progress_bytes)
            .expect("v1 progress must be written");

        let reopened = WorkspaceStore::new(directory.path()).expect("v1 workspace must open");
        let listed = reopened.list_jobs().expect("v1 job must list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, JobKind::FakeValidation);
        assert_eq!(listed[0].scenario, Some(FakeBehavior::Success));
        reopened
            .load_stored_job(&listed[0].job_id)
            .expect("v1 runner request must load");
        assert_eq!(
            fs::read(job_dir.join(PROGRESS_FILE)).expect("v1 progress must remain"),
            v1_progress_bytes
        );
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

    #[test]
    fn image_recovery_removes_owned_partial_but_preserves_raced_final() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let input_directory = tempfile::tempdir().expect("input directory must be created");
        let input = input_directory.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_image_job(
                &input,
                ImageSettings {
                    preset: ImagePreset::Anime,
                    scale: 2,
                    backend: crate::domain::ImageBackend::Auto,
                    output_format: crate::domain::ImageOutputFormat::Png,
                    metadata: crate::domain::MetadataPolicy::Preserve,
                },
            )
            .expect("image job must be created");
        let stored = store
            .load_stored_job(&created.job_id)
            .expect("image job must load");
        let StoredRunnerRequest::ImageUpscale(request) = stored.runner_request else {
            panic!("image runner request must load")
        };
        rgb_png(&request.output.path, 4, 4, [4, 5, 6]);
        let final_path = created.output_path.expect("final output must be planned");
        fs::write(&final_path, b"raced final").expect("raced final must save");
        store
            .update_summary(&created.job_id, |summary| {
                summary.status = JobStatus::Running;
            })
            .expect("job must become active");

        store.recover_interrupted().expect("recovery must complete");

        assert!(!request.output.path.exists());
        assert_eq!(
            fs::read(final_path).expect("raced final must remain"),
            b"raced final"
        );
        assert_eq!(
            store
                .load_summary(&created.job_id)
                .expect("summary must load")
                .status,
            JobStatus::Interrupted
        );
    }

    #[test]
    fn workspace_lock_is_held_for_store_lifetime() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");

        assert!(matches!(
            WorkspaceStore::new(directory.path()),
            Err(WorkspaceError::WorkspaceLocked)
        ));

        drop(store);
        WorkspaceStore::new(directory.path()).expect("lock must be released with the store");
    }

    #[test]
    fn job_is_published_from_staging_as_a_complete_uuid_directory() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");

        assert!(directory.path().join(&created.job_id).is_dir());
        assert!(
            fs::read_dir(directory.path().join(STAGING_DIR))
                .expect("staging directory must be readable")
                .next()
                .is_none()
        );
        for required in [
            JOB_SPEC_FILE,
            PLAN_FILE,
            RUNNER_JOB_FILE,
            PROGRESS_FILE,
            MANIFEST_FILE,
            LOGS_FILE,
            PLAN_REVISIONS_FILE,
        ] {
            assert!(
                directory
                    .path()
                    .join(&created.job_id)
                    .join(required)
                    .is_file()
            );
        }
    }

    #[test]
    fn startup_quarantines_incomplete_staging_with_diagnostic() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let staged = store.staging_dir().join(Uuid::new_v4().to_string());
        fs::create_dir(&staged).expect("incomplete staging directory must be created");
        fs::write(staged.join("partial"), b"unfinished").expect("partial file must be written");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("recovery must succeed");
        assert!(
            fs::read_dir(reopened.staging_dir())
                .expect("staging directory must be readable")
                .next()
                .is_none()
        );
        let entries = quarantine_entries(directory.path());
        assert_eq!(entries.iter().filter(|path| path.is_dir()).count(), 1);
        assert_eq!(
            entries
                .iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .count(),
            1
        );
    }

    #[test]
    fn corrupt_job_is_quarantined_without_hiding_good_jobs() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let good = store
            .create_fake_job(FakeBehavior::Success)
            .expect("good job must be created");
        let corrupt = store
            .create_fake_job(FakeBehavior::Failed)
            .expect("corrupt job fixture must be created");
        fs::write(
            directory.path().join(&corrupt.job_id).join(PROGRESS_FILE),
            b"not json",
        )
        .expect("progress must be corrupted");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("startup must recover");
        let jobs = reopened
            .list_jobs()
            .expect("good jobs must remain listable");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, good.job_id);
        assert!(!directory.path().join(&corrupt.job_id).exists());
        assert_eq!(
            quarantine_entries(directory.path())
                .iter()
                .filter(|path| path.is_dir())
                .count(),
            1
        );
    }

    #[test]
    fn startup_truncates_only_an_incomplete_jsonl_tail() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");
        let logs = directory.path().join(&created.job_id).join(LOGS_FILE);
        fs::write(&logs, b"{\"sequence\":1}\n{\"sequence\":")
            .expect("interrupted log must be written");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("startup must recover tail");
        assert_eq!(
            fs::read(&logs).expect("log must remain"),
            b"{\"sequence\":1}\n"
        );
        assert_eq!(reopened.list_jobs().expect("job must remain").len(), 1);
        assert!(quarantine_entries(directory.path()).is_empty());
    }

    #[test]
    fn malformed_jsonl_before_later_records_quarantines_job() {
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");
        let logs = directory.path().join(&created.job_id).join(LOGS_FILE);
        fs::write(&logs, b"{\"sequence\":1}\nnot-json\n{\"sequence\":3}\n")
            .expect("corrupt log must be written");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("startup must continue");
        assert!(
            reopened
                .list_jobs()
                .expect("listing must continue")
                .is_empty()
        );
        assert!(!directory.path().join(&created.job_id).exists());
        assert_eq!(
            quarantine_entries(directory.path())
                .iter()
                .filter(|path| path.is_dir())
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_symlink_is_quarantined_without_touching_the_external_file() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let outside = tempfile::tempdir().expect("outside directory must be created");
        let external_log = outside.path().join("external.jsonl");
        let external_contents = b"{\"sequence\":1}\n{\"unfinished\":";
        fs::write(&external_log, external_contents).expect("external log must be written");

        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");
        let logs = directory.path().join(&created.job_id).join(LOGS_FILE);
        fs::remove_file(&logs).expect("managed log must be removed");
        symlink(&external_log, &logs).expect("test symlink must be created");
        drop(store);

        let reopened = WorkspaceStore::new(directory.path()).expect("startup must continue");
        assert!(
            reopened
                .list_jobs()
                .expect("listing must continue")
                .is_empty()
        );
        assert_eq!(
            fs::read(&external_log).expect("external log must remain readable"),
            external_contents
        );
        assert_eq!(
            quarantine_entries(directory.path())
                .iter()
                .filter(|path| path.is_dir())
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn job_directory_symlink_cannot_escape_workspace() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let outside = tempfile::tempdir().expect("outside directory must be created");
        let store = WorkspaceStore::new(directory.path()).expect("store must be created");
        let created = store
            .create_fake_job(FakeBehavior::Success)
            .expect("job must be created");
        let job_dir = directory.path().join(&created.job_id);
        fs::remove_dir_all(&job_dir).expect("temporary job directory must be removed");
        symlink(outside.path(), &job_dir).expect("test symlink must be created");

        assert!(matches!(
            store.load_summary(&created.job_id),
            Err(WorkspaceError::UnsafeRunnerRequest)
        ));
    }
}
