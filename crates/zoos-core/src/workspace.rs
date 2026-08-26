use std::collections::HashSet;
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
use zoos_media::{MediaDescriptor, verify_interpolated_output};
use zoos_runner_protocol::{
    FakeBehavior, FakeJobRequest, FakeParameters, FakeTask, IMAGE_PROTOCOL_VERSION_V2,
    ImageBackendSettingsV2, ImageDeviceV2, ImageInferenceFormatV2, ImageInferenceInputV2,
    ImageIntermediateOutputV2, ImageModelId, ImageOutputFormat, ImagePixelFormatV2,
    ImagePreset as ProtocolImagePreset, ImageRunnerInput, ImageRunnerOutput, ImageSemanticModelV2,
    ImageTask, ImageUpscaleJobRequest, ImageUpscaleJobRequestV2, ImageUpscaleParameters,
    ImageUpscaleParametersV2, MuxPlan, RifeModel, RunnerEvent, RunnerInput, RunnerOutput,
    VIDEO_PROTOCOL_VERSION, VideoDevice, VideoInterpolateJobRequest, VideoInterpolateParameters,
    VideoRunnerInput, VideoRunnerOutput, VideoWorkPaths,
};

use crate::domain::{
    ImageBackend, ImageBatchMetadata, ImageOutputFormat as ProductOutputFormat, ImagePreset,
    ImageSettings, JobKind, JobManifest, JobPlan, JobProgress, JobStatus, JobSummary,
    MetadataPolicy, ProductJobSpec, RationalRate, StoredJob, StoredRunnerRequest, VideoBackend,
    VideoSettings,
};
use crate::image_pipeline::{
    Goal1bImageError, ImageMetadata, ImagePipelineLimits, MetadataPolicy as PipelineMetadataPolicy,
    OutputEncoding, PreparedImage, VerifiedPipelineOutput, prepare_image_input,
    render_pipeline_output,
};
use crate::image_safety::{
    ImageOutputPlan, ImageSafetyError, ImageVerification, cleanup_owned_output, plan_image_output,
    publish_verified_output, recheck_input,
};
use crate::video_safety::{
    VideoOutputPlan, VideoPipelineVerification, VideoSafetyError, cleanup_owned_video_output,
    cleanup_video_work_directory, plan_video_output, publish_staged_video_output,
    recheck_video_input, stage_private_video_output, validate_published_video_output,
};

const JOB_SPEC_FILE: &str = "job-spec.json";
const PLAN_FILE: &str = "plan.json";
const RUNNER_JOB_FILE: &str = "runner-job.json";
const PROGRESS_FILE: &str = "progress.json";
const MANIFEST_FILE: &str = "manifest.json";
const LOGS_FILE: &str = "logs.jsonl";
const PLAN_REVISIONS_FILE: &str = "plan-revisions.jsonl";
const VERIFICATION_FILE: &str = "verification.json";
const IMAGE_PIPELINE_FILE: &str = "image-pipeline.json";
const MEDIA_DESCRIPTOR_FILE: &str = "media-descriptor.json";
const MUX_PLAN_FILE: &str = "mux-plan.json";
const VIDEO_PIPELINE_FILE: &str = "video-pipeline.json";
const VIDEO_RUNNER_EVIDENCE_FILE: &str = "runner-evidence.json";
const LOCK_FILE: &str = ".workspace.lock";
const STAGING_DIR: &str = "staging";
const QUARANTINE_DIR: &str = "quarantine";
const DIAGNOSTIC_SUFFIX: &str = ".diagnostic.json";
const GPU_RUNTIME_SHA256: &str = "c1c35d92079085de96b9d547fd7e4464bc8a2e9ccf28d7b8c712d72ade91b7cc";
const PHOTO_PARAM_SHA256: &str = "35330ececcea33b6c397a72548e788d5d53becee4734c50b7fada36e89f10a86";
const PHOTO_BIN_SHA256: &str = "713ee713b0353afaa27976f0563a64a5043bd70b9bd8936c2e26e25ebcdbcddf";
const ANIME_PARAM_SHA256: &str = "2b8fb6e0ae4d2d85704ca08c119a2f5ea40add4f2ecd512eb7f4cd44b6127ed4";
const ANIME_BIN_SHA256: &str = "fe01c269cfd10cdef8e018ab66ebe750cf79c7af4d1f9c16c737e1295229bacc";
const ORT_RUNTIME_SHA256: &str = "68f6e54e695583adc371aef610ec4abb1ffaa3df656582922de7690f7e2000eb";
const PHOTO_ONNX_SHA256: &str = "95c08dbcaa58b4fabae771e74ae458d93df59b86cdcb885b85ade5be4e7f826b";
const ANIME_ONNX_SHA256: &str = "8244ce14b66d7f285f5ed4980ce53d098c9aa7c5533d8782a5deeb7217035eb1";
const FFMPEG_SHA256: &str = "653e700a788f3376ebc3817a3dcda56e111111410f7edd8eea919c4089216d4e";
const FFPROBE_SHA256: &str = "edaf9c5f53aef960ceb5f779d986e7dea86ee549e6716a2c03b70010b88a4da6";
const RIFE_ENGINE_SHA256: &str = "d11429c72f0cddfb170fd131ee9373dc5329a5729c4382c0acfd40092e5ed19a";
const RIFE_MODEL_PARAM_SHA256: &str =
    "724569596bcd1e7b9fa50455c604777ebed99746d2ef40aa86e31b5725f1053c";
const RIFE_MODEL_BIN_SHA256: &str =
    "f334ed2260149ce0188a6dcf049844e8b0cdd912e01cbcfb63553157d2508958";
const VIDEO_ENCODE_BITRATE_BITS_PER_SECOND: u64 = 12_000_000;

#[derive(Deserialize)]
struct RunnerRequestVersion {
    protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImagePipelinePlan {
    schema_version: u32,
    job_id: String,
    source_path: PathBuf,
    source_sha256: String,
    inference_png: PathBuf,
    alpha_png: Option<PathBuf>,
    native_x4_png: PathBuf,
    destination_partial: PathBuf,
    destination_final: PathBuf,
    width: u32,
    height: u32,
    had_alpha: bool,
    orientation: u16,
    metadata: ImageMetadata,
    scale: u8,
    output_format: ProductOutputFormat,
    metadata_policy: MetadataPolicy,
    selected_backend: ImageBackend,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoPipelinePlan {
    schema_version: u32,
    job_id: String,
    descriptor: MediaDescriptor,
    mux_plan: MuxPlan,
    output: VideoOutputPlan,
    selected_backend: VideoBackend,
    chunk_frames: u32,
    scene_threshold_permille: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoRunnerEvidence {
    schema_version: u32,
    job_id: String,
    selected_backend: String,
    selected_device: String,
    source_frames: u64,
    output_frames: u64,
    chunk_count: u32,
    scene_cut_count: u64,
    ffmpeg_sha256: String,
    ffprobe_sha256: String,
    rife_sha256: String,
    model_sha256: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePipelineVerification {
    pub schema_version: u32,
    pub job_id: String,
    pub actual_backend: ImageBackend,
    pub source_path: PathBuf,
    pub source_sha256_before: String,
    pub source_sha256_after: String,
    pub inference_sha256: String,
    pub intermediate_sha256: String,
    pub output_path: PathBuf,
    pub output_sha256: String,
    pub output_format: ProductOutputFormat,
    pub output_width: u32,
    pub output_height: u32,
    pub alpha_preserved: bool,
    pub icc_preserved: bool,
    pub exif_preserved: bool,
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
            video_settings: None,
            source_rate: None,
            target_rate: None,
            video_container: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            selected_video_backend: None,
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
            video_settings: None,
            source_rate: None,
            target_rate: None,
            video_container: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            selected_video_backend: None,
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
            actual_video_backend: None,
            actual_device: None,
            runtime_sha256: None,
            model_param_sha256: None,
            model_bin_sha256: None,
            model_onnx_sha256: None,
            ffmpeg_sha256: None,
            ffprobe_sha256: None,
            rife_engine_sha256: None,
            rife_model_param_sha256: None,
            rife_model_bin_sha256: None,
            fallback_reason: None,
            source_sha256: None,
            intermediate_sha256: None,
            final_sha256: None,
            icc_preserved: None,
            exif_preserved: None,
            alpha_preserved: None,
            source_rate: None,
            target_rate: None,
            source_frames: None,
            output_frames: None,
            scene_cut_count: None,
            chunk_count: None,
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
            video_settings: None,
            source_rate: None,
            target_rate: None,
            video_container: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            selected_video_backend: None,
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
            video_settings: None,
            source_rate: None,
            target_rate: None,
            video_container: None,
            batch_id: None,
            batch_index: None,
            batch_total: None,
            selected_backend: None,
            selected_video_backend: None,
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
            actual_video_backend: None,
            actual_device: None,
            runtime_sha256: None,
            model_param_sha256: None,
            model_bin_sha256: None,
            model_onnx_sha256: None,
            ffmpeg_sha256: None,
            ffprobe_sha256: None,
            rife_engine_sha256: None,
            rife_model_param_sha256: None,
            rife_model_bin_sha256: None,
            fallback_reason: None,
            source_sha256: None,
            intermediate_sha256: None,
            final_sha256: None,
            icc_preserved: None,
            exif_preserved: None,
            alpha_preserved: None,
            source_rate: None,
            target_rate: None,
            source_frames: None,
            output_frames: None,
            scene_cut_count: None,
            chunk_count: None,
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

    pub fn create_image_job_v2(
        &self,
        input_path: &Path,
        settings: ImageSettings,
        selected_backend: ImageBackend,
        batch: Option<ImageBatchMetadata>,
    ) -> Result<JobSummary, WorkspaceError> {
        if selected_backend == ImageBackend::Auto {
            return Err(WorkspaceError::InvalidSelectedBackend);
        }
        if settings.backend != ImageBackend::Auto && settings.backend != selected_backend {
            return Err(WorkspaceError::InvalidSelectedBackend);
        }
        if let Some(batch) = batch.as_ref()
            && (batch.batch_id.trim().is_empty()
                || batch.total == 0
                || batch.index == 0
                || batch.index > batch.total)
        {
            return Err(WorkspaceError::InvalidBatchMetadata);
        }

        let job_id = Uuid::new_v4().to_string();
        let job_dir = self.staging_dir().join(&job_id);
        let published_dir = self.root.join(&job_id);
        let staging_work = job_dir.join("work");
        let published_work = published_dir.join("work");
        fs::create_dir(&job_dir)?;

        let create = (|| -> Result<JobSummary, WorkspaceError> {
            let source_sha256 = checked_source_sha256(input_path)?;
            let staged =
                prepare_image_input(input_path, &staging_work, ImagePipelineLimits::default())?;
            if checked_source_sha256(input_path)? != source_sha256 {
                return Err(ImageSafetyError::InputChanged.into());
            }
            validate_pipeline_dimensions(staged.width, staged.height, settings.scale)?;
            ensure_pipeline_workspace_space(&staging_work, staged.width, staged.height)?;
            if staged.had_alpha && settings.output_format == ProductOutputFormat::Jpeg {
                return Err(Goal1bImageError::AlphaJpegUnsupported.into());
            }

            let reserved_outputs = self.active_output_reservations()?;
            let (final_path, destination_partial) = plan_pipeline_destination(
                input_path,
                settings.scale,
                settings.output_format,
                &job_id,
                staged.width,
                staged.height,
                &reserved_outputs,
            )?;
            let input_name = input_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(ImageSafetyError::InvalidInputPath)?
                .to_owned();
            let inference_png = published_work.join("inference-rgb.png");
            let alpha_png = staged
                .alpha_png
                .as_ref()
                .map(|_| published_work.join("alpha.png"));
            let native_x4_png = published_work.join("sr-native-x4.png");
            let inference_sha256 = crate::image_safety::sha256_file(&staged.inference_png)?;
            let pipeline = ImagePipelinePlan {
                schema_version: 1,
                job_id: job_id.clone(),
                source_path: input_path.to_owned(),
                source_sha256: source_sha256.clone(),
                inference_png: inference_png.clone(),
                alpha_png,
                native_x4_png: native_x4_png.clone(),
                destination_partial: destination_partial.clone(),
                destination_final: final_path.clone(),
                width: staged.width,
                height: staged.height,
                had_alpha: staged.had_alpha,
                orientation: staged.orientation,
                metadata: staged.metadata,
                scale: settings.scale,
                output_format: settings.output_format,
                metadata_policy: settings.metadata,
                selected_backend,
            };
            let (device, backend_settings, runner_id) = match selected_backend {
                ImageBackend::VulkanGpu => (
                    ImageDeviceV2::Vulkan { index: 0 },
                    ImageBackendSettingsV2::Vulkan {
                        tile_size: 256,
                        threads: "1:2:2".into(),
                    },
                    "zoos-runner-realesrgan",
                ),
                ImageBackend::OrtCpu => (
                    ImageDeviceV2::Cpu,
                    ImageBackendSettingsV2::OrtCpu {
                        tile_size: 128,
                        intra_threads: 4,
                        inter_threads: 1,
                    },
                    "zoos-runner-ort",
                ),
                ImageBackend::Auto => unreachable!("validated above"),
            };
            let runner_request = ImageUpscaleJobRequestV2 {
                protocol_version: IMAGE_PROTOCOL_VERSION_V2,
                job_id: job_id.clone(),
                task: ImageTask::ImageUpscale,
                input: ImageInferenceInputV2 {
                    path: inference_png,
                    sha256: inference_sha256,
                    width: staged.width,
                    height: staged.height,
                    format: ImageInferenceFormatV2::Png,
                    pixel_format: ImagePixelFormatV2::Rgb8,
                },
                output: ImageIntermediateOutputV2 {
                    path: native_x4_png,
                    format: ImageInferenceFormatV2::Png,
                    pixel_format: ImagePixelFormatV2::Rgb8,
                },
                parameters: ImageUpscaleParametersV2 {
                    semantic_model: match settings.preset {
                        ImagePreset::Photo => ImageSemanticModelV2::Photo,
                        ImagePreset::Anime => ImageSemanticModelV2::Anime,
                    },
                    requested_scale: settings.scale,
                    native_scale: 4,
                    device,
                    backend_settings,
                },
            };
            runner_request
                .validate()
                .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;

            let created_at_ms = now_ms();
            let batch_id = batch.as_ref().map(|batch| batch.batch_id.clone());
            let batch_index = batch.as_ref().map(|batch| batch.index);
            let batch_total = batch.as_ref().map(|batch| batch.total);
            let job_spec = ProductJobSpec {
                schema_version: 3,
                job_id: job_id.clone(),
                kind: JobKind::ImageUpscale,
                input_name: Some(input_name.clone()),
                output_path: Some(final_path.clone()),
                image_settings: Some(settings),
                video_settings: None,
                source_rate: None,
                target_rate: None,
                video_container: None,
                batch_id: batch_id.clone(),
                batch_index,
                batch_total,
                selected_backend: Some(selected_backend),
                selected_video_backend: None,
                scenario: None,
                created_at_ms,
            };
            let plan = JobPlan {
                schema_version: 2,
                job_id: job_id.clone(),
                execution_backend: "process".into(),
                runner_id: runner_id.into(),
            };
            let summary = JobSummary {
                job_id: job_id.clone(),
                kind: JobKind::ImageUpscale,
                input_name: Some(input_name),
                output_path: Some(final_path),
                image_settings: Some(settings),
                video_settings: None,
                source_rate: None,
                target_rate: None,
                video_container: None,
                batch_id,
                batch_index,
                batch_total,
                selected_backend: Some(selected_backend),
                selected_video_backend: None,
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
                schema_version: 2,
                job_id: job_id.clone(),
                runner_id: runner_id.into(),
                runner_version: env!("CARGO_PKG_VERSION").into(),
                result: None,
                exit_code: None,
                started_at_ms: None,
                finished_at_ms: None,
                actual_backend: Some(selected_backend),
                actual_video_backend: None,
                actual_device: None,
                runtime_sha256: None,
                model_param_sha256: None,
                model_bin_sha256: None,
                model_onnx_sha256: None,
                ffmpeg_sha256: None,
                ffprobe_sha256: None,
                rife_engine_sha256: None,
                rife_model_param_sha256: None,
                rife_model_bin_sha256: None,
                fallback_reason: None,
                source_sha256: Some(source_sha256),
                intermediate_sha256: None,
                final_sha256: None,
                icc_preserved: None,
                exif_preserved: None,
                alpha_preserved: None,
                source_rate: None,
                target_rate: None,
                source_frames: None,
                output_frames: None,
                scene_cut_count: None,
                chunk_count: None,
            };

            write_json_atomic(&job_dir.join(JOB_SPEC_FILE), &job_spec)?;
            write_json_atomic(&job_dir.join(PLAN_FILE), &plan)?;
            write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &runner_request)?;
            write_json_atomic(&job_dir.join(IMAGE_PIPELINE_FILE), &pipeline)?;
            write_json_atomic(&job_dir.join(PROGRESS_FILE), &progress)?;
            write_json_atomic(&job_dir.join(MANIFEST_FILE), &manifest)?;
            create_empty_file(&job_dir.join(LOGS_FILE))?;
            create_empty_file(&job_dir.join(PLAN_REVISIONS_FILE))?;
            sync_directory(&staging_work)?;
            sync_directory(&job_dir)?;
            fs::rename(&job_dir, &published_dir)?;
            sync_directory(&self.staging_dir())?;
            sync_directory(&self.root)?;
            Ok(summary)
        })();

        if create.is_err()
            && job_dir.exists()
            && let Err(error) = self.quarantine(&job_dir, "Goal 1B image job creation failed")
        {
            eprintln!("could not quarantine failed Goal 1B staging job: {error}");
        }
        create
    }

    pub fn create_video_job(
        &self,
        descriptor: MediaDescriptor,
        settings: VideoSettings,
        selected_backend: VideoBackend,
    ) -> Result<JobSummary, WorkspaceError> {
        if selected_backend == VideoBackend::Auto
            || (settings.backend != VideoBackend::Auto && settings.backend != selected_backend)
        {
            return Err(WorkspaceError::InvalidSelectedVideoBackend);
        }
        let input_path = descriptor.input_path.clone();
        let job_id = Uuid::new_v4().to_string();
        let job_dir = self.staging_dir().join(&job_id);
        let published_dir = self.root.join(&job_id);
        let staging_work = job_dir.join("work");
        let published_work = published_dir.join("work");
        fs::create_dir(&job_dir)?;
        fs::create_dir(&staging_work)?;
        fs::create_dir(staging_work.join("input-frames"))?;
        fs::create_dir(staging_work.join("output-frames"))?;

        let create = (|| -> Result<JobSummary, WorkspaceError> {
            let mux_plan = descriptor.mux_plan();
            let source_rate = descriptor.frame_rate;
            let target_rate = RationalRate {
                numerator: source_rate
                    .numerator
                    .checked_mul(2)
                    .ok_or(WorkspaceError::UnsafeRunnerRequest)?,
                denominator: source_rate.denominator,
            };
            let frame_bytes = u64::from(descriptor.width)
                .saturating_mul(u64::from(descriptor.height))
                .saturating_mul(3);
            let bytes_per_interval = frame_bytes.saturating_mul(9).max(1);
            let chunk_frames =
                u32::try_from(((768 * 1024 * 1024_u64) / bytes_per_interval).clamp(1, 120))
                    .map_err(|_| WorkspaceError::UnsafeRunnerRequest)?;
            let bounded_frame_bytes = frame_bytes
                .saturating_mul(u64::from(chunk_frames).saturating_add(1))
                .saturating_mul(5);
            let input_size = fs::symlink_metadata(&input_path)?.len();
            let encoded_video_bytes = descriptor
                .duration_ms
                .saturating_mul(VIDEO_ENCODE_BITRATE_BITS_PER_SECOND)
                .div_ceil(8_000);
            let estimated_output_bytes = input_size
                .saturating_mul(3)
                .max(encoded_video_bytes.saturating_add(input_size));
            // The runner retains all encoded chunks while creating an equally sized joined
            // stream, then creates the private muxed output. Account for those accumulated
            // intermediates separately from the private + destination-side partial copies.
            let bounded_workspace_bytes =
                bounded_frame_bytes.saturating_add(estimated_output_bytes.saturating_mul(2));
            let output = plan_video_output(
                &input_path,
                descriptor.container,
                &job_id,
                &published_work,
                estimated_output_bytes,
                bounded_workspace_bytes,
                &self.active_output_reservations()?,
            )?;
            if descriptor.input_path != output.input.path
                || descriptor.width == 0
                || descriptor.height == 0
                || descriptor.frame_count < 2
            {
                return Err(WorkspaceError::UnsafeRunnerRequest);
            }
            let input_name = input_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(VideoSafetyError::InvalidInputPath)?
                .to_owned();
            let pipeline = VideoPipelinePlan {
                schema_version: 1,
                job_id: job_id.clone(),
                descriptor: descriptor.clone(),
                mux_plan: mux_plan.clone(),
                output: output.clone(),
                selected_backend,
                chunk_frames,
                scene_threshold_permille: 350,
            };
            let device = match selected_backend {
                VideoBackend::VulkanGpu => VideoDevice::Vulkan { index: 0 },
                VideoBackend::NcnnCpu => VideoDevice::NcnnCpu,
                VideoBackend::Auto => unreachable!("validated above"),
            };
            let runner_request = VideoInterpolateJobRequest {
                protocol_version: VIDEO_PROTOCOL_VERSION,
                job_id: job_id.clone(),
                task: zoos_runner_protocol::RunnerTask::VideoInterpolate,
                input: VideoRunnerInput {
                    path: input_path.clone(),
                    sha256: output.input.sha256.clone(),
                    width: descriptor.width,
                    height: descriptor.height,
                    container: descriptor.container,
                },
                output: VideoRunnerOutput {
                    path: output.private_output_path.clone(),
                    container: descriptor.container,
                },
                work: VideoWorkPaths {
                    root: published_work.clone(),
                    input_frames: published_work.join("input-frames"),
                    output_frames: published_work.join("output-frames"),
                },
                parameters: VideoInterpolateParameters {
                    source_rate,
                    target_rate,
                    frame_count: descriptor.frame_count,
                    chunk_frames,
                    scene_threshold_permille: pipeline.scene_threshold_permille,
                    model: RifeModel::RifeV46,
                    device,
                },
                mux_plan: mux_plan.clone(),
            };
            runner_request
                .validate()
                .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;

            let created_at_ms = now_ms();
            let job_spec = ProductJobSpec {
                schema_version: 4,
                job_id: job_id.clone(),
                kind: JobKind::VideoInterpolate,
                input_name: Some(input_name.clone()),
                output_path: Some(output.final_path.clone()),
                image_settings: None,
                video_settings: Some(settings),
                source_rate: Some(source_rate),
                target_rate: Some(target_rate),
                video_container: Some(descriptor.container),
                batch_id: None,
                batch_index: None,
                batch_total: None,
                selected_backend: None,
                selected_video_backend: Some(selected_backend),
                scenario: None,
                created_at_ms,
            };
            let plan = JobPlan {
                schema_version: 2,
                job_id: job_id.clone(),
                execution_backend: "process".into(),
                runner_id: "zoos-runner-rife".into(),
            };
            let summary = JobSummary {
                job_id: job_id.clone(),
                kind: JobKind::VideoInterpolate,
                input_name: Some(input_name),
                output_path: Some(output.final_path.clone()),
                image_settings: None,
                video_settings: Some(settings),
                source_rate: Some(source_rate),
                target_rate: Some(target_rate),
                video_container: Some(descriptor.container),
                batch_id: None,
                batch_index: None,
                batch_total: None,
                selected_backend: None,
                selected_video_backend: Some(selected_backend),
                scenario: None,
                status: JobStatus::Created,
                progress_percent: 0,
                stage: None,
                message: "Ready to interpolate video".into(),
                error: None,
                created_at_ms,
                updated_at_ms: created_at_ms,
            };
            let progress = JobProgress {
                schema_version: 1,
                summary: summary.clone(),
            };
            let manifest = JobManifest {
                schema_version: 3,
                job_id: job_id.clone(),
                runner_id: "zoos-runner-rife".into(),
                runner_version: env!("CARGO_PKG_VERSION").into(),
                result: None,
                exit_code: None,
                started_at_ms: None,
                finished_at_ms: None,
                actual_backend: None,
                actual_video_backend: Some(selected_backend),
                actual_device: None,
                runtime_sha256: None,
                model_param_sha256: None,
                model_bin_sha256: None,
                model_onnx_sha256: None,
                ffmpeg_sha256: Some(FFMPEG_SHA256.into()),
                ffprobe_sha256: Some(FFPROBE_SHA256.into()),
                rife_engine_sha256: Some(RIFE_ENGINE_SHA256.into()),
                rife_model_param_sha256: Some(RIFE_MODEL_PARAM_SHA256.into()),
                rife_model_bin_sha256: Some(RIFE_MODEL_BIN_SHA256.into()),
                fallback_reason: None,
                source_sha256: Some(output.input.sha256.clone()),
                intermediate_sha256: None,
                final_sha256: None,
                icc_preserved: None,
                exif_preserved: None,
                alpha_preserved: None,
                source_rate: Some(source_rate),
                target_rate: Some(target_rate),
                source_frames: Some(descriptor.frame_count),
                output_frames: None,
                scene_cut_count: None,
                chunk_count: None,
            };

            write_json_atomic(&job_dir.join(JOB_SPEC_FILE), &job_spec)?;
            write_json_atomic(&job_dir.join(PLAN_FILE), &plan)?;
            write_json_atomic(&job_dir.join(RUNNER_JOB_FILE), &runner_request)?;
            write_json_atomic(&job_dir.join(MEDIA_DESCRIPTOR_FILE), &descriptor)?;
            write_json_atomic(&job_dir.join(MUX_PLAN_FILE), &mux_plan)?;
            write_json_atomic(&job_dir.join(VIDEO_PIPELINE_FILE), &pipeline)?;
            write_json_atomic(&job_dir.join(PROGRESS_FILE), &progress)?;
            write_json_atomic(&job_dir.join(MANIFEST_FILE), &manifest)?;
            create_empty_file(&job_dir.join(LOGS_FILE))?;
            create_empty_file(&job_dir.join(PLAN_REVISIONS_FILE))?;
            sync_directory(&staging_work.join("input-frames"))?;
            sync_directory(&staging_work.join("output-frames"))?;
            sync_directory(&staging_work)?;
            sync_directory(&job_dir)?;
            fs::rename(&job_dir, &published_dir)?;
            sync_directory(&self.staging_dir())?;
            sync_directory(&self.root)?;
            Ok(summary)
        })();

        if create.is_err()
            && job_dir.exists()
            && let Err(error) = self.quarantine(&job_dir, "Goal 2 video job creation failed")
        {
            eprintln!("could not quarantine failed Goal 2 staging job: {error}");
        }
        create
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

    fn active_output_reservations(&self) -> Result<HashSet<PathBuf>, WorkspaceError> {
        Ok(self
            .list_jobs()?
            .into_iter()
            .filter(|job| !job.status.is_terminal())
            .filter_map(|job| job.output_path)
            .collect())
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

    pub(crate) fn finish_unstarted_manifest(
        &self,
        job_id: &str,
        result: &str,
    ) -> Result<(), WorkspaceError> {
        let path = self.job_dir(job_id)?.join(MANIFEST_FILE);
        let mut manifest: JobManifest = read_json(&path)?;
        manifest.result = Some(result.into());
        manifest.exit_code = None;
        manifest.started_at_ms = None;
        manifest.finished_at_ms = Some(now_ms());
        write_json_atomic(&path, &manifest)
    }

    fn record_pipeline_verification(
        &self,
        job_id: &str,
        verification: &ImagePipelineVerification,
    ) -> Result<(), WorkspaceError> {
        let path = self.job_dir(job_id)?.join(MANIFEST_FILE);
        let mut manifest: JobManifest = read_json(&path)?;
        manifest.actual_backend = Some(verification.actual_backend);
        manifest.model_param_sha256 = None;
        manifest.model_bin_sha256 = None;
        manifest.model_onnx_sha256 = None;
        let settings = self
            .load_summary(job_id)?
            .image_settings
            .ok_or(WorkspaceError::UnsafeRunnerRequest)?;
        match (verification.actual_backend, settings.preset) {
            (ImageBackend::VulkanGpu, ImagePreset::Photo) => {
                manifest.runtime_sha256 = Some(GPU_RUNTIME_SHA256.into());
                manifest.model_param_sha256 = Some(PHOTO_PARAM_SHA256.into());
                manifest.model_bin_sha256 = Some(PHOTO_BIN_SHA256.into());
            }
            (ImageBackend::VulkanGpu, ImagePreset::Anime) => {
                manifest.runtime_sha256 = Some(GPU_RUNTIME_SHA256.into());
                manifest.model_param_sha256 = Some(ANIME_PARAM_SHA256.into());
                manifest.model_bin_sha256 = Some(ANIME_BIN_SHA256.into());
            }
            (ImageBackend::OrtCpu, ImagePreset::Photo) => {
                manifest.runtime_sha256 = Some(ORT_RUNTIME_SHA256.into());
                manifest.model_onnx_sha256 = Some(PHOTO_ONNX_SHA256.into());
            }
            (ImageBackend::OrtCpu, ImagePreset::Anime) => {
                manifest.runtime_sha256 = Some(ORT_RUNTIME_SHA256.into());
                manifest.model_onnx_sha256 = Some(ANIME_ONNX_SHA256.into());
            }
            (ImageBackend::Auto, _) => return Err(WorkspaceError::InvalidSelectedBackend),
        }
        manifest.source_sha256 = Some(verification.source_sha256_after.clone());
        manifest.intermediate_sha256 = Some(verification.intermediate_sha256.clone());
        manifest.final_sha256 = Some(verification.output_sha256.clone());
        manifest.icc_preserved = Some(verification.icc_preserved);
        manifest.exif_preserved = Some(verification.exif_preserved);
        manifest.alpha_preserved = Some(verification.alpha_preserved);
        write_json_atomic(&path, &manifest)
    }

    fn record_video_verification(
        &self,
        job_id: &str,
        verification: &VideoPipelineVerification,
        evidence: &VideoRunnerEvidence,
    ) -> Result<(), WorkspaceError> {
        let path = self.job_dir(job_id)?.join(MANIFEST_FILE);
        let mut manifest: JobManifest = read_json(&path)?;
        manifest.actual_video_backend = Some(match evidence.selected_backend.as_str() {
            "vulkan" => VideoBackend::VulkanGpu,
            "ncnn_cpu" => VideoBackend::NcnnCpu,
            _ => return Err(WorkspaceError::UnsafeRunnerRequest),
        });
        manifest.actual_device = Some(evidence.selected_device.clone());
        manifest.ffmpeg_sha256 = Some(evidence.ffmpeg_sha256.clone());
        manifest.ffprobe_sha256 = Some(evidence.ffprobe_sha256.clone());
        manifest.rife_engine_sha256 = Some(evidence.rife_sha256.clone());
        manifest.rife_model_param_sha256 = evidence.model_sha256.get("flownet.param").cloned();
        manifest.rife_model_bin_sha256 = evidence.model_sha256.get("flownet.bin").cloned();
        manifest.source_sha256 = Some(verification.input_sha256_after.clone());
        manifest.intermediate_sha256 = Some(verification.output_sha256.clone());
        manifest.final_sha256 = Some(verification.output_sha256.clone());
        manifest.source_rate = Some(verification.source_rate);
        manifest.target_rate = Some(verification.target_rate);
        manifest.source_frames = Some(verification.source_frames);
        manifest.output_frames = Some(verification.output_frames);
        manifest.scene_cut_count = Some(verification.scene_cut_count);
        manifest.chunk_count = Some(verification.chunk_count);
        write_json_atomic(&path, &manifest)
    }

    pub(crate) fn record_runner_device(
        &self,
        job_id: &str,
        warning_code: &str,
        message: &str,
    ) -> Result<(), WorkspaceError> {
        let path = self.job_dir(job_id)?.join(MANIFEST_FILE);
        let mut manifest: JobManifest = read_json(&path)?;
        let valid = matches!(
            (manifest.runner_id.as_str(), warning_code),
            ("zoos-runner-realesrgan", "GPU_DEVICE")
                | ("zoos-runner-ort", "CPU_DEVICE")
                | ("zoos-runner-rife", "VIDEO_DEVICE")
        );
        if !valid {
            return Ok(());
        }
        let device = message
            .split_once(" | ")
            .map_or(message, |(device, _)| device)
            .trim()
            .chars()
            .take(256)
            .collect::<String>();
        if device.is_empty() {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        manifest.actual_device = Some(device);
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
                let pipeline: ImagePipelinePlan = read_json(&job_dir.join(IMAGE_PIPELINE_FILE))?;
                let upstream_partial = upstream_private_output_path(&pipeline.native_x4_png)
                    .ok_or(WorkspaceError::UnsafeRunnerRequest)?;
                let destination_partial_was_present =
                    match fs::symlink_metadata(&pipeline.destination_partial) {
                        Ok(_) => true,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                        Err(error) => return Err(error.into()),
                    };
                remove_if_exists(&request.output.path)?;
                remove_if_exists(&upstream_partial)?;
                remove_if_exists(&pipeline.destination_partial)?;
                remove_if_exists(&pipeline.inference_png)?;
                if let Some(alpha) = pipeline.alpha_png.as_ref() {
                    remove_if_exists(alpha)?;
                }
                if !destination_partial_was_present
                    && let Ok(verification) =
                        read_json::<ImagePipelineVerification>(&job_dir.join(VERIFICATION_FILE))
                    && verification.job_id == job_id
                    && verification.output_path == pipeline.destination_final
                    && crate::image_safety::sha256_file(&pipeline.destination_final)
                        .ok()
                        .as_deref()
                        == Some(verification.output_sha256.as_str())
                {
                    remove_if_exists(&pipeline.destination_final)?;
                }
                remove_if_exists(&job_dir.join(VERIFICATION_FILE))?;
            }
            StoredRunnerRequest::VideoInterpolate(_) => {
                let pipeline: VideoPipelinePlan = read_json(&job_dir.join(VIDEO_PIPELINE_FILE))?;
                cleanup_owned_video_output(&pipeline.output, &job_dir.join(VERIFICATION_FILE))?;
                cleanup_video_work_directory(&job_dir.join("work"))?;
            }
        }
        Ok(())
    }

    pub fn prepare_execution(&self, job_id: &str) -> Result<(), WorkspaceError> {
        let kind = self.load_summary(job_id)?.kind;
        match kind {
            JobKind::FakeValidation => Ok(()),
            JobKind::ImageUpscale => self.recheck_image_input(job_id),
            JobKind::VideoInterpolate => self.recheck_video_input(job_id),
        }
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
                let pipeline: ImagePipelinePlan =
                    read_json(&self.job_dir(job_id)?.join(IMAGE_PIPELINE_FILE))?;
                let source = checked_source_sha256(&pipeline.source_path)
                    .map_err(|_| ImageSafetyError::InputChanged)?;
                let inference = crate::image_safety::sha256_file(&request.input.path)
                    .map_err(|_| ImageSafetyError::InputChanged)?;
                if source == pipeline.source_sha256 && inference == request.input.sha256 {
                    Ok(())
                } else {
                    Err(ImageSafetyError::InputChanged.into())
                }
            }
            StoredRunnerRequest::Fake(_) => Ok(()),
            StoredRunnerRequest::VideoInterpolate(_) => self.recheck_video_input(job_id),
        }
    }

    fn recheck_video_input(&self, job_id: &str) -> Result<(), WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        let pipeline: VideoPipelinePlan = read_json(&job_dir.join(VIDEO_PIPELINE_FILE))?;
        recheck_video_input(&pipeline.output.input)?;
        Ok(())
    }

    pub(crate) fn video_output_to_probe(&self, job_id: &str) -> Result<PathBuf, WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        let pipeline: VideoPipelinePlan = read_json(&job_dir.join(VIDEO_PIPELINE_FILE))?;
        let stored = self.load_stored_job(job_id)?;
        let StoredRunnerRequest::VideoInterpolate(request) = stored.runner_request else {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        };
        if request.output.path != pipeline.output.private_output_path {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        require_regular_file(&request.output.path)?;
        Ok(request.output.path)
    }

    pub(crate) fn publish_verified_video_output(
        &self,
        job_id: &str,
        output_descriptor: &MediaDescriptor,
        expected_private_sha256: &str,
    ) -> Result<(), WorkspaceError> {
        let job_dir = self.job_dir(job_id)?;
        let pipeline: VideoPipelinePlan = read_json(&job_dir.join(VIDEO_PIPELINE_FILE))?;
        let stored = self.load_stored_job(job_id)?;
        let StoredRunnerRequest::VideoInterpolate(request) = stored.runner_request else {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        };
        if request.output.path != pipeline.output.private_output_path
            || output_descriptor.input_path != request.output.path
            || request.mux_plan != pipeline.mux_plan
        {
            return Err(WorkspaceError::UnsafeRunnerRequest);
        }
        verify_interpolated_output(&pipeline.descriptor, output_descriptor, &pipeline.mux_plan)?;
        let evidence: VideoRunnerEvidence =
            read_json(&job_dir.join("work").join(VIDEO_RUNNER_EVIDENCE_FILE))?;
        validate_video_runner_evidence(&pipeline, &evidence)?;
        let output_sha256 = stage_private_video_output(&pipeline.output)?;
        if output_sha256 != expected_private_sha256 {
            return Err(VideoSafetyError::InvalidOutput(
                "private output changed during verification",
            )
            .into());
        }
        let source_after = recheck_video_input(&pipeline.output.input)?;
        let audio_streams = u32::try_from(
            output_descriptor
                .streams
                .iter()
                .filter(|stream| stream.kind == zoos_runner_protocol::MuxStreamKind::Audio)
                .count(),
        )
        .map_err(|_| WorkspaceError::UnsafeRunnerRequest)?;
        let subtitle_streams = u32::try_from(
            output_descriptor
                .streams
                .iter()
                .filter(|stream| stream.kind == zoos_runner_protocol::MuxStreamKind::Subtitle)
                .count(),
        )
        .map_err(|_| WorkspaceError::UnsafeRunnerRequest)?;
        let chapter_count = u32::try_from(output_descriptor.chapters.len())
            .map_err(|_| WorkspaceError::UnsafeRunnerRequest)?;
        let verification = VideoPipelineVerification {
            schema_version: 1,
            job_id: job_id.into(),
            input_path: pipeline.output.input.path.clone(),
            input_sha256_before: pipeline.output.input.sha256.clone(),
            input_sha256_after: source_after,
            output_path: pipeline.output.final_path.clone(),
            output_sha256,
            container: output_descriptor.container,
            width: output_descriptor.width,
            height: output_descriptor.height,
            source_rate: pipeline.descriptor.frame_rate,
            target_rate: output_descriptor.frame_rate,
            source_frames: pipeline.descriptor.frame_count,
            output_frames: output_descriptor.frame_count,
            duration_ms: output_descriptor.duration_ms,
            audio_streams,
            subtitle_streams,
            chapter_count,
            scene_cut_count: evidence.scene_cut_count,
            chunk_count: evidence.chunk_count,
        };
        publish_staged_video_output(
            &pipeline.output,
            &job_dir.join(VERIFICATION_FILE),
            &verification,
        )?;
        self.record_video_verification(job_id, &verification, &evidence)?;
        remove_if_exists(&pipeline.output.private_output_path)?;
        Ok(())
    }

    pub fn publish_output(&self, job_id: &str) -> Result<(), WorkspaceError> {
        let kind = self.load_summary(job_id)?.kind;
        match kind {
            JobKind::FakeValidation => Ok(()),
            JobKind::ImageUpscale => self.publish_image_output(job_id).map(drop),
            JobKind::VideoInterpolate => Err(WorkspaceError::UnsupportedLifecycle(
                JobKind::VideoInterpolate,
            )),
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
            StoredRunnerRequest::ImageUpscaleV2(request) => {
                let pipeline: ImagePipelinePlan = read_json(&job_dir.join(IMAGE_PIPELINE_FILE))?;
                if request.output.path != pipeline.native_x4_png {
                    return Err(WorkspaceError::UnsafeRunnerRequest);
                }
                require_safe_destination_directory(
                    &pipeline.source_path,
                    &pipeline.destination_final,
                )?;
                require_regular_file(&pipeline.inference_png)?;
                require_regular_file(&pipeline.native_x4_png)?;
                if let Some(alpha) = pipeline.alpha_png.as_ref() {
                    require_regular_file(alpha)?;
                }
                if pipeline.destination_partial.exists() {
                    return Err(ImageSafetyError::OutputExists(
                        pipeline.destination_partial.clone(),
                    )
                    .into());
                }
                let intermediate_sha256 =
                    crate::image_safety::sha256_file(&pipeline.native_x4_png)?;
                let prepared = prepared_from_pipeline(&pipeline);
                let rendered = match render_pipeline_output(
                    &prepared,
                    &pipeline.native_x4_png,
                    &pipeline.destination_partial,
                    pipeline.scale,
                    pipeline_encoding(pipeline.output_format),
                    pipeline_metadata_policy(pipeline.metadata_policy),
                    ImagePipelineLimits::default(),
                ) {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        let _ = remove_if_exists(&pipeline.destination_partial);
                        return Err(error.into());
                    }
                };
                let source_after = checked_source_sha256(&pipeline.source_path)
                    .map_err(|_| ImageSafetyError::InputChanged)?;
                if source_after != pipeline.source_sha256 {
                    remove_if_exists(&pipeline.destination_partial)?;
                    return Err(ImageSafetyError::InputChanged.into());
                }
                let verification = pipeline_verification(
                    &pipeline,
                    &request,
                    &rendered,
                    source_after,
                    intermediate_sha256,
                )?;
                if let Err(error) =
                    write_json_atomic(&job_dir.join(VERIFICATION_FILE), &verification)
                {
                    let _ = remove_if_exists(&pipeline.destination_partial);
                    return Err(error);
                }
                if let Err(error) = crate::image_safety::no_replace_rename(
                    &pipeline.destination_partial,
                    &pipeline.destination_final,
                ) {
                    let _ = remove_if_exists(&pipeline.destination_partial);
                    let _ = remove_if_exists(&job_dir.join(VERIFICATION_FILE));
                    return Err(error.into());
                }
                if let Err(error) =
                    crate::image_safety::sync_parent_directory(&pipeline.destination_final)
                {
                    rollback_pipeline_final(&pipeline.destination_final, &rendered.sha256);
                    let _ = remove_if_exists(&job_dir.join(VERIFICATION_FILE));
                    return Err(error.into());
                }
                if let Err(error) = self.record_pipeline_verification(job_id, &verification) {
                    rollback_pipeline_final(&pipeline.destination_final, &rendered.sha256);
                    let _ = remove_if_exists(&job_dir.join(VERIFICATION_FILE));
                    return Err(error);
                }
                remove_if_exists(&pipeline.native_x4_png)?;
                remove_if_exists(&pipeline.inference_png)?;
                if let Some(alpha) = pipeline.alpha_png.as_ref() {
                    remove_if_exists(alpha)?;
                }
                Ok(None)
            }
            StoredRunnerRequest::Fake(_) => Ok(None),
            StoredRunnerRequest::VideoInterpolate(_) => Err(WorkspaceError::UnsupportedLifecycle(
                JobKind::VideoInterpolate,
            )),
        }
    }

    pub fn recover_interrupted(&self) -> Result<(), WorkspaceError> {
        for job in self.list_jobs()? {
            if job.status.is_active() && self.recover_completed_video_job(&job)? {
                continue;
            }
            if job.status == JobStatus::Created
                && ((job.kind == JobKind::ImageUpscale && job.selected_backend.is_some())
                    || (job.kind == JobKind::VideoInterpolate
                        && job.selected_video_backend.is_some()))
            {
                // Goal 1B picker-created jobs have no manual "start later" UI. If the app ended
                // between creation and start, keeping normalized pixels and filename reservations
                // would strand an invisible queue forever. Legacy Goal 1A jobs have no persisted
                // selected backend and remain startable for compatibility.
                self.cleanup_unverified_output(&job.job_id)?;
                self.finish_unstarted_manifest(&job.job_id, "cancelled_after_restart")?;
                self.update_summary(&job.job_id, |summary| {
                    summary.status = JobStatus::Cancelled;
                    summary.stage = None;
                    summary.message = "Cancelled before execution after app restart".into();
                    summary.error = None;
                })?;
            } else if job.status.is_active() {
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

    fn recover_completed_video_job(&self, job: &JobSummary) -> Result<bool, WorkspaceError> {
        if job.kind != JobKind::VideoInterpolate {
            return Ok(false);
        }
        let job_dir = self.job_dir(&job.job_id)?;
        let manifest: JobManifest = read_json(&job_dir.join(MANIFEST_FILE))?;
        if manifest.result.as_deref() != Some("completed")
            || manifest.exit_code != Some(0)
            || manifest.started_at_ms.is_none()
            || manifest.finished_at_ms.is_none()
        {
            return Ok(false);
        }
        let pipeline: VideoPipelinePlan = read_json(&job_dir.join(VIDEO_PIPELINE_FILE))?;
        let verification_path = job_dir.join(VERIFICATION_FILE);
        if require_regular_file(&verification_path).is_err() {
            return Ok(false);
        }
        let verification: VideoPipelineVerification = match read_json(&verification_path) {
            Ok(verification) => verification,
            Err(_) => return Ok(false),
        };
        let manifest_matches = manifest.actual_video_backend == Some(pipeline.selected_backend)
            && manifest
                .actual_device
                .as_deref()
                .is_some_and(|device| !device.is_empty())
            && manifest.ffmpeg_sha256.as_deref() == Some(FFMPEG_SHA256)
            && manifest.ffprobe_sha256.as_deref() == Some(FFPROBE_SHA256)
            && manifest.rife_engine_sha256.as_deref() == Some(RIFE_ENGINE_SHA256)
            && manifest.rife_model_param_sha256.as_deref() == Some(RIFE_MODEL_PARAM_SHA256)
            && manifest.rife_model_bin_sha256.as_deref() == Some(RIFE_MODEL_BIN_SHA256)
            && manifest.source_sha256.as_deref() == Some(verification.input_sha256_after.as_str())
            && manifest.intermediate_sha256.as_deref() == Some(verification.output_sha256.as_str())
            && manifest.final_sha256.as_deref() == Some(verification.output_sha256.as_str())
            && manifest.source_rate == Some(verification.source_rate)
            && manifest.target_rate == Some(verification.target_rate)
            && manifest.source_frames == Some(verification.source_frames)
            && manifest.output_frames == Some(verification.output_frames)
            && manifest.scene_cut_count == Some(verification.scene_cut_count)
            && manifest.chunk_count == Some(verification.chunk_count);
        if !manifest_matches
            || validate_published_video_output(&pipeline.output, &verification).is_err()
        {
            return Ok(false);
        }
        self.update_summary(&job.job_id, |summary| {
            summary.status = JobStatus::Completed;
            summary.progress_percent = 100;
            summary.stage = None;
            summary.message = "Video interpolation completed successfully".into();
            summary.error = None;
        })?;
        Ok(true)
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
        if spec.input_name != progress.summary.input_name
            || spec.output_path != progress.summary.output_path
            || spec.image_settings != progress.summary.image_settings
            || spec.video_settings != progress.summary.video_settings
            || spec.source_rate != progress.summary.source_rate
            || spec.target_rate != progress.summary.target_rate
            || spec.video_container != progress.summary.video_container
            || spec.batch_id != progress.summary.batch_id
            || spec.batch_index != progress.summary.batch_index
            || spec.batch_total != progress.summary.batch_total
            || spec.selected_backend != progress.summary.selected_backend
            || spec.selected_video_backend != progress.summary.selected_video_backend
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
                    && is_safe_image_request_v2(job_dir, &progress.summary, &request)
                    && (matches!(
                        (progress.summary.selected_backend, plan.runner_id.as_str()),
                        (Some(ImageBackend::VulkanGpu), "zoos-runner-realesrgan")
                            | (Some(ImageBackend::OrtCpu), "zoos-runner-ort")
                    ) || (progress.summary.selected_backend.is_none()
                        && !job_dir.join(IMAGE_PIPELINE_FILE).exists())) => {}
            StoredRunnerRequest::VideoInterpolate(request)
                if request.job_id == job_id
                    && progress.summary.output_path.as_ref() != Some(&request.output.path)
                    && plan.runner_id == "zoos-runner-rife"
                    && is_safe_video_request(job_dir, &progress.summary, &request) => {}
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
                            || !is_safe_image_request_v2(job_dir, summary, &request)
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
            JobKind::VideoInterpolate => {
                let request: VideoInterpolateJobRequest =
                    read_json(&job_dir.join(RUNNER_JOB_FILE))?;
                request
                    .validate()
                    .map_err(|error| WorkspaceError::InvalidRunnerContract(error.to_string()))?;
                if request.job_id != summary.job_id
                    || summary.video_settings.is_none()
                    || !is_safe_video_request(job_dir, summary, &request)
                {
                    return Err(WorkspaceError::UnsafeRunnerRequest);
                }
                Ok(StoredRunnerRequest::VideoInterpolate(request))
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

fn validate_pipeline_dimensions(
    width: u32,
    height: u32,
    requested_scale: u8,
) -> Result<(), WorkspaceError> {
    if !matches!(requested_scale, 2 | 4) {
        return Err(ImageSafetyError::UnsupportedScale(requested_scale).into());
    }
    for scale in [requested_scale, 4] {
        let output_width = u64::from(width) * u64::from(scale);
        let output_height = u64::from(height) * u64::from(scale);
        if output_width > 32_000
            || output_height > 32_000
            || output_width.saturating_mul(output_height) > 100_000_000
        {
            return Err(ImageSafetyError::OutputTooLarge {
                width: u32::try_from(output_width).unwrap_or(u32::MAX),
                height: u32::try_from(output_height).unwrap_or(u32::MAX),
            }
            .into());
        }
    }
    Ok(())
}

fn plan_pipeline_destination(
    input: &Path,
    scale: u8,
    format: ProductOutputFormat,
    job_id: &str,
    width: u32,
    height: u32,
    reserved_outputs: &HashSet<PathBuf>,
) -> Result<(PathBuf, PathBuf), WorkspaceError> {
    let parent = input.parent().ok_or(ImageSafetyError::InvalidInputPath)?;
    let destination_dir = parent.join("Upscaled");
    fs::create_dir_all(&destination_dir)?;
    require_safe_destination_directory(input, &destination_dir.join("placeholder"))?;
    let native_pixels = u64::from(width)
        .saturating_mul(4)
        .saturating_mul(u64::from(height).saturating_mul(4));
    let final_pixels = u64::from(width)
        .saturating_mul(u64::from(scale))
        .saturating_mul(u64::from(height).saturating_mul(u64::from(scale)));
    let required = (1024 * 1024 * 1024_u64).max(
        native_pixels
            .saturating_mul(3)
            .saturating_mul(2)
            .saturating_add(final_pixels.saturating_mul(4).saturating_mul(2)),
    );
    let available = fs4::available_space(&destination_dir)?;
    if available < required {
        return Err(ImageSafetyError::InsufficientDisk {
            required,
            available,
        }
        .into());
    }
    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .ok_or(ImageSafetyError::InvalidInputPath)?;
    let extension = output_extension(format);
    let base = format!("{stem}_upscaled_{scale}x");
    let mut final_path = None;
    for suffix in 1..=999 {
        let name = if suffix == 1 {
            format!("{base}.{extension}")
        } else {
            format!("{base}_{suffix}.{extension}")
        };
        let candidate = destination_dir.join(name);
        if !candidate.exists() && !reserved_outputs.contains(&candidate) {
            final_path = Some(candidate);
            break;
        }
    }
    let final_path = final_path.ok_or(ImageSafetyError::NoOutputNameAvailable)?;
    let final_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ImageSafetyError::InvalidInputPath)?;
    let partial = destination_dir.join(format!(".{final_name}.zoos-{job_id}.partial.{extension}"));
    if partial.exists() {
        return Err(ImageSafetyError::OutputExists(partial).into());
    }
    Ok((final_path, partial))
}

fn ensure_pipeline_workspace_space(
    work: &Path,
    width: u32,
    height: u32,
) -> Result<(), WorkspaceError> {
    let native_raw = u64::from(width)
        .saturating_mul(4)
        .saturating_mul(u64::from(height).saturating_mul(4))
        .saturating_mul(3);
    let required = (1024 * 1024 * 1024_u64).max(native_raw.saturating_mul(2));
    let available = fs4::available_space(work)?;
    if available < required {
        return Err(ImageSafetyError::InsufficientDisk {
            required,
            available,
        }
        .into());
    }
    Ok(())
}

fn require_safe_destination_directory(
    source: &Path,
    destination: &Path,
) -> Result<(), WorkspaceError> {
    let expected = source
        .parent()
        .ok_or(ImageSafetyError::InvalidInputPath)?
        .join("Upscaled");
    if destination.parent() != Some(expected.as_path()) {
        return Err(WorkspaceError::UnsafeRunnerRequest);
    }
    let metadata = fs::symlink_metadata(&expected)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::UnsafeRunnerRequest);
    }
    Ok(())
}

fn output_extension(format: ProductOutputFormat) -> &'static str {
    match format {
        ProductOutputFormat::Png => "png",
        ProductOutputFormat::Jpeg => "jpg",
        ProductOutputFormat::Webp => "webp",
    }
}

fn checked_source_sha256(path: &Path) -> Result<String, WorkspaceError> {
    if !path.is_absolute() {
        return Err(ImageSafetyError::InvalidInputPath.into());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ImageSafetyError::InvalidInputPath.into());
    }
    if metadata.len() > 512 * 1024 * 1024 {
        return Err(Goal1bImageError::InputTooLarge.into());
    }
    Ok(crate::image_safety::sha256_file(path)?)
}

fn pipeline_encoding(format: ProductOutputFormat) -> OutputEncoding {
    match format {
        ProductOutputFormat::Png => OutputEncoding::Png,
        ProductOutputFormat::Jpeg => OutputEncoding::Jpeg,
        ProductOutputFormat::Webp => OutputEncoding::Webp,
    }
}

fn pipeline_metadata_policy(policy: MetadataPolicy) -> PipelineMetadataPolicy {
    match policy {
        MetadataPolicy::Preserve => PipelineMetadataPolicy::Preserve,
        MetadataPolicy::Strip => PipelineMetadataPolicy::Strip,
    }
}

fn prepared_from_pipeline(plan: &ImagePipelinePlan) -> PreparedImage {
    PreparedImage {
        input_path: plan.source_path.clone(),
        inference_png: plan.inference_png.clone(),
        alpha_png: plan.alpha_png.clone(),
        width: plan.width,
        height: plan.height,
        had_alpha: plan.had_alpha,
        orientation: plan.orientation,
        metadata: plan.metadata.clone(),
    }
}

fn pipeline_verification(
    plan: &ImagePipelinePlan,
    request: &ImageUpscaleJobRequestV2,
    rendered: &VerifiedPipelineOutput,
    source_after: String,
    intermediate_sha256: String,
) -> Result<ImagePipelineVerification, WorkspaceError> {
    Ok(ImagePipelineVerification {
        schema_version: 2,
        job_id: plan.job_id.clone(),
        actual_backend: plan.selected_backend,
        source_path: plan.source_path.clone(),
        source_sha256_before: plan.source_sha256.clone(),
        source_sha256_after: source_after,
        inference_sha256: request.input.sha256.clone(),
        intermediate_sha256,
        output_path: plan.destination_final.clone(),
        output_sha256: rendered.sha256.clone(),
        output_format: plan.output_format,
        output_width: rendered.width,
        output_height: rendered.height,
        alpha_preserved: rendered.has_alpha == plan.had_alpha,
        icc_preserved: rendered.icc_preserved,
        exif_preserved: rendered.exif_preserved,
    })
}

fn rollback_pipeline_final(path: &Path, expected_sha256: &str) {
    if crate::image_safety::sha256_file(path).ok().as_deref() == Some(expected_sha256) {
        let _ = fs::remove_file(path);
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
        JobKind::VideoInterpolate => "zoos-runner-rife",
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

fn is_safe_image_request_v2(
    job_dir: &Path,
    summary: &JobSummary,
    request: &ImageUpscaleJobRequestV2,
) -> bool {
    let Some(final_path) = summary.output_path.as_deref() else {
        return false;
    };
    let Some(final_name) = final_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(settings) = summary.image_settings else {
        return false;
    };
    let Ok(pipeline) = read_json::<ImagePipelinePlan>(&job_dir.join(IMAGE_PIPELINE_FILE)) else {
        return request.input.path != request.output.path
            && request.output.path
                == final_path.with_file_name(format!(
                    ".{final_name}.zoos-{}.native-x4.partial.png",
                    summary.job_id
                ));
    };
    let work = job_dir.join("work");
    let extension = output_extension(settings.output_format);
    let expected_partial = final_path.with_file_name(format!(
        ".{final_name}.zoos-{}.partial.{extension}",
        summary.job_id
    ));
    let work_is_safe = fs::symlink_metadata(&work)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
    let upstream_private_output = upstream_private_output_path(&pipeline.native_x4_png);
    let managed_files_safe = [
        Some(request.input.path.as_path()),
        pipeline.alpha_png.as_deref(),
        Some(request.output.path.as_path()),
        upstream_private_output.as_deref(),
    ]
    .into_iter()
    .flatten()
    .all(|path| match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file() && !metadata.file_type().is_symlink(),
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    });
    let final_name_is_safe = pipeline
        .source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            let base = format!("{stem}_upscaled_{}x", settings.scale);
            final_name == format!("{base}.{extension}")
                || (2..=999).any(|suffix| final_name == format!("{base}_{suffix}.{extension}"))
        });
    let model_matches = matches!(
        (settings.preset, request.parameters.semantic_model),
        (ImagePreset::Photo, ImageSemanticModelV2::Photo)
            | (ImagePreset::Anime, ImageSemanticModelV2::Anime)
    );
    let backend_matches = match (
        pipeline.selected_backend,
        &request.parameters.device,
        &request.parameters.backend_settings,
    ) {
        (
            ImageBackend::VulkanGpu,
            ImageDeviceV2::Vulkan { index: 0 },
            ImageBackendSettingsV2::Vulkan { tile_size, threads },
        ) => *tile_size == 256 && threads == "1:2:2",
        (
            ImageBackend::OrtCpu,
            ImageDeviceV2::Cpu,
            ImageBackendSettingsV2::OrtCpu {
                tile_size,
                intra_threads,
                inter_threads,
            },
        ) => *tile_size == 128 && *intra_threads == 4 && *inter_threads == 1,
        _ => false,
    };
    pipeline.schema_version == 1
        && pipeline.job_id == summary.job_id
        && pipeline.destination_final == final_path
        && pipeline.destination_partial == expected_partial
        && pipeline.inference_png == work.join("inference-rgb.png")
        && pipeline
            .alpha_png
            .as_ref()
            .is_none_or(|path| path == &work.join("alpha.png"))
        && pipeline.native_x4_png == work.join("sr-native-x4.png")
        && upstream_private_output
            .is_some_and(|path| path == work.join(".sr-native-x4.png.zoos-upstream.partial.png"))
        && pipeline.source_path != final_path
        && final_path.parent().is_some_and(|final_parent| {
            pipeline
                .source_path
                .parent()
                .is_some_and(|source_parent| final_parent == source_parent.join("Upscaled"))
        })
        && pipeline.scale == settings.scale
        && request.parameters.requested_scale == settings.scale
        && model_matches
        && backend_matches
        && (matches!(settings.backend, ImageBackend::Auto)
            || settings.backend == pipeline.selected_backend)
        && pipeline.output_format == settings.output_format
        && pipeline.metadata_policy == settings.metadata
        && Some(pipeline.selected_backend) == summary.selected_backend
        && request.input.path == pipeline.inference_png
        && request.input.width == pipeline.width
        && request.input.height == pipeline.height
        && request.output.path == pipeline.native_x4_png
        && request.input.path != request.output.path
        && work_is_safe
        && managed_files_safe
        && final_name_is_safe
}

fn is_safe_video_request(
    job_dir: &Path,
    summary: &JobSummary,
    request: &VideoInterpolateJobRequest,
) -> bool {
    let Ok(pipeline) = read_json::<VideoPipelinePlan>(&job_dir.join(VIDEO_PIPELINE_FILE)) else {
        return false;
    };
    let Ok(descriptor) = read_json::<MediaDescriptor>(&job_dir.join(MEDIA_DESCRIPTOR_FILE)) else {
        return false;
    };
    let Ok(mux_plan) = read_json::<MuxPlan>(&job_dir.join(MUX_PLAN_FILE)) else {
        return false;
    };
    let Some(final_path) = summary.output_path.as_ref() else {
        return false;
    };
    let Some(settings) = summary.video_settings else {
        return false;
    };
    let work = job_dir.join("work");
    let input_frames = work.join("input-frames");
    let output_frames = work.join("output-frames");
    let backend_matches = matches!(
        (pipeline.selected_backend, request.parameters.device),
        (VideoBackend::VulkanGpu, VideoDevice::Vulkan { index: 0 })
            | (VideoBackend::NcnnCpu, VideoDevice::NcnnCpu)
    );
    let final_parent_matches =
        pipeline.output.input.path.parent().is_some_and(|parent| {
            final_path.parent() == Some(parent.join("Interpolated").as_path())
        });
    let name_matches = pipeline
        .output
        .input
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .zip(final_path.file_name().and_then(|name| name.to_str()))
        .is_some_and(|(stem, name)| {
            let extension = crate::video_safety::container_extension(descriptor.container);
            let base = format!("{stem}_interpolated_2x");
            name == format!("{base}.{extension}")
                || (2..=999).any(|suffix| name == format!("{base}_{suffix}.{extension}"))
        });
    let evidence_path = work.join(VIDEO_RUNNER_EVIDENCE_FILE);
    let work_safe = fs::symlink_metadata(&work)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
    let optional_directories_safe = [input_frames.as_path(), output_frames.as_path()]
        .iter()
        .all(|path| match fs::symlink_metadata(path) {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        });
    let optional_files_safe = [request.output.path.as_path(), evidence_path.as_path()]
        .iter()
        .all(|path| match fs::symlink_metadata(path) {
            Ok(metadata) => metadata.is_file() && !metadata.file_type().is_symlink(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        });
    let managed_paths_safe = work_safe && optional_directories_safe && optional_files_safe;

    pipeline.schema_version == 1
        && pipeline.job_id == summary.job_id
        && pipeline.descriptor == descriptor
        && pipeline.mux_plan == mux_plan
        && pipeline.mux_plan == request.mux_plan
        && pipeline.output.final_path == *final_path
        && pipeline.output.private_output_path == request.output.path
        && pipeline.output.input.path == request.input.path
        && pipeline.output.input.sha256 == request.input.sha256
        && pipeline.output.input.container == request.input.container
        && request.work.root == work
        && request.work.input_frames == input_frames
        && request.work.output_frames == output_frames
        && request.input.width == descriptor.width
        && request.input.height == descriptor.height
        && request.parameters.source_rate == descriptor.frame_rate
        && request.parameters.frame_count == descriptor.frame_count
        && request.parameters.chunk_frames == pipeline.chunk_frames
        && request.parameters.scene_threshold_permille == pipeline.scene_threshold_permille
        && request.parameters.model == RifeModel::RifeV46
        && summary.selected_video_backend == Some(pipeline.selected_backend)
        && (settings.backend == VideoBackend::Auto || settings.backend == pipeline.selected_backend)
        && summary.source_rate == Some(request.parameters.source_rate)
        && summary.target_rate == Some(request.parameters.target_rate)
        && summary.video_container == Some(descriptor.container)
        && backend_matches
        && final_parent_matches
        && name_matches
        && managed_paths_safe
}

fn upstream_private_output_path(destination: &Path) -> Option<PathBuf> {
    let parent = destination.parent()?;
    let name = destination.file_name()?.to_str()?;
    Some(parent.join(format!(".{name}.zoos-upstream.partial.png")))
}

fn validate_video_runner_evidence(
    pipeline: &VideoPipelinePlan,
    evidence: &VideoRunnerEvidence,
) -> Result<(), WorkspaceError> {
    let (backend, device) = match pipeline.selected_backend {
        VideoBackend::VulkanGpu => ("vulkan", "gpu:0"),
        VideoBackend::NcnnCpu => ("ncnn_cpu", "cpu"),
        VideoBackend::Auto => return Err(WorkspaceError::InvalidSelectedVideoBackend),
    };
    let expected_chunks = u32::try_from(
        pipeline
            .descriptor
            .frame_count
            .saturating_sub(1)
            .div_ceil(u64::from(pipeline.chunk_frames)),
    )
    .map_err(|_| WorkspaceError::UnsafeRunnerRequest)?;
    let model_keys = evidence
        .model_sha256
        .keys()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let valid = evidence.schema_version == 1
        && evidence.job_id == pipeline.job_id
        && evidence.selected_backend == backend
        && evidence.selected_device == device
        && evidence.source_frames == pipeline.descriptor.frame_count
        && evidence.output_frames == pipeline.descriptor.frame_count.saturating_mul(2)
        && evidence.chunk_count == expected_chunks
        && evidence.scene_cut_count <= pipeline.descriptor.frame_count.saturating_sub(1)
        && evidence.ffmpeg_sha256 == FFMPEG_SHA256
        && evidence.ffprobe_sha256 == FFPROBE_SHA256
        && evidence.rife_sha256 == RIFE_ENGINE_SHA256
        && model_keys == HashSet::from(["flownet.param", "flownet.bin"])
        && evidence
            .model_sha256
            .get("flownet.param")
            .map(String::as_str)
            == Some(RIFE_MODEL_PARAM_SHA256)
        && evidence.model_sha256.get("flownet.bin").map(String::as_str)
            == Some(RIFE_MODEL_BIN_SHA256);
    if valid {
        Ok(())
    } else {
        Err(WorkspaceError::UnsafeRunnerRequest)
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
    #[error("selected image backend must be resolved before job creation")]
    InvalidSelectedBackend,
    #[error("selected video backend must be resolved before job creation")]
    InvalidSelectedVideoBackend,
    #[error("batch metadata is invalid")]
    InvalidBatchMetadata,
    #[error("job lifecycle is not implemented for {0:?}")]
    UnsupportedLifecycle(JobKind),
    #[error("verified ffprobe is not configured for video output verification")]
    MediaVerifierUnavailable,
    #[error(transparent)]
    Image(#[from] ImageSafetyError),
    #[error(transparent)]
    Pipeline(#[from] Goal1bImageError),
    #[error(transparent)]
    Video(#[from] VideoSafetyError),
    #[error(transparent)]
    Media(#[from] zoos_media::MediaError),
    #[error("workspace I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("workspace JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

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

    fn goal1b_settings(format: ProductOutputFormat) -> ImageSettings {
        ImageSettings {
            preset: ImagePreset::Photo,
            scale: 2,
            backend: ImageBackend::Auto,
            output_format: format,
            metadata: MetadataPolicy::Preserve,
        }
    }

    fn video_descriptor(path: &Path) -> MediaDescriptor {
        MediaDescriptor {
            input_path: path.to_owned(),
            container: zoos_runner_protocol::VideoContainer::Mkv,
            format_name: "matroska,webm".into(),
            duration_ms: 1_000,
            width: 64,
            height: 64,
            frame_count: 25,
            frame_rate: RationalRate {
                numerator: 25,
                denominator: 1,
            },
            video_time_base: Some(RationalRate {
                numerator: 1,
                denominator: 1_000,
            }),
            video_stream_index: 0,
            streams: vec![zoos_media::MediaStreamDescriptor {
                input_index: 0,
                kind: zoos_runner_protocol::MuxStreamKind::Video,
                codec_name: "h264".into(),
                start_time_ms: Some(0),
                duration_ms: 1_000,
                duration_from_format: false,
                tags: std::collections::BTreeMap::new(),
                disposition: std::collections::BTreeMap::new(),
            }],
            chapters: Vec::new(),
            format_tags: std::collections::BTreeMap::new(),
        }
    }

    fn video_settings() -> VideoSettings {
        VideoSettings {
            backend: VideoBackend::Auto,
        }
    }

    #[test]
    fn video_jobs_reserve_names_and_persist_private_runner_contract() {
        let root = tempfile::tempdir().expect("workspace root");
        let inputs = tempfile::tempdir().expect("input root");
        let input = inputs.path().join("clip.mkv");
        fs::write(&input, b"source-video").expect("video fixture");
        let store = WorkspaceStore::new(root.path()).expect("workspace");
        let first = store
            .create_video_job(
                video_descriptor(&input),
                video_settings(),
                VideoBackend::VulkanGpu,
            )
            .expect("first video job");
        let second = store
            .create_video_job(
                video_descriptor(&input),
                video_settings(),
                VideoBackend::NcnnCpu,
            )
            .expect("second video job");
        assert_eq!(first.kind, JobKind::VideoInterpolate);
        assert_eq!(first.source_rate.unwrap().numerator, 25);
        assert_eq!(first.target_rate.unwrap().numerator, 50);
        assert_eq!(first.selected_video_backend, Some(VideoBackend::VulkanGpu));
        assert_eq!(
            first
                .output_path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some("clip_interpolated_2x.mkv")
        );
        assert_eq!(
            second
                .output_path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some("clip_interpolated_2x_2.mkv")
        );
        let stored = store
            .load_stored_job(&first.job_id)
            .expect("video job must load");
        let StoredRunnerRequest::VideoInterpolate(request) = stored.runner_request else {
            panic!("video request expected")
        };
        assert!(
            request
                .output
                .path
                .starts_with(store.root.join(&first.job_id).join("work"))
        );
        assert_ne!(first.output_path.as_ref(), Some(&request.output.path));
        assert_eq!(request.parameters.chunk_frames, 120);
        assert_eq!(request.parameters.scene_threshold_permille, 350);
    }

    #[test]
    fn verified_video_publish_records_evidence_and_cleans_private_work() {
        let root = tempfile::tempdir().expect("workspace root");
        let inputs = tempfile::tempdir().expect("input root");
        let input = inputs.path().join("clip.mkv");
        fs::write(&input, b"source-video").expect("video fixture");
        let store = WorkspaceStore::new(root.path()).expect("workspace");
        let job = store
            .create_video_job(
                video_descriptor(&input),
                video_settings(),
                VideoBackend::VulkanGpu,
            )
            .expect("video job");
        let StoredRunnerRequest::VideoInterpolate(request) = store
            .load_stored_job(&job.job_id)
            .expect("stored job")
            .runner_request
        else {
            panic!("video request expected")
        };
        fs::write(&request.output.path, b"verified-video-output").expect("private video output");
        let evidence = serde_json::json!({
            "schema_version": 1,
            "job_id": job.job_id,
            "selected_backend": "vulkan",
            "selected_device": "gpu:0",
            "source_frames": 25,
            "output_frames": 50,
            "chunk_count": 1,
            "scene_cut_count": 0,
            "ffmpeg_sha256": FFMPEG_SHA256,
            "ffprobe_sha256": FFPROBE_SHA256,
            "rife_sha256": RIFE_ENGINE_SHA256,
            "model_sha256": {
                "flownet.param": RIFE_MODEL_PARAM_SHA256,
                "flownet.bin": RIFE_MODEL_BIN_SHA256
            }
        });
        write_json_atomic(
            &root
                .path()
                .join(&job.job_id)
                .join("work")
                .join(VIDEO_RUNNER_EVIDENCE_FILE),
            &evidence,
        )
        .expect("runner evidence");
        let output_descriptor = MediaDescriptor {
            input_path: request.output.path.clone(),
            container: zoos_runner_protocol::VideoContainer::Mkv,
            format_name: "matroska,webm".into(),
            duration_ms: 1_000,
            width: 64,
            height: 64,
            frame_count: 50,
            frame_rate: RationalRate {
                numerator: 50,
                denominator: 1,
            },
            video_time_base: Some(RationalRate {
                numerator: 1,
                denominator: 1_000,
            }),
            video_stream_index: 0,
            streams: vec![zoos_media::MediaStreamDescriptor {
                input_index: 0,
                kind: zoos_runner_protocol::MuxStreamKind::Video,
                codec_name: "h264".into(),
                start_time_ms: Some(0),
                duration_ms: 1_000,
                duration_from_format: false,
                tags: std::collections::BTreeMap::new(),
                disposition: std::collections::BTreeMap::new(),
            }],
            chapters: Vec::new(),
            format_tags: std::collections::BTreeMap::new(),
        };
        let private_sha256 =
            crate::image_safety::sha256_file(&request.output.path).expect("private output hash");
        store
            .publish_verified_video_output(&job.job_id, &output_descriptor, &private_sha256)
            .expect("verified publish");
        let final_path = job.output_path.expect("final path");
        assert_eq!(
            fs::read(&final_path).expect("final video"),
            b"verified-video-output"
        );
        let work = store.root.join(&job.job_id).join("work");
        assert!(work.is_dir());
        assert!(!request.output.path.exists());
        assert!(work.join(VIDEO_RUNNER_EVIDENCE_FILE).is_file());
        assert!(store.list_jobs().is_ok());
        let manifest: JobManifest =
            read_json(&root.path().join(&job.job_id).join(MANIFEST_FILE)).expect("manifest");
        assert_eq!(manifest.actual_video_backend, Some(VideoBackend::VulkanGpu));
        assert_eq!(manifest.final_sha256.as_deref().map(str::len), Some(64));
        assert_eq!(manifest.output_frames, Some(50));
        assert_eq!(manifest.chunk_count, Some(1));
    }

    #[test]
    fn recovery_promotes_manifest_completed_video_without_removing_final() {
        let root = tempfile::tempdir().expect("workspace root");
        let inputs = tempfile::tempdir().expect("input root");
        let input = inputs.path().join("clip.mkv");
        fs::write(&input, b"source-video").expect("video fixture");
        let store = WorkspaceStore::new(root.path()).expect("workspace");
        let job = store
            .create_video_job(
                video_descriptor(&input),
                video_settings(),
                VideoBackend::VulkanGpu,
            )
            .expect("video job");
        let StoredRunnerRequest::VideoInterpolate(request) = store
            .load_stored_job(&job.job_id)
            .expect("stored job")
            .runner_request
        else {
            panic!("video request expected")
        };
        fs::write(&request.output.path, b"verified-video-output").expect("private output");
        write_json_atomic(
            &root
                .path()
                .join(&job.job_id)
                .join("work")
                .join(VIDEO_RUNNER_EVIDENCE_FILE),
            &serde_json::json!({
                "schema_version": 1,
                "job_id": job.job_id,
                "selected_backend": "vulkan",
                "selected_device": "gpu:0",
                "source_frames": 25,
                "output_frames": 50,
                "chunk_count": 1,
                "scene_cut_count": 0,
                "ffmpeg_sha256": FFMPEG_SHA256,
                "ffprobe_sha256": FFPROBE_SHA256,
                "rife_sha256": RIFE_ENGINE_SHA256,
                "model_sha256": {
                    "flownet.param": RIFE_MODEL_PARAM_SHA256,
                    "flownet.bin": RIFE_MODEL_BIN_SHA256
                }
            }),
        )
        .expect("runner evidence");
        let output_descriptor = MediaDescriptor {
            input_path: request.output.path.clone(),
            container: zoos_runner_protocol::VideoContainer::Mkv,
            format_name: "matroska,webm".into(),
            duration_ms: 1_000,
            width: 64,
            height: 64,
            frame_count: 50,
            frame_rate: RationalRate {
                numerator: 50,
                denominator: 1,
            },
            video_time_base: Some(RationalRate {
                numerator: 1,
                denominator: 1_000,
            }),
            video_stream_index: 0,
            streams: vec![zoos_media::MediaStreamDescriptor {
                input_index: 0,
                kind: zoos_runner_protocol::MuxStreamKind::Video,
                codec_name: "h264".into(),
                start_time_ms: Some(0),
                duration_ms: 1_000,
                duration_from_format: false,
                tags: std::collections::BTreeMap::new(),
                disposition: std::collections::BTreeMap::new(),
            }],
            chapters: Vec::new(),
            format_tags: std::collections::BTreeMap::new(),
        };
        let private_sha256 =
            crate::image_safety::sha256_file(&request.output.path).expect("private output hash");
        store
            .publish_verified_video_output(&job.job_id, &output_descriptor, &private_sha256)
            .expect("verified publish");
        store
            .finish_manifest(&job.job_id, "completed", Some(0), now_ms())
            .expect("completed manifest");
        store
            .update_summary(&job.job_id, |summary| {
                summary.status = JobStatus::Verifying;
                summary.progress_percent = 99;
            })
            .expect("crash-window summary");
        let final_path = job.output_path.expect("final output");
        assert!(final_path.exists());
        drop(store);

        let reopened = WorkspaceStore::new(root.path()).expect("reopen workspace");
        reopened
            .recover_interrupted()
            .expect("recover completed publish");
        let recovered = reopened
            .load_summary(&job.job_id)
            .expect("recovered summary");
        assert_eq!(recovered.status, JobStatus::Completed);
        assert_eq!(recovered.progress_percent, 100);
        assert!(final_path.exists());
        assert!(
            root.path()
                .join(&job.job_id)
                .join(VERIFICATION_FILE)
                .is_file()
        );
    }

    #[test]
    fn changed_video_input_is_rejected_and_cleanup_preserves_source() {
        let root = tempfile::tempdir().expect("workspace root");
        let inputs = tempfile::tempdir().expect("input root");
        let input = inputs.path().join("clip.mkv");
        fs::write(&input, b"source-video").expect("video fixture");
        let store = WorkspaceStore::new(root.path()).expect("workspace");
        let job = store
            .create_video_job(
                video_descriptor(&input),
                video_settings(),
                VideoBackend::VulkanGpu,
            )
            .expect("video job");
        fs::write(&input, b"changed-video").expect("changed input");
        assert!(matches!(
            store.prepare_execution(&job.job_id),
            Err(WorkspaceError::Video(VideoSafetyError::InputChanged))
        ));
        store
            .cleanup_unverified_output(&job.job_id)
            .expect("video cleanup");
        assert_eq!(fs::read(&input).expect("source remains"), b"changed-video");
        assert!(!root.path().join(&job.job_id).join("work").exists());
    }

    fn write_native_x4(store: &WorkspaceStore, job_id: &str) -> ImageUpscaleJobRequestV2 {
        let StoredRunnerRequest::ImageUpscaleV2(request) = store
            .load_stored_job(job_id)
            .expect("Goal 1B job must load")
            .runner_request
        else {
            panic!("Goal 1B job must use v2")
        };
        let input = image::open(&request.input.path)
            .expect("normalized input must decode")
            .into_rgb8();
        image::imageops::resize(
            &input,
            input.width() * 4,
            input.height() * 4,
            image::imageops::FilterType::Nearest,
        )
        .save_with_format(&request.output.path, ImageFormat::Png)
        .expect("native x4 output must save");
        request
    }

    fn jpeg_with_orientation(path: &Path, orientation: u16) {
        RgbImage::from_pixel(3, 2, Rgb([10, 20, 30]))
            .save_with_format(path, ImageFormat::Jpeg)
            .expect("JPEG fixture must save");
        let mut tiff = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(tiff);
        let length = u16::try_from(payload.len() + 2)
            .expect("EXIF fixture must fit")
            .to_be_bytes();
        let mut segment = vec![0xff, 0xe1, length[0], length[1]];
        segment.extend(payload);
        let mut bytes = fs::read(path).expect("JPEG fixture must read");
        bytes.splice(2..2, segment);
        fs::write(path, bytes).expect("oriented JPEG must save");
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
        store
            .recover_interrupted()
            .expect("legacy Goal 1A created job must survive recovery");
        assert_eq!(
            store.load_summary(&created.job_id).unwrap().status,
            JobStatus::Created
        );

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

    #[test]
    fn goal1b_jobs_normalize_orientation_and_publish_all_output_formats() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");

        for (index, format) in [
            ProductOutputFormat::Png,
            ProductOutputFormat::Jpeg,
            ProductOutputFormat::Webp,
        ]
        .into_iter()
        .enumerate()
        {
            let input = sources.path().join(format!("oriented-{index}.jpg"));
            jpeg_with_orientation(&input, 6);
            let created = store
                .create_image_job_v2(
                    &input,
                    goal1b_settings(format),
                    ImageBackend::OrtCpu,
                    Some(ImageBatchMetadata {
                        batch_id: "batch-a".into(),
                        index: u32::try_from(index + 1).unwrap(),
                        total: 3,
                    }),
                )
                .expect("Goal 1B job must be created");
            assert_eq!(created.batch_id.as_deref(), Some("batch-a"));
            assert_eq!(created.selected_backend, Some(ImageBackend::OrtCpu));
            let request = write_native_x4(&store, &created.job_id);
            assert_eq!((request.input.width, request.input.height), (2, 3));
            assert_eq!(request.input.path.file_name().unwrap(), "inference-rgb.png");
            assert_eq!(request.output.path.file_name().unwrap(), "sr-native-x4.png");
            assert_eq!(
                store
                    .load_stored_job(&created.job_id)
                    .expect("job must load")
                    .runner_id,
                "zoos-runner-ort"
            );
            store
                .record_runner_device(
                    &created.job_id,
                    "CPU_DEVICE",
                    "cpu:0 Apple M5 | ONNX Runtime 1.29.0",
                )
                .expect("CPU device evidence must record");

            store
                .publish_image_output(&created.job_id)
                .expect("output must publish");
            let final_path = created.output_path.expect("final path must exist");
            assert!(final_path.is_file());
            assert_eq!(
                final_path.extension().and_then(|value| value.to_str()),
                Some(output_extension(format))
            );
            let verification: ImagePipelineVerification = read_json(
                &workspace
                    .path()
                    .join(&created.job_id)
                    .join(VERIFICATION_FILE),
            )
            .expect("verification must load");
            assert_eq!(
                verification.source_sha256_before,
                verification.source_sha256_after
            );
            assert_eq!(verification.output_format, format);
            assert_eq!(
                (verification.output_width, verification.output_height),
                (4, 6)
            );
            assert_eq!(verification.actual_backend, ImageBackend::OrtCpu);
            let manifest: JobManifest =
                read_json(&workspace.path().join(&created.job_id).join(MANIFEST_FILE))
                    .expect("manifest must load");
            assert_eq!(manifest.final_sha256, Some(verification.output_sha256));
            assert_eq!(
                manifest.intermediate_sha256,
                Some(verification.intermediate_sha256)
            );
            assert_eq!(manifest.actual_device.as_deref(), Some("cpu:0 Apple M5"));
            assert_eq!(manifest.runtime_sha256.as_deref(), Some(ORT_RUNTIME_SHA256));
            assert_eq!(
                manifest.model_onnx_sha256.as_deref(),
                Some(PHOTO_ONNX_SHA256)
            );
            assert_eq!(manifest.model_param_sha256, None);
            assert_eq!(manifest.model_bin_sha256, None);
        }
    }

    #[test]
    fn goal1b_limits_and_format_neutral_suffixes_are_planned_before_execution() {
        assert!(matches!(
            validate_pipeline_dimensions(8_001, 1, 2),
            Err(WorkspaceError::Image(
                ImageSafetyError::OutputTooLarge { .. }
            ))
        ));
        assert!(matches!(
            validate_pipeline_dimensions(2_501, 2_500, 2),
            Err(WorkspaceError::Image(
                ImageSafetyError::OutputTooLarge { .. }
            ))
        ));

        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("photo.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let output_dir = sources.path().join("Upscaled");
        fs::create_dir(&output_dir).unwrap();
        fs::write(output_dir.join("photo_upscaled_2x.jpg"), b"existing").unwrap();
        let (final_path, partial_path) = plan_pipeline_destination(
            &input,
            2,
            ProductOutputFormat::Jpeg,
            "job-id",
            2,
            2,
            &HashSet::new(),
        )
        .expect("second JPEG name must plan");
        assert_eq!(final_path.file_name().unwrap(), "photo_upscaled_2x_2.jpg");
        assert_eq!(
            partial_path.file_name().unwrap(),
            ".photo_upscaled_2x_2.jpg.zoos-job-id.partial.jpg"
        );
    }

    #[test]
    fn goal1b_pending_jobs_reserve_same_stem_output_names() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let png = sources.path().join("photo.png");
        let jpeg = sources.path().join("photo.jpg");
        rgb_png(&png, 2, 2, [1, 2, 3]);
        RgbImage::from_pixel(2, 2, Rgb([4, 5, 6]))
            .save_with_format(&jpeg, ImageFormat::Jpeg)
            .expect("JPEG fixture must save");
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");

        let first = store
            .create_image_job_v2(
                &png,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .expect("first output must plan");
        let second = store
            .create_image_job_v2(
                &jpeg,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .expect("second output must plan around the pending reservation");

        assert_eq!(
            first.output_path.unwrap().file_name().unwrap(),
            "photo_upscaled_2x.png"
        );
        assert_eq!(
            second.output_path.unwrap().file_name().unwrap(),
            "photo_upscaled_2x_2.png"
        );
    }

    #[test]
    fn goal1b_alpha_is_recombined_and_alpha_jpeg_is_rejected() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("alpha.png");
        RgbaImage::from_pixel(2, 3, Rgba([10, 20, 30, 80]))
            .save_with_format(&input, ImageFormat::Png)
            .expect("RGBA fixture must save");
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");

        assert!(matches!(
            store.create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Jpeg),
                ImageBackend::VulkanGpu,
                None,
            ),
            Err(WorkspaceError::Pipeline(
                Goal1bImageError::AlphaJpegUnsupported
            ))
        ));
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::VulkanGpu,
                None,
            )
            .expect("alpha PNG job must create");
        let request = write_native_x4(&store, &created.job_id);
        assert!(request.input.path.with_file_name("alpha.png").is_file());
        store
            .record_runner_device(&created.job_id, "GPU_DEVICE", "gpu:0 Apple M5")
            .expect("GPU device evidence must record");
        store
            .publish_image_output(&created.job_id)
            .expect("alpha output must publish");
        assert!(matches!(
            image::open(created.output_path.unwrap()).expect("output must decode"),
            image::DynamicImage::ImageRgba8(_)
        ));
        let manifest: JobManifest =
            read_json(&workspace.path().join(&created.job_id).join(MANIFEST_FILE)).unwrap();
        assert_eq!(manifest.actual_device.as_deref(), Some("gpu:0 Apple M5"));
        assert_eq!(manifest.runtime_sha256.as_deref(), Some(GPU_RUNTIME_SHA256));
        assert_eq!(
            manifest.model_param_sha256.as_deref(),
            Some(PHOTO_PARAM_SHA256)
        );
        assert_eq!(manifest.model_bin_sha256.as_deref(), Some(PHOTO_BIN_SHA256));
        assert_eq!(manifest.model_onnx_sha256, None);
    }

    #[test]
    fn goal1b_publish_race_and_source_change_preserve_existing_files() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let raced = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Webp),
                ImageBackend::OrtCpu,
                None,
            )
            .expect("race job must create");
        write_native_x4(&store, &raced.job_id);
        let raced_final = raced.output_path.as_ref().unwrap();
        fs::write(raced_final, b"existing").expect("raced file must save");
        assert!(matches!(
            store.publish_image_output(&raced.job_id),
            Err(WorkspaceError::Image(ImageSafetyError::OutputExists(_)))
        ));
        assert_eq!(fs::read(raced_final).unwrap(), b"existing");
        let raced_pipeline: ImagePipelinePlan = read_json(
            &workspace
                .path()
                .join(&raced.job_id)
                .join(IMAGE_PIPELINE_FILE),
        )
        .unwrap();
        assert!(!raced_pipeline.destination_partial.exists());

        let changed = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .expect("changed job must create");
        let request = write_native_x4(&store, &changed.job_id);
        rgb_png(&input, 2, 2, [9, 8, 7]);
        assert!(matches!(
            store.recheck_image_input(&changed.job_id),
            Err(WorkspaceError::Image(ImageSafetyError::InputChanged))
        ));
        assert!(matches!(
            store.publish_image_output(&changed.job_id),
            Err(WorkspaceError::Image(ImageSafetyError::InputChanged))
        ));
        let pipeline: ImagePipelinePlan = read_json(
            &workspace
                .path()
                .join(&changed.job_id)
                .join(IMAGE_PIPELINE_FILE),
        )
        .unwrap();
        assert!(!pipeline.destination_partial.exists());
        assert!(!changed.output_path.unwrap().exists());
        assert!(request.output.path.exists());
    }

    #[test]
    fn goal1b_recovery_distinguishes_prepublish_intent_from_completed_rename() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");

        let before_rename = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .unwrap();
        let pipeline: ImagePipelinePlan = read_json(
            &workspace
                .path()
                .join(&before_rename.job_id)
                .join(IMAGE_PIPELINE_FILE),
        )
        .unwrap();
        rgb_png(&pipeline.destination_partial, 4, 4, [8, 7, 6]);
        fs::copy(&pipeline.destination_partial, &pipeline.destination_final)
            .expect("identical raced final must copy");
        let output_sha256 = crate::image_safety::sha256_file(&pipeline.destination_final).unwrap();
        write_json_atomic(
            &workspace
                .path()
                .join(&before_rename.job_id)
                .join(VERIFICATION_FILE),
            &ImagePipelineVerification {
                schema_version: 2,
                job_id: before_rename.job_id.clone(),
                actual_backend: ImageBackend::OrtCpu,
                source_path: input.clone(),
                source_sha256_before: pipeline.source_sha256.clone(),
                source_sha256_after: pipeline.source_sha256.clone(),
                inference_sha256: "0".repeat(64),
                intermediate_sha256: "1".repeat(64),
                output_path: pipeline.destination_final.clone(),
                output_sha256,
                output_format: ProductOutputFormat::Png,
                output_width: 4,
                output_height: 4,
                alpha_preserved: false,
                icc_preserved: false,
                exif_preserved: false,
            },
        )
        .unwrap();
        store
            .cleanup_unverified_output(&before_rename.job_id)
            .expect("pre-rename cleanup must succeed");
        assert!(pipeline.destination_final.exists());
        assert!(!pipeline.destination_partial.exists());

        let after_rename = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .unwrap();
        write_native_x4(&store, &after_rename.job_id);
        store
            .publish_image_output(&after_rename.job_id)
            .expect("second output must publish");
        let published = after_rename.output_path.unwrap();
        assert!(published.exists());
        store
            .cleanup_unverified_output(&after_rename.job_id)
            .expect("post-rename cleanup must roll back owned final");
        assert!(!published.exists());
    }

    #[test]
    fn goal1b_recovery_cleans_active_and_unstarted_jobs() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input_a = sources.path().join("a.png");
        let input_b = sources.path().join("b.png");
        rgb_png(&input_a, 2, 2, [1, 2, 3]);
        rgb_png(&input_b, 2, 2, [4, 5, 6]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let a = store
            .create_image_job_v2(
                &input_a,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::VulkanGpu,
                Some(ImageBatchMetadata {
                    batch_id: "batch".into(),
                    index: 1,
                    total: 2,
                }),
            )
            .unwrap();
        let b = store
            .create_image_job_v2(
                &input_b,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::VulkanGpu,
                Some(ImageBatchMetadata {
                    batch_id: "batch".into(),
                    index: 2,
                    total: 2,
                }),
            )
            .unwrap();
        let request = write_native_x4(&store, &a.job_id);
        let pipeline: ImagePipelinePlan =
            read_json(&workspace.path().join(&a.job_id).join(IMAGE_PIPELINE_FILE)).unwrap();
        fs::write(&pipeline.destination_partial, b"partial").unwrap();
        store
            .update_summary(&a.job_id, |summary| summary.status = JobStatus::Running)
            .unwrap();
        drop(store);

        let reopened = WorkspaceStore::new(workspace.path()).expect("workspace must reopen");
        reopened
            .recover_interrupted()
            .expect("recovery must finish");
        assert_eq!(
            reopened.load_summary(&a.job_id).unwrap().status,
            JobStatus::Interrupted
        );
        assert_eq!(
            reopened.load_summary(&b.job_id).unwrap().status,
            JobStatus::Cancelled
        );
        assert!(!request.input.path.exists());
        assert!(!request.output.path.exists());
        assert!(!pipeline.destination_partial.exists());
        assert_eq!(
            fs::read_dir(workspace.path().join(&b.job_id).join("work"))
                .unwrap()
                .count(),
            0
        );
        let b_manifest: JobManifest =
            read_json(&workspace.path().join(&b.job_id).join(MANIFEST_FILE)).unwrap();
        assert_eq!(
            b_manifest.result.as_deref(),
            Some("cancelled_after_restart")
        );
    }

    #[test]
    fn goal1b_recovery_removes_deterministic_gpu_upstream_partial_after_wrapper_crash() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::VulkanGpu,
                None,
            )
            .expect("GPU job must create");
        let private_output = workspace
            .path()
            .join(&created.job_id)
            .join("work/.sr-native-x4.png.zoos-upstream.partial.png");
        fs::write(
            &private_output,
            b"wrapper was killed after claiming this file",
        )
        .expect("crash residue fixture");
        store
            .update_summary(&created.job_id, |summary| {
                summary.status = JobStatus::Running
            })
            .expect("job must become active");
        drop(store);

        let reopened = WorkspaceStore::new(workspace.path()).expect("workspace must reopen");
        assert!(
            private_output.exists(),
            "regular owned residue is valid before recovery"
        );
        reopened
            .recover_interrupted()
            .expect("interrupted recovery must clean owned files");
        assert!(!private_output.exists());
        assert_eq!(
            reopened.load_summary(&created.job_id).unwrap().status,
            JobStatus::Interrupted
        );
    }

    #[cfg(unix)]
    #[test]
    fn goal1b_gpu_upstream_partial_symlink_is_quarantined_without_touching_target() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        let external = sources.path().join("external.txt");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        fs::write(&external, b"must remain unchanged").unwrap();
        let expected = fs::read(&external).unwrap();
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::VulkanGpu,
                None,
            )
            .expect("GPU job must create");
        let private_output = workspace
            .path()
            .join(&created.job_id)
            .join("work/.sr-native-x4.png.zoos-upstream.partial.png");
        symlink(&external, private_output).expect("malicious private output symlink");
        drop(store);

        let reopened = WorkspaceStore::new(workspace.path()).expect("startup must continue");
        assert!(reopened.list_jobs().unwrap().is_empty());
        assert_eq!(fs::read(external).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn goal1b_managed_work_symlink_is_quarantined_without_touching_target() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        let external = sources.path().join("external.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        rgb_png(&external, 2, 2, [9, 9, 9]);
        let expected = fs::read(&external).unwrap();
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .unwrap();
        let inference = workspace
            .path()
            .join(&created.job_id)
            .join("work/inference-rgb.png");
        fs::remove_file(&inference).unwrap();
        symlink(&external, &inference).unwrap();
        drop(store);

        let reopened = WorkspaceStore::new(workspace.path()).expect("startup must continue");
        assert!(reopened.list_jobs().unwrap().is_empty());
        assert_eq!(fs::read(external).unwrap(), expected);
    }

    #[test]
    fn goal1b_runner_request_must_match_product_settings_and_selected_backend() {
        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .expect("job must create");
        let job_dir = store
            .job_dir(&created.job_id)
            .expect("job directory must resolve safely");
        let StoredRunnerRequest::ImageUpscaleV2(request) = store
            .load_stored_job(&created.job_id)
            .expect("original request must load")
            .runner_request
        else {
            panic!("expected v2 request")
        };
        assert!(is_safe_image_request_v2(&job_dir, &created, &request));

        let mut wrong_model = request.clone();
        wrong_model.parameters.semantic_model = ImageSemanticModelV2::Anime;
        assert!(!is_safe_image_request_v2(&job_dir, &created, &wrong_model));

        let mut wrong_scale = request.clone();
        wrong_scale.parameters.requested_scale = 4;
        assert!(!is_safe_image_request_v2(&job_dir, &created, &wrong_scale));

        let mut wrong_device = request.clone();
        wrong_device.parameters.device = ImageDeviceV2::Vulkan { index: 0 };
        wrong_device.parameters.backend_settings = ImageBackendSettingsV2::Vulkan {
            tile_size: 256,
            threads: "1:2:2".into(),
        };
        assert!(!is_safe_image_request_v2(&job_dir, &created, &wrong_device));

        let mut wrong_cpu_settings = request;
        wrong_cpu_settings.parameters.backend_settings = ImageBackendSettingsV2::OrtCpu {
            tile_size: 64,
            intra_threads: 4,
            inter_threads: 1,
        };
        assert!(!is_safe_image_request_v2(
            &job_dir,
            &created,
            &wrong_cpu_settings
        ));
    }

    #[cfg(unix)]
    #[test]
    fn goal1b_dangling_managed_output_symlink_is_quarantined() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");
        let created = store
            .create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            )
            .unwrap();
        let output = workspace
            .path()
            .join(&created.job_id)
            .join("work/sr-native-x4.png");
        let external = sources.path().join("must-not-be-created.png");
        symlink(&external, &output).expect("dangling managed symlink");
        drop(store);

        let reopened = WorkspaceStore::new(workspace.path()).expect("startup must continue");
        assert!(reopened.list_jobs().unwrap().is_empty());
        assert!(!external.exists());
    }

    #[cfg(unix)]
    #[test]
    fn goal1b_destination_directory_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace must be created");
        let sources = tempfile::tempdir().expect("sources must be created");
        let outside = tempfile::tempdir().expect("outside must be created");
        let input = sources.path().join("input.png");
        rgb_png(&input, 2, 2, [1, 2, 3]);
        symlink(outside.path(), sources.path().join("Upscaled"))
            .expect("destination symlink must create");
        let store = WorkspaceStore::new(workspace.path()).expect("store must open");

        assert!(matches!(
            store.create_image_job_v2(
                &input,
                goal1b_settings(ProductOutputFormat::Png),
                ImageBackend::OrtCpu,
                None,
            ),
            Err(WorkspaceError::UnsafeRunnerRequest)
        ));
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
