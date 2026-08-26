export type FakeScenario =
  | 'success'
  | 'failed'
  | 'malformed_ndjson'
  | 'crash'
  | 'hang'
  | 'completed_then_nonzero'
  | 'spawn_grandchild_and_hang'

export type JobKind = 'fake_validation' | 'image_upscale'
export type ImagePreset = 'photo' | 'anime'
export type ImageScale = 2 | 4

export interface ImageSettings {
  preset: ImagePreset
  scale: ImageScale
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

export interface ImageEngineStatus {
  state: ImageEngineState
  code: string | null
  message: string
  engine_version?: string
  device?: string
}

export const activeStatuses = new Set<JobStatus>([
  'PROBING',
  'PLANNING',
  'RUNNING',
  'VERIFYING',
])
