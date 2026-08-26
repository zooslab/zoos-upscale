import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ImageEngineStatus, JobSummary } from '../lib/types/jobs'
import ImageUpscale from './ImageUpscale.svelte'

const api = vi.hoisted(() => ({
  cancelJob: vi.fn(), getImageEngineStatus: vi.fn(), listJobs: vi.fn(),
  pickAndCreateImageJob: vi.fn(), startJob: vi.fn(),
}))

vi.mock('../lib/api/jobs', () => ({
  ...api,
  commandError: (error: unknown) => error,
  isDesktopRuntime: () => true,
}))

const readyEngine: ImageEngineStatus = {
  state: 'READY', code: null, message: 'Ready', engine_version: '0.2.5.0', device: 'Apple M5',
}

function imageJob(overrides: Partial<JobSummary> = {}): JobSummary {
  return {
    job_id: 'image-1', kind: 'image_upscale', input_name: 'sample.png',
    output_path: '/Pictures/Upscaled/sample_upscaled_2x.png',
    image_settings: { preset: 'photo', scale: 2 }, status: 'CREATED',
    progress_percent: 0, stage: null, message: 'Ready', error: null,
    created_at_ms: 1, updated_at_ms: 1, ...overrides,
  }
}

describe('ImageUpscale', () => {
  beforeEach(() => {
    api.cancelJob.mockReset()
    api.getImageEngineStatus.mockReset().mockResolvedValue(readyEngine)
    api.listJobs.mockReset().mockResolvedValue([])
    api.pickAndCreateImageJob.mockReset()
    api.startJob.mockReset()
  })

  afterEach(() => cleanup())

  it('explains when the engine is not installed and disables selection', async () => {
    api.getImageEngineStatus.mockResolvedValue({
      state: 'NOT_INSTALLED', code: 'ENGINE_NOT_INSTALLED', message: '검증된 엔진 캐시가 없습니다.',
    })
    render(ImageUpscale)
    expect(await screen.findByText('ENGINE_NOT_INSTALLED')).toBeTruthy()
    expect(screen.getByText('검증된 엔진 캐시가 없습니다.')).toBeTruthy()
    expect(screen.queryByRole('button', { name: '이미지 선택' })).toBeNull()
  })

  it('uses the selected preset and scale, then automatically starts the created job', async () => {
    api.pickAndCreateImageJob.mockResolvedValue(imageJob({ image_settings: { preset: 'anime', scale: 4 } }))
    api.startJob.mockResolvedValue(imageJob({
      image_settings: { preset: 'anime', scale: 4 }, status: 'RUNNING', progress_percent: 3,
    }))
    render(ImageUpscale)
    await screen.findByText('엔진 준비됨')
    await fireEvent.click(screen.getByRole('button', { name: '애니' }))
    await fireEvent.click(screen.getByRole('button', { name: '4배' }))
    await fireEvent.click(screen.getByRole('button', { name: '이미지 선택' }))
    await waitFor(() => {
      expect(api.pickAndCreateImageJob).toHaveBeenCalledWith('anime', 4)
      expect(api.startJob).toHaveBeenCalledWith('image-1')
    })
  })

  it('shows running progress and cancels only the current image job', async () => {
    api.listJobs.mockResolvedValue([
      imageJob({ status: 'RUNNING', progress_percent: 42, stage: '타일 처리 중' }),
    ])
    api.cancelJob.mockResolvedValue(imageJob({ status: 'CANCELLED', progress_percent: 42 }))
    render(ImageUpscale)
    expect(await screen.findByText('타일 처리 중')).toBeTruthy()
    expect(screen.getByRole('progressbar', { name: '업스케일 진행률' }).getAttribute('aria-valuenow')).toBe('42')
    await fireEvent.click(screen.getByRole('button', { name: '작업 취소' }))
    await waitFor(() => expect(api.cancelJob).toHaveBeenCalledWith('image-1'))
  })

  it('shows the final output path for a completed job', async () => {
    api.listJobs.mockResolvedValue([imageJob({ status: 'COMPLETED', progress_percent: 100 })])
    render(ImageUpscale)
    expect(await screen.findByText('저장 완료')).toBeTruthy()
    expect(screen.getByText('/Pictures/Upscaled/sample_upscaled_2x.png')).toBeTruthy()
    expect(screen.queryByRole('button', { name: '작업 취소' })).toBeNull()
  })

  it('renders a stable structured command error', async () => {
    api.pickAndCreateImageJob.mockRejectedValue({
      code: 'UNSUPPORTED_IMAGE_MODE', message: 'RGB8 PNG 또는 JPEG만 지원합니다.',
    })
    render(ImageUpscale)
    await screen.findByText('엔진 준비됨')
    await fireEvent.click(screen.getByRole('button', { name: '이미지 선택' }))
    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('UNSUPPORTED_IMAGE_MODE')
    expect(alert.textContent).toContain('RGB8 PNG 또는 JPEG만 지원합니다.')
  })
})
