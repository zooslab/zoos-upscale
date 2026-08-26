import { cleanup, fireEvent, render, screen } from '@testing-library/svelte'
import { afterEach, describe, expect, it } from 'vitest'
import App from './App.svelte'

describe('App', () => {
  afterEach(() => cleanup())
  it('shows the image workflow and keeps the runner lab in development diagnostics', () => {
    render(App)

    expect(screen.getByRole('heading', { name: '이미지를 더 크게, 선명하게.' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: '이미지 업스케일' })).toBeTruthy()
    expect(screen.getByText('개발자 진단 · Runner Lab')).toBeTruthy()
    expect(screen.getByRole('heading', { name: '실행 경로 검증' })).toBeTruthy()
    expect(screen.getByText('Tauri v2 · Rust · Svelte 5')).toBeTruthy()
  })

  it('switches to the simple video interpolation workflow', async () => {
    render(App)
    await fireEvent.click(screen.getByRole('button', { name: /영상.*프레임 2배 보간/ }))
    expect(screen.getByRole('heading', { name: '영상을 더 부드럽게.' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: '영상 프레임 보간' })).toBeTruthy()
    expect(screen.getByText(/25 · 29.97 · 30 fps 영상을 정확히 2배/)).toBeTruthy()
  })
})
