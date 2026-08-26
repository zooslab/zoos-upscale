<script lang="ts">
  import { onMount } from 'svelte'
  import {
    cancelJob,
    commandErrorMessage,
    createFakeJob,
    isDesktopRuntime,
    listJobs,
    startJob,
  } from '../lib/api/jobs'
  import { activeStatuses, type FakeScenario, type JobStatus, type JobSummary } from '../lib/types/jobs'

  const scenarios: ReadonlyArray<{
    value: FakeScenario
    label: string
    detail: string
  }> = [
    { value: 'success', label: '정상 완료', detail: '진행과 결과 검증' },
    { value: 'failed', label: '실패 처리', detail: '구조화된 오류 표시' },
    { value: 'hang', label: '응답 없음', detail: '취소·timeout 확인' },
  ]

  const statusLabels: Record<JobStatus, string> = {
    CREATED: '대기',
    PROBING: '확인 중',
    PLANNING: '계획 중',
    RUNNING: '실행 중',
    VERIFYING: '검증 중',
    COMPLETED: '완료',
    FAILED: '실패',
    CANCELLED: '취소됨',
    INTERRUPTED: '중단됨',
  }

  const runtimeAvailable = isDesktopRuntime()
  let selectedScenario = $state<FakeScenario>('success')
  let jobs = $state<JobSummary[]>([])
  let requestPending = $state(false)
  let errorMessage = $state<string | null>(null)
  let pollTimer: number | null = null
  let fakeJobs = $derived(jobs.filter((job) => job.kind === 'fake_validation'))
  let activeJob = $derived(fakeJobs.find((job) => activeStatuses.has(job.status)) ?? null)

  onMount(() => {
    if (!runtimeAvailable) return

    void refreshJobs()
    return stopPolling
  })

  function stopPolling(): void {
    if (pollTimer !== null) {
      window.clearTimeout(pollTimer)
      pollTimer = null
    }
  }

  function schedulePollIfActive(): void {
    stopPolling()
    if (!jobs.some((job) => activeStatuses.has(job.status))) return
    pollTimer = window.setTimeout(() => void refreshJobs(), 500)
  }

  async function refreshJobs(): Promise<void> {
    try {
      jobs = await listJobs()
    } catch (error) {
      errorMessage = commandErrorMessage(error)
    } finally {
      schedulePollIfActive()
    }
  }

  async function runScenario(): Promise<void> {
    requestPending = true
    errorMessage = null
    try {
      const created = await createFakeJob(selectedScenario)
      const started = await startJob(created.job_id)
      jobs = [started, ...jobs.filter((job) => job.job_id !== started.job_id)]
      schedulePollIfActive()
    } catch (error) {
      errorMessage = commandErrorMessage(error)
    } finally {
      requestPending = false
    }
  }

  async function cancelActiveJob(): Promise<void> {
    if (!activeJob) return
    requestPending = true
    errorMessage = null
    try {
      await cancelJob(activeJob.job_id)
      await refreshJobs()
    } catch (error) {
      errorMessage = commandErrorMessage(error)
    } finally {
      requestPending = false
    }
  }

  function scenarioLabel(scenario?: FakeScenario): string {
    if (!scenario) return '이전 Fake 작업'
    return scenarios.find((item) => item.value === scenario)?.label ?? scenario
  }
</script>

<section class="status-card" aria-labelledby="lab-title">
  <div class="status-card__header">
    <span class="window-controls" aria-hidden="true"><i></i><i></i><i></i></span>
    <span>Goal 0 · Runner Lab</span>
  </div>

  <div class="status-card__body lab-body">
    <div class="lab-heading">
      <div>
        <span class="status-label">NATIVE SIDECAR</span>
        <h2 id="lab-title">실행 경로 검증</h2>
      </div>
      <span class:offline={!runtimeAvailable} class="runtime-state">
        {runtimeAvailable ? '연결됨' : '미리보기'}
      </span>
    </div>

    <div class="scenario-grid" aria-label="검증 시나리오">
      {#each scenarios as scenario}
        <button
          class:selected={selectedScenario === scenario.value}
          type="button"
          onclick={() => (selectedScenario = scenario.value)}
          disabled={requestPending || Boolean(activeJob) || !runtimeAvailable}
        >
          <strong>{scenario.label}</strong>
          <span>{scenario.detail}</span>
        </button>
      {/each}
    </div>

    <div class="lab-actions">
      <button
        class="primary-action"
        type="button"
        onclick={runScenario}
        disabled={requestPending || Boolean(activeJob) || !runtimeAvailable}
      >
        {requestPending ? '처리 중…' : '검증 시작'}
      </button>
      {#if activeJob}
        <button
          class="cancel-action"
          type="button"
          onclick={cancelActiveJob}
          disabled={requestPending}
        >
          취소
        </button>
      {/if}
    </div>

    {#if errorMessage}
      <p class="command-error" role="alert">{errorMessage}</p>
    {/if}

    <div class="job-list" aria-live="polite">
      {#if fakeJobs.length === 0}
        <div class="empty-job">
          <span aria-hidden="true">↗</span>
          <p>시나리오를 선택해 Rust 작업 경로를 확인하세요.</p>
        </div>
      {:else}
        {#each fakeJobs.slice(0, 3) as job (job.job_id)}
          <article class:failed={job.status === 'FAILED'} class:completed={job.status === 'COMPLETED'}>
            <div class="job-row">
              <div>
                <strong>{scenarioLabel(job.scenario)}</strong>
                <span>{job.error?.message ?? job.message}</span>
              </div>
              <span class="job-status">{statusLabels[job.status]}</span>
            </div>
            <div
              class="progress-track"
              role="progressbar"
              aria-label={`${scenarioLabel(job.scenario)} 진행률`}
              aria-valuenow={job.progress_percent}
              aria-valuemin="0"
              aria-valuemax="100"
            >
              <span style={`width: ${job.progress_percent}%`}></span>
            </div>
          </article>
        {/each}
      {/if}
    </div>
  </div>
</section>
