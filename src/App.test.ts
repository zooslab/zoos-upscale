import { render, screen } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import App from './App.svelte'

describe('App', () => {
  it('shows the current Goal 0 runner lab', () => {
    render(App)

    expect(screen.getByRole('heading', { name: '선명하게 키우고, 더 부드럽게.' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: '실행 경로 검증' })).toBeTruthy()
    expect(screen.getByText('Tauri v2 · Rust · Svelte 5')).toBeTruthy()
  })
})
