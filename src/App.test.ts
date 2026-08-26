import { render, screen } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import App from './App.svelte'

describe('App', () => {
  it('shows the image workflow and keeps the runner lab in development diagnostics', () => {
    render(App)

    expect(screen.getByRole('heading', { name: '이미지를 더 크게, 선명하게.' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: '이미지 업스케일' })).toBeTruthy()
    expect(screen.getByText('개발자 진단 · Runner Lab')).toBeTruthy()
    expect(screen.getByRole('heading', { name: '실행 경로 검증' })).toBeTruthy()
    expect(screen.getByText('Tauri v2 · Rust · Svelte 5')).toBeTruthy()
  })
})
