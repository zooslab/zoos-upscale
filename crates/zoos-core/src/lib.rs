mod domain;
mod image_pipeline;
mod image_safety;
mod orchestrator;
mod process;
mod video_safety;
mod workspace;

pub use domain::{
    ImageBackend, ImageBatchMetadata, ImageOutputFormat, ImagePreset, ImageSettings,
    JPEG_OUTPUT_QUALITY, JobErrorView, JobKind, JobStatus, JobSummary, MetadataPolicy,
    RationalRate, VideoBackend, VideoContainer, VideoSettings,
};
pub use image_pipeline::{
    Goal1bImageError, ImageMetadata, ImagePipelineLimits, MetadataPolicy as PipelineMetadataPolicy,
    OutputEncoding, PreparedImage, VerifiedPipelineOutput, prepare_image_input,
    render_pipeline_output, verify_pipeline_output,
};
pub use image_safety::{
    ImageOutputPlan, ImageSafetyError, ImageVerification, ValidatedImageInput,
    cleanup_owned_output, plan_image_output, publish_verified_output, recheck_input,
    validate_image_input, verify_partial_output,
};
pub use orchestrator::{JobOrchestrator, OrchestratorError};
pub use process::{BackendError, ProcessExecutionBackend, RunnerLaunchSpec, RunnerRegistry};
pub use video_safety::{
    ValidatedVideoFile, VideoOutputPlan, VideoPipelineVerification, VideoSafetyError,
    cleanup_owned_video_output, cleanup_video_work_directory, plan_video_output,
    publish_staged_video_output, recheck_video_input, stage_private_video_output,
    validate_video_file,
};
pub use workspace::{ImagePipelineVerification, WorkspaceError};
pub use zoos_media::{
    Ffprobe, MediaChapterDescriptor, MediaDescriptor, MediaError, MediaStreamDescriptor,
    verify_interpolated_output,
};
pub use zoos_runner_protocol::FakeBehavior;
