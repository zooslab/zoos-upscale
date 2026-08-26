import { beforeEach, describe, expect, it, vi } from 'vitest'

const tauri = vi.hoisted(() => ({ invoke: vi.fn(), isTauri: vi.fn(() => true) }))

vi.mock('@tauri-apps/api/core', () => tauri)

import {
  cancelBatch,
  commandError,
  pickAndCreateImageBatch,
  pickAndCreateImageJob,
  getVideoEngineStatus,
  pickAndCreateVideoJob,
} from './jobs'

describe('image job API', () => {
  beforeEach(() => tauri.invoke.mockReset())

  it('uses camelCase invoke arguments for the native picker command', async () => {
    tauri.invoke.mockResolvedValue(null)

    await expect(
      pickAndCreateImageJob('anime', 4, 'ort_cpu', 'webp', 'strip'),
    ).resolves.toBeNull()
    expect(tauri.invoke).toHaveBeenCalledWith('pick_and_create_image_job', {
      preset: 'anime',
      scale: 4,
      backend: 'ort_cpu',
      outputFormat: 'webp',
      metadata: 'strip',
    })
  })

  it('passes all processing options to batch selection and uses the batch id for cancel', async () => {
    tauri.invoke.mockResolvedValueOnce(null).mockResolvedValueOnce(undefined)

    await pickAndCreateImageBatch('photo', 2, 'auto', 'jpeg', 'preserve')
    expect(tauri.invoke).toHaveBeenNthCalledWith(1, 'pick_and_create_image_batch', {
      preset: 'photo', scale: 2, backend: 'auto', outputFormat: 'jpeg', metadata: 'preserve',
    })

    await cancelBatch('batch-7')
    expect(tauri.invoke).toHaveBeenNthCalledWith(2, 'cancel_batch', { batchId: 'batch-7' })
  })

  it('preserves structured command errors and parses serialized errors', () => {
    expect(commandError({ code: 'OUTPUT_EXISTS', message: '이미 존재합니다.' })).toEqual({
      code: 'OUTPUT_EXISTS',
      message: '이미 존재합니다.',
    })
    expect(commandError('{"code":"INPUT_CHANGED","message":"입력이 변경되었습니다."}')).toEqual({
      code: 'INPUT_CHANGED',
      message: '입력이 변경되었습니다.',
    })
  })

  it('uses the Goal 2 status and atomic video picker commands', async () => {
    tauri.invoke.mockResolvedValueOnce({ media: {}, gpu: {}, cpu: {}, recommended_backend: null }).mockResolvedValueOnce(null)
    await getVideoEngineStatus()
    await pickAndCreateVideoJob('ncnn_cpu')
    expect(tauri.invoke).toHaveBeenNthCalledWith(1, 'get_video_engine_status')
    expect(tauri.invoke).toHaveBeenNthCalledWith(2, 'pick_and_create_video_job', {
      backend: 'ncnn_cpu',
    })
  })
})
