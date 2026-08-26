import { beforeEach, describe, expect, it, vi } from 'vitest'

const tauri = vi.hoisted(() => ({ invoke: vi.fn(), isTauri: vi.fn(() => true) }))

vi.mock('@tauri-apps/api/core', () => tauri)

import { commandError, pickAndCreateImageJob } from './jobs'

describe('image job API', () => {
  beforeEach(() => tauri.invoke.mockReset())

  it('uses camelCase invoke arguments for the native picker command', async () => {
    tauri.invoke.mockResolvedValue(null)

    await expect(pickAndCreateImageJob('anime', 4)).resolves.toBeNull()
    expect(tauri.invoke).toHaveBeenCalledWith('pick_and_create_image_job', {
      preset: 'anime',
      scale: 4,
    })
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
})
