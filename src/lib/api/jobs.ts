import { invoke, isTauri } from '@tauri-apps/api/core'
import type {
  FakeScenario,
  ImageEngineStatus,
  ImagePreset,
  ImageScale,
  JobErrorView,
  JobSummary,
} from '../types/jobs'

export function isDesktopRuntime(): boolean {
  return isTauri()
}

export function createFakeJob(scenario: FakeScenario): Promise<JobSummary> {
  return invoke<JobSummary>('create_fake_job', { scenario })
}

export function getImageEngineStatus(): Promise<ImageEngineStatus> {
  return invoke<ImageEngineStatus>('get_image_engine_status')
}

export function pickAndCreateImageJob(
  preset: ImagePreset,
  scale: ImageScale,
): Promise<JobSummary | null> {
  return invoke<JobSummary | null>('pick_and_create_image_job', { preset, scale })
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
  return commandError(error).message
}

export function commandError(error: unknown): JobErrorView {
  if (
    typeof error === 'object' &&
    error !== null &&
    'message' in error &&
    typeof error.message === 'string'
  ) {
    const code =
      'code' in error && typeof error.code === 'string' ? error.code : 'COMMAND_FAILED'
    return { code, message: error.message }
  }
  if (typeof error === 'string') {
    try {
      const parsed: unknown = JSON.parse(error)
      if (
        typeof parsed === 'object' &&
        parsed !== null &&
        'message' in parsed &&
        typeof parsed.message === 'string'
      ) {
        return {
          code:
            'code' in parsed && typeof parsed.code === 'string'
              ? parsed.code
              : 'COMMAND_FAILED',
          message: parsed.message,
        }
      }
    } catch {
      return { code: 'COMMAND_FAILED', message: error }
    }
  }
  return {
    code: 'COMMAND_FAILED',
    message: '요청을 처리하지 못했습니다. 잠시 후 다시 시도해 주세요.',
  }
}
