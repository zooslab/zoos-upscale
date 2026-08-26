import { invoke, isTauri } from '@tauri-apps/api/core'
import type { FakeScenario, JobSummary } from '../types/jobs'

export function isDesktopRuntime(): boolean {
  return isTauri()
}

export function createFakeJob(scenario: FakeScenario): Promise<JobSummary> {
  return invoke<JobSummary>('create_fake_job', { scenario })
}

export function listJobs(): Promise<JobSummary[]> {
  return invoke<JobSummary[]>('list_jobs')
}

export function startJob(jobId: string): Promise<JobSummary> {
  return invoke<JobSummary>('start_job', { jobId })
}

export function cancelJob(jobId: string): Promise<JobSummary> {
  return invoke<JobSummary>('cancel_job', { jobId })
}

export function commandErrorMessage(error: unknown): string {
  if (
    typeof error === 'object' &&
    error !== null &&
    'message' in error &&
    typeof error.message === 'string'
  ) {
    return error.message
  }
  return '요청을 처리하지 못했습니다. 잠시 후 다시 시도해 주세요.'
}
