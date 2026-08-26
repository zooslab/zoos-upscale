import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { JobSummary } from '../lib/types/jobs'
import JobLab from './JobLab.svelte'

const api = vi.hoisted(() => ({
  cancelJob: vi.fn(),
  createFakeJob: vi.fn(),
  listJobs: vi.fn(),
  startJob: vi.fn(),
}))

vi.mock('../lib/api/jobs', () => ({
  ...api,
  commandErrorMessage: () => '요청 실패',
  isDesktopRuntime: () => true,
}))

function job(overrides: Partial<JobSummary> = {}): JobSummary {
  return {
    job_id: 'job-1',
    kind: 'fake_validation',
    scenario: 'success',
    status: 'CREATED',
    progress_percent: 0,
    stage: null,
    message: 'Ready to start',
    error: null,
    created_at_ms: 1,
    updated_at_ms: 1,
    ...overrides,
  }
}

describe('JobLab', () => {
  beforeEach(() => {
    api.cancelJob.mockReset()
    api.createFakeJob.mockReset()
    api.listJobs.mockReset().mockResolvedValue([])
    api.startJob.mockReset()
  })

  afterEach(() => cleanup())

  it('creates and starts a fake job through typed commands', async () => {
    api.createFakeJob.mockResolvedValue(job())
    api.startJob.mockResolvedValue(job({ status: 'RUNNING' }))

    render(JobLab)
    await fireEvent.click(screen.getByRole('button', { name: '검증 시작' }))

    await waitFor(() => {
      expect(api.createFakeJob).toHaveBeenCalledWith('success')
      expect(api.startJob).toHaveBeenCalledWith('job-1')
    })
  })

  it('cancels only the active job id', async () => {
    api.listJobs.mockResolvedValue([job({ status: 'RUNNING', progress_percent: 35 })])
    api.cancelJob.mockResolvedValue(job({ status: 'RUNNING', stage: 'cancelling' }))

    render(JobLab)
    const cancel = await screen.findByRole('button', { name: '취소' })
    await fireEvent.click(cancel)

    await waitFor(() => expect(api.cancelJob).toHaveBeenCalledWith('job-1'))
  })
})
