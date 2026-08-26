import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { JobSummary, VideoEngineStatus } from '../lib/types/jobs'
import VideoInterpolate from './VideoInterpolate.svelte'

const api = vi.hoisted(() => ({
  cancelJob: vi.fn(),
  getVideoEngineStatus: vi.fn(),
  listJobs: vi.fn(),
  pickAndCreateVideoJob: vi.fn(),
}))
vi.mock('../lib/api/jobs', () => ({
  ...api,
  commandError: (error: unknown) => error,
  isDesktopRuntime: () => true,
}))

const unavailable = {
  state: 'NOT_INSTALLED' as const,
  code: 'ENGINE_NOT_INSTALLED',
  message: '설치되지 않았습니다.',
}
const ready = {
  state: 'READY' as const,
  code: null,
  message: '준비됨',
  engine_version: '1',
  device: 'local',
}
const allReady: VideoEngineStatus = {
  media: ready,
  gpu: ready,
  cpu: ready,
  recommended_backend: 'vulkan_gpu',
}

function videoJob(status: JobSummary['status'], error: JobSummary['error'] = null): JobSummary {
  return {
    job_id: 'video-1',
    kind: 'video_interpolate',
    input_name: 'sample.mov',
    output_path: '/Interpolated/sample_interpolated_2x.mov',
    video_settings: { backend: 'auto' },
    source_rate: { numerator: 30000, denominator: 1001 },
    target_rate: { numerator: 60000, denominator: 1001 },
    video_container: 'mov',
    selected_video_backend: 'vulkan_gpu',
    status,
    progress_percent: status === 'COMPLETED' ? 100 : 42,
    stage: status === 'RUNNING' ? '프레임 보간' : null,
    message: status === 'FAILED' ? '처리에 실패했습니다.' : '처리 중',
    error,
    created_at_ms: 1,
    updated_at_ms: 2,
  }
}

async function mounted() {
  render(VideoInterpolate)
  await screen.findByText('엔진 준비됨')
}

describe('VideoInterpolate Goal 2', () => {
  beforeEach(() => {
    for (const mock of Object.values(api)) mock.mockReset()
    api.getVideoEngineStatus.mockResolvedValue(allReady)
    api.listJobs.mockResolvedValue([])
    api.pickAndCreateVideoJob.mockResolvedValue(null)
  })

  afterEach(() => cleanup())

  it('shows media, GPU and CPU availability and disables selection without an engine', async () => {
    api.getVideoEngineStatus.mockResolvedValue({
      media: unavailable,
      gpu: unavailable,
      cpu: unavailable,
      recommended_backend: null,
    })
    render(VideoInterpolate)
    expect(await screen.findByText('미디어 · 사용 불가')).toBeTruthy()
    expect(screen.getByText('GPU · 사용 불가')).toBeTruthy()
    expect(screen.getByText('CPU · 사용 불가')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'MP4 · MOV · MKV 선택' }).hasAttribute('disabled')).toBe(true)
    expect(screen.getByText('ENGINE_NOT_INSTALLED')).toBeTruthy()
  })

  it('creates and automatically tracks a running job without calling start_job', async () => {
    api.pickAndCreateVideoJob.mockResolvedValue(videoJob('RUNNING'))
    await mounted()
    await fireEvent.click(screen.getByRole('button', { name: 'CPU' }))
    await fireEvent.click(screen.getByRole('button', { name: 'MP4 · MOV · MKV 선택' }))
    await waitFor(() => expect(api.pickAndCreateVideoJob).toHaveBeenCalledWith('ncnn_cpu'))
    expect(await screen.findByText('프레임 보간 중')).toBeTruthy()
    expect(screen.getByText('29.97')).toBeTruthy()
    expect(screen.getByText('59.94 fps')).toBeTruthy()
  })

  it('cancels the active video job', async () => {
    api.listJobs.mockResolvedValue([videoJob('RUNNING')])
    api.cancelJob.mockResolvedValue(videoJob('CANCELLED'))
    await mounted()
    await fireEvent.click(await screen.findByRole('button', { name: '작업 취소' }))
    await waitFor(() => expect(api.cancelJob).toHaveBeenCalledWith('video-1'))
    expect((await screen.findAllByText('취소됨')).length).toBeGreaterThan(0)
  })

  it('shows the completed output path', async () => {
    api.listJobs.mockResolvedValue([videoJob('COMPLETED')])
    await mounted()
    expect(await screen.findByText('저장 완료')).toBeTruthy()
    expect(screen.getByText('/Interpolated/sample_interpolated_2x.mov')).toBeTruthy()
  })

  it('shows a structured job error', async () => {
    api.listJobs.mockResolvedValue([
      videoJob('FAILED', { code: 'UNSUPPORTED_VIDEO', message: '지원하지 않는 영상입니다.' }),
    ])
    await mounted()
    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('UNSUPPORTED_VIDEO')
    expect(alert.textContent).toContain('지원하지 않는 영상입니다.')
  })
})
