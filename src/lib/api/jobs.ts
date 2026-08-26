import { invoke, isTauri } from '@tauri-apps/api/core'
import type {
  FakeScenario,
  ImageBackend,
  ImageBatchCreation,
  ImageEngineStatus,
  ImageOutputFormat,
  ImagePreset,
  ImageScale,
  JobErrorView,
  JobSummary,
  MetadataPolicy,
  VideoBackend,
  VideoEngineStatus,
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

export function getVideoEngineStatus(): Promise<VideoEngineStatus> {
  return invoke<VideoEngineStatus>('get_video_engine_status')
}

export function pickAndCreateVideoJob(backend: VideoBackend): Promise<JobSummary | null> {
  return invoke<JobSummary | null>('pick_and_create_video_job', { backend })
}

export function pickAndCreateImageJob(
  preset: ImagePreset,
  scale: ImageScale,
  backend: ImageBackend,
  outputFormat: ImageOutputFormat,
  metadata: MetadataPolicy,
): Promise<JobSummary | null> {
  return invoke<JobSummary | null>('pick_and_create_image_job', {
    preset,
    scale,
    backend,
    outputFormat,
    metadata,
  })
}

export function pickAndCreateImageBatch(
  preset: ImagePreset,
  scale: ImageScale,
  backend: ImageBackend,
  outputFormat: ImageOutputFormat,
  metadata: MetadataPolicy,
): Promise<ImageBatchCreation | null> {
  return invoke<ImageBatchCreation | null>('pick_and_create_image_batch', {
    preset,
    scale,
    backend,
    outputFormat,
    metadata,
  })
}

export function cancelBatch(batchId: string): Promise<void> {
  return invoke<void>('cancel_batch', { batchId })
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
