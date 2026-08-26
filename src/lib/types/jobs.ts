export type FakeScenario =
  | 'success'
  | 'failed'
  | 'malformed_ndjson'
  | 'crash'
  | 'hang'
  | 'completed_then_nonzero'
  | 'spawn_grandchild_and_hang'

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
  scenario: FakeScenario
  status: JobStatus
  progress_percent: number
  stage: string | null
  message: string
  error: JobErrorView | null
  created_at_ms: number
  updated_at_ms: number
}

export const activeStatuses = new Set<JobStatus>([
  'PROBING',
  'PLANNING',
  'RUNNING',
  'VERIFYING',
])
