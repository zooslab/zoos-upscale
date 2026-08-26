import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ImageEngineStatus, JobSummary } from '../lib/types/jobs'
import ImageUpscale from './ImageUpscale.svelte'

const api = vi.hoisted(() => ({
  cancelBatch: vi.fn(), cancelJob: vi.fn(), getImageEngineStatus: vi.fn(), listJobs: vi.fn(),
  pickAndCreateImageBatch: vi.fn(), pickAndCreateImageJob: vi.fn(), startJob: vi.fn(),
}))
vi.mock('../lib/api/jobs', () => ({ ...api, commandError: (error: unknown) => error, isDesktopRuntime: () => true }))

const unavailable = { state: 'NOT_INSTALLED' as const, code: 'ENGINE_NOT_INSTALLED', message: '설치되지 않았습니다.' }
const ready = { state: 'READY' as const, code: null, message: '준비됨', engine_version: '1', device: 'local' }
const bothReady: ImageEngineStatus = { gpu: ready, cpu: ready, recommended_backend: 'vulkan_gpu' }
function job(id = 'image-1', status: JobSummary['status'] = 'CREATED', batch?: { id: string; index: number; total: number }): JobSummary {
  return {
    job_id: id, kind: 'image_upscale', input_name: `${id}.png`, output_path: `/Upscaled/${id}.png`,
    image_settings: { preset: 'photo', scale: 2, backend: 'auto', output_format: 'png', metadata: 'preserve' },
    batch_id: batch?.id, batch_index: batch?.index, batch_total: batch?.total,
    status, progress_percent: status === 'COMPLETED' ? 100 : 0, stage: null, message: status,
    error: status === 'FAILED' ? { code: 'UPSTREAM_FAILED', message: '처리 실패' } : null,
    created_at_ms: 1, updated_at_ms: 1,
  }
}
async function mounted() { render(ImageUpscale); await screen.findByText('엔진 준비됨') }

describe('ImageUpscale Goal 1B', () => {
  beforeEach(() => {
    for (const mock of Object.values(api)) mock.mockReset()
    api.getImageEngineStatus.mockResolvedValue(bothReady); api.listJobs.mockResolvedValue([])
    api.cancelBatch.mockResolvedValue(undefined)
    api.cancelJob.mockImplementation(async (id: string) => job(id, 'CANCELLED'))
  })
  afterEach(() => { cleanup(); vi.useRealTimers() })

  it('shows each engine independently and disables an unavailable backend', async () => {
    api.getImageEngineStatus.mockResolvedValue({ gpu: unavailable, cpu: ready, recommended_backend: 'ort_cpu' })
    await mounted()
    expect(screen.getByText('GPU · 사용 불가')).toBeTruthy()
    expect(screen.getByText('CPU · 준비됨')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'GPU' }).hasAttribute('disabled')).toBe(true)
    expect(screen.getByRole('button', { name: 'CPU' }).hasAttribute('disabled')).toBe(false)
  })

  it('disables both pickers and explains the error when no backend is ready', async () => {
    api.getImageEngineStatus.mockResolvedValue({ gpu: unavailable, cpu: unavailable, recommended_backend: null })
    render(ImageUpscale)
    expect(await screen.findByText('ENGINE_NOT_INSTALLED')).toBeTruthy()
    expect(screen.getByRole('button', { name: '이미지 선택' }).hasAttribute('disabled')).toBe(true)
    expect(screen.getByRole('button', { name: '폴더 일괄' }).hasAttribute('disabled')).toBe(true)
  })

  it('passes preset, scale, backend, format and metadata to the single picker then starts it', async () => {
    api.pickAndCreateImageJob.mockResolvedValue(job()); api.startJob.mockResolvedValue(job('image-1', 'RUNNING'))
    await mounted()
    await fireEvent.click(screen.getByRole('button', { name: '애니' }))
    await fireEvent.click(screen.getByRole('button', { name: '4배' }))
    await fireEvent.click(screen.getByRole('button', { name: 'CPU' }))
    await fireEvent.click(screen.getByRole('button', { name: 'WebP' }))
    await fireEvent.click(screen.getByRole('button', { name: '제거' }))
    await fireEvent.click(screen.getByRole('button', { name: '이미지 선택' }))
    await waitFor(() => expect(api.pickAndCreateImageJob).toHaveBeenCalledWith('anime', 4, 'ort_cpu', 'webp', 'strip'))
    expect(api.startJob).toHaveBeenCalledWith('image-1')
  })

  it('resynchronizes after a single start failure', async () => {
    const created = job(); api.pickAndCreateImageJob.mockResolvedValue(created)
    api.startJob.mockRejectedValue({ code: 'ENGINE_NOT_INSTALLED', message: '엔진 변경' })
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValueOnce([created])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '이미지 선택' }))
    expect((await screen.findByRole('alert')).textContent).toContain('ENGINE_NOT_INSTALLED')
    await waitFor(() => {
      expect(api.listJobs).toHaveBeenCalledTimes(2); expect(api.getImageEngineStatus).toHaveBeenCalledTimes(2)
      expect(api.cancelJob).toHaveBeenCalledWith('image-1')
    })
  })

  it('starts a batch sequentially, shows rejected inputs, and continues after a file fails', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const first = job('first', 'CREATED', { id: 'batch-1', index: 1, total: 2 })
    const second = job('second', 'CREATED', { id: 'batch-1', index: 2, total: 2 })
    api.pickAndCreateImageBatch.mockResolvedValue({ batch_id: 'batch-1', jobs: [first, second], rejected: [{ input_name: 'bad.txt', code: 'UNSUPPORTED_IMAGE_MODE', message: '지원하지 않음' }] })
    api.startJob.mockResolvedValueOnce(job('first', 'RUNNING', { id: 'batch-1', index: 1, total: 2 })).mockResolvedValueOnce(job('second', 'RUNNING', { id: 'batch-1', index: 2, total: 2 }))
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValueOnce([job('first', 'FAILED', { id: 'batch-1', index: 1, total: 2 }), second])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '폴더 일괄' }))
    expect(api.pickAndCreateImageBatch).toHaveBeenCalledWith('photo', 2, 'auto', 'png', 'preserve')
    await waitFor(() => expect(api.startJob).toHaveBeenNthCalledWith(1, 'first'))
    expect(screen.getByText(/선택하지 못한 파일 1개/).textContent).toContain('bad.txt')
    await vi.advanceTimersByTimeAsync(550)
    await waitFor(() => expect(api.startJob).toHaveBeenNthCalledWith(2, 'second'))
    expect(screen.getByText('실패')).toBeTruthy()
  })

  it('continues to the next batch item when starting one item throws', async () => {
    const first = job('first', 'CREATED', { id: 'batch-2', index: 1, total: 2 }), second = job('second', 'CREATED', { id: 'batch-2', index: 2, total: 2 })
    api.pickAndCreateImageBatch.mockResolvedValue({ batch_id: 'batch-2', jobs: [first, second], rejected: [] })
    api.startJob.mockRejectedValueOnce({ code: 'UPSTREAM_FAILED', message: '첫 파일 실패' }).mockResolvedValueOnce(job('second', 'RUNNING', { id: 'batch-2', index: 2, total: 2 }))
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValueOnce([first, second])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '폴더 일괄' }))
    await waitFor(() => expect(api.startJob).toHaveBeenCalledTimes(2))
    expect(api.startJob).toHaveBeenNthCalledWith(2, 'second')
    expect(api.cancelJob).toHaveBeenCalledWith('first')
    expect(screen.getByText('시작 실패')).toBeTruthy()
  })

  it('retries a transient JOB_BUSY without abandoning the created batch job', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const first = job('first', 'CREATED', { id: 'batch-busy', index: 1, total: 1 })
    api.pickAndCreateImageBatch.mockResolvedValue({ batch_id: 'batch-busy', jobs: [first], rejected: [] })
    api.startJob.mockRejectedValueOnce({ code: 'JOB_BUSY', message: '정리 중' }).mockResolvedValueOnce(job('first', 'RUNNING', { id: 'batch-busy', index: 1, total: 1 }))
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValue([first])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '폴더 일괄' }))
    await waitFor(() => expect(api.startJob).toHaveBeenCalledTimes(1))
    expect(api.cancelJob).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(550)
    await waitFor(() => expect(api.startJob).toHaveBeenCalledTimes(2))
    expect(api.cancelJob).not.toHaveBeenCalled()
  })

  it('does not let a quarantined batch record keep polling forever', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const first = job('first', 'CREATED', { id: 'batch-quarantine', index: 1, total: 2 })
    const second = job('second', 'CREATED', { id: 'batch-quarantine', index: 2, total: 2 })
    api.pickAndCreateImageBatch.mockResolvedValue({ batch_id: 'batch-quarantine', jobs: [first, second], rejected: [] })
    api.startJob.mockResolvedValueOnce(job('first', 'RUNNING', { id: 'batch-quarantine', index: 1, total: 2 })).mockResolvedValueOnce(job('second', 'RUNNING', { id: 'batch-quarantine', index: 2, total: 2 }))
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValueOnce([second]).mockResolvedValue([job('second', 'COMPLETED', { id: 'batch-quarantine', index: 2, total: 2 })])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '폴더 일괄' }))
    await vi.advanceTimersByTimeAsync(550)
    await waitFor(() => expect(api.startJob).toHaveBeenCalledTimes(2))
    await vi.advanceTimersByTimeAsync(550)
    expect(await screen.findByText('일괄 작업 2/2')).toBeTruthy()
    expect(screen.getByText('기록 격리됨')).toBeTruthy()
    expect(screen.getByText('일괄 작업 완료')).toBeTruthy()
  })

  it('cancels the active and pending batch through one batch command', async () => {
    const first = job('first', 'CREATED', { id: 'batch-3', index: 1, total: 2 }), second = job('second', 'CREATED', { id: 'batch-3', index: 2, total: 2 })
    api.pickAndCreateImageBatch.mockResolvedValue({ batch_id: 'batch-3', jobs: [first, second], rejected: [] })
    api.startJob.mockResolvedValue(job('first', 'RUNNING', { id: 'batch-3', index: 1, total: 2 }))
    api.listJobs.mockResolvedValueOnce([]).mockResolvedValueOnce([job('first', 'CANCELLED', { id: 'batch-3', index: 1, total: 2 }), job('second', 'CANCELLED', { id: 'batch-3', index: 2, total: 2 })])
    await mounted(); await fireEvent.click(screen.getByRole('button', { name: '폴더 일괄' }))
    const cancel = await screen.findByRole('button', { name: '일괄 작업 취소' }); await fireEvent.click(cancel)
    await waitFor(() => expect(api.cancelBatch).toHaveBeenCalledWith('batch-3'))
    expect(api.cancelJob).not.toHaveBeenCalled(); expect(await screen.findByText('일괄 작업 취소됨')).toBeTruthy()
  })
})
