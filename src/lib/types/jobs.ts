export type FakeScenario =
  | 'success'
  | 'failed'
  | 'malformed_ndjson'
  | 'crash'
  | 'hang'
  | 'completed_then_nonzero'
  | 'spawn_grandchild_and_hang'

export type JobKind = 'fake_validation' | 'image_upscale' | 'video_interpolate'
export type ImagePreset = 'photo' | 'anime'
export type ImageScale = 2 | 4
export type ImageBackend = 'auto' | 'vulkan_gpu' | 'ort_cpu'
export type ConcreteImageBackend = Exclude<ImageBackend, 'auto'>
export type ImageOutputFormat = 'png' | 'jpeg' | 'webp'
export type MetadataPolicy = 'preserve' | 'strip'
export type VideoBackend = 'auto' | 'vulkan_gpu' | 'ncnn_cpu'
export type ConcreteVideoBackend = Exclude<VideoBackend, 'auto'>
export type VideoContainer = 'mp4' | 'mov' | 'mkv'

export interface RationalRate {
  numerator: number
  denominator: number
}

export interface VideoSettings {
  backend: VideoBackend
}

export interface ImageSettings {
  preset: ImagePreset
  scale: ImageScale
  backend: ImageBackend
  output_format: ImageOutputFormat
  metadata: MetadataPolicy
}

export type JobStatus =
  | 'CREATED'
  | 'PROBING'
  | 'PLANNING'
  | 'RUNNING'
  | 'VERIFYING'
  | 'COMPLETED'
  | 'FAILED'
  | 'CANCELLED'
  | 'INTERRUPTED'

export interface JobErrorView {
  code: string
  message: string
}

export interface JobSummary {
  job_id: string
  kind: JobKind
  input_name?: string
  output_path?: string
  image_settings?: ImageSettings
  video_settings?: VideoSettings
  source_rate?: RationalRate
  target_rate?: RationalRate
  video_container?: VideoContainer
  batch_id?: string
  batch_index?: number
  batch_total?: number
  selected_backend?: ConcreteImageBackend
  selected_video_backend?: ConcreteVideoBackend
  scenario?: FakeScenario
  status: JobStatus
  progress_percent: number
  stage: string | null
  message: string
  error: JobErrorView | null
  created_at_ms: number
  updated_at_ms: number
}

export type ImageEngineState = 'READY' | 'NOT_INSTALLED' | 'INVALID'

export interface ImageBackendStatus {
  state: ImageEngineState
  code: string | null
  message: string
  engine_version?: string
  device?: string
}

export interface ImageEngineStatus {
  gpu: ImageBackendStatus
  cpu: ImageBackendStatus
  recommended_backend: ConcreteImageBackend | null
}

export interface VideoEngineStatus {
  media: ImageBackendStatus
  gpu: ImageBackendStatus
  cpu: ImageBackendStatus
  recommended_backend: ConcreteVideoBackend | null
}

export interface BatchRejectedInput {
  input_name: string
  code: string
  message: string
}

export interface ImageBatchCreation {
  batch_id: string
  jobs: JobSummary[]
  rejected: BatchRejectedInput[]
}

export const activeStatuses = new Set<JobStatus>([
  'PROBING',
  'PLANNING',
  'RUNNING',
  'VERIFYING',
])
