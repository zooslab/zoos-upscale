<script lang="ts">
  import { onMount } from 'svelte'
  import {
    cancelJob,
    commandError,
    getImageEngineStatus,
    isDesktopRuntime,
    listJobs,
    pickAndCreateImageJob,
    startJob,
  } from '../lib/api/jobs'
  import {
    activeStatuses,
    type ImageEngineStatus,
    type ImagePreset,
    type ImageScale,
    type JobErrorView,
    type JobStatus,
    type JobSummary,
  } from '../lib/types/jobs'

  const statusLabels: Record<JobStatus, string> = {
    CREATED: '시작 준비',
    PROBING: '엔진 확인 중',
    PLANNING: '출력 준비 중',
    RUNNING: '업스케일 중',
    VERIFYING: '결과 검증 중',
    COMPLETED: '완료',
    FAILED: '실패',
    CANCELLED: '취소됨',
    INTERRUPTED: '중단됨',
  }

  const runtimeAvailable = isDesktopRuntime()
  let preset = $state<ImagePreset>('photo')
  let scale = $state<ImageScale>(2)
  let engine = $state<ImageEngineStatus | null>(null)
  let jobs = $state<JobSummary[]>([])
  let requestPending = $state(false)
  let requestError = $state<JobErrorView | null>(null)
  let cancellingJobId = $state<string | null>(null)
  let pollTimer: number | null = null

  let imageJobs = $derived(jobs.filter((job) => job.kind === 'image_upscale'))
  let currentJob = $derived(
    imageJobs.find((job) => activeStatuses.has(job.status)) ?? imageJobs[0] ?? null,
  )
  let isActive = $derived(Boolean(currentJob && activeStatuses.has(currentJob.status)))
  let isCancelling = $derived(currentJob?.job_id === cancellingJobId)
  let shownError = $derived(requestError ?? currentJob?.error ?? null)
  let canSelect = $derived(
    runtimeAvailable && engine?.state === 'READY' && !requestPending && !isActive,
  )

  onMount(() => {
    if (!runtimeAvailable) return
    void initialize()
    return stopPolling
  })

  async function initialize(): Promise<void> {
    try {
      ;[engine, jobs] = await Promise.all([getImageEngineStatus(), listJobs()])
      schedulePollIfActive()
    } catch (error) {
      requestError = commandError(error)
    }
  }

  function stopPolling(): void {
    if (pollTimer !== null) {
      window.clearTimeout(pollTimer)
      pollTimer = null
    }
  }

  function schedulePollIfActive(): void {
    stopPolling()
    if (!runtimeAvailable || !jobs.some((job) => activeStatuses.has(job.status))) return
    pollTimer = window.setTimeout(() => void refreshJobs(), 500)
  }

  async function refreshJobs(): Promise<void> {
    try {
      jobs = await listJobs()
      clearFinishedCancellation()
    } catch (error) {
      requestError = commandError(error)
    } finally {
      schedulePollIfActive()
    }
  }

  async function selectImage(): Promise<void> {
    requestPending = true
    requestError = null
    let created: JobSummary | null = null
    try {
      created = await pickAndCreateImageJob(preset, scale)
      if (created === null) return
      jobs = [created, ...jobs.filter((job) => job.job_id !== created?.job_id)]
      const started = await startJob(created.job_id)
      jobs = [started, ...jobs.filter((job) => job.job_id !== started.job_id)]
      schedulePollIfActive()
    } catch (error) {
      requestError = commandError(error)
      await resynchronizeAfterStartError(created !== null)
    } finally {
      requestPending = false
    }
  }

  async function cancelCurrentJob(): Promise<void> {
    if (
      !currentJob ||
      !activeStatuses.has(currentJob.status) ||
      cancellingJobId === currentJob.job_id
    ) return
    const jobId = currentJob.job_id
    cancellingJobId = jobId
    requestPending = true
    requestError = null
    try {
      const cancelled = await cancelJob(jobId)
      jobs = [cancelled, ...jobs.filter((job) => job.job_id !== cancelled.job_id)]
      clearFinishedCancellation()
      schedulePollIfActive()
    } catch (error) {
      requestError = commandError(error)
      try {
        jobs = await listJobs()
        clearFinishedCancellation()
      } catch {
        // Keep the original cancellation error visible and continue polling the known active job.
      }
    } finally {
      requestPending = false
    }
  }

  function clearFinishedCancellation(): void {
    if (cancellingJobId === null) return
    const cancellingJob = jobs.find((job) => job.job_id === cancellingJobId)
    if (!cancellingJob || !activeStatuses.has(cancellingJob.status)) cancellingJobId = null
  }

  async function resynchronizeAfterStartError(jobWasCreated: boolean): Promise<void> {
    if (jobWasCreated) {
      try {
        jobs = await listJobs()
      } catch {
        // Preserve the command error that caused the failed start.
      }
    }
    try {
      engine = await getImageEngineStatus()
    } catch {
      // Preserve the command error; the last known engine status remains useful context.
    }
  }
</script>

<section class="upscale-card" aria-labelledby="upscale-title">
  <div class="engine-row">
    <div>
      <span class="status-label">IMAGE ENGINE</span>
      <h2 id="upscale-title">이미지 업스케일</h2>
    </div>
    <span
      class:ready={engine?.state === 'READY'}
      class:unavailable={engine?.state !== 'READY'}
      class="engine-badge"
    >
      {engine?.state === 'READY' ? '엔진 준비됨' : engine ? '엔진 사용 불가' : '확인 중'}
    </span>
  </div>

  {#if !runtimeAvailable}
    <p class="engine-notice">데스크톱 앱에서 이미지를 선택할 수 있습니다.</p>
  {:else if engine && engine.state !== 'READY'}
    <div class="engine-notice" role="status">
      <strong>{engine.code ?? 'ENGINE_NOT_INSTALLED'}</strong>
      <span>{engine.message}</span>
    </div>
  {:else}
    <fieldset class="choice-group" disabled={requestPending || isActive}>
      <legend>이미지 종류</legend>
      <div class="segmented">
        <button class:selected={preset === 'photo'} type="button" onclick={() => (preset = 'photo')}>
          사진
        </button>
        <button class:selected={preset === 'anime'} type="button" onclick={() => (preset = 'anime')}>
          애니
        </button>
      </div>
    </fieldset>

    <fieldset class="choice-group" disabled={requestPending || isActive}>
      <legend>확대 배율</legend>
      <div class="segmented">
        <button class:selected={scale === 2} type="button" onclick={() => (scale = 2)}>2배</button>
        <button class:selected={scale === 4} type="button" onclick={() => (scale = 4)}>4배</button>
      </div>
    </fieldset>

    <button class="select-image" type="button" onclick={selectImage} disabled={!canSelect}>
      {requestPending ? '처리 중…' : '이미지 선택'}
    </button>
  {/if}

  {#if currentJob}
    <article class="current-job" aria-live="polite">
      <div class="job-heading">
        <div>
          <strong>{currentJob.input_name ?? '선택한 이미지'}</strong>
          <span>{currentJob.stage ?? statusLabels[currentJob.status]}</span>
        </div>
        <b>{statusLabels[currentJob.status]}</b>
      </div>
      <div
        class="progress-track"
        role="progressbar"
        aria-label="업스케일 진행률"
        aria-valuenow={currentJob.progress_percent}
        aria-valuemin="0"
        aria-valuemax="100"
      >
        <span style={`width: ${currentJob.progress_percent}%`}></span>
      </div>
      {#if isActive}
        <button
          class="cancel-action"
          type="button"
          onclick={cancelCurrentJob}
          disabled={requestPending || isCancelling}
        >
          {isCancelling ? '취소 중…' : '작업 취소'}
        </button>
      {:else if currentJob.status === 'COMPLETED' && currentJob.output_path}
        <div class="output-path">
          <span>저장 완료</span>
          <code>{currentJob.output_path}</code>
        </div>
      {/if}
    </article>
  {/if}

  {#if shownError}
    <div class="structured-error" role="alert">
      <strong>{shownError.code}</strong>
      <span>{shownError.message}</span>
    </div>
  {/if}
</section>
