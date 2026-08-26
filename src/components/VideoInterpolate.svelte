<script lang="ts">
  import { onMount } from 'svelte'
  import {
    cancelJob,
    commandError,
    getVideoEngineStatus,
    isDesktopRuntime,
    listJobs,
    pickAndCreateVideoJob,
  } from '../lib/api/jobs'
  import {
    activeStatuses,
    type ImageBackendStatus,
    type JobErrorView,
    type JobStatus,
    type JobSummary,
    type RationalRate,
    type VideoBackend,
    type VideoEngineStatus,
  } from '../lib/types/jobs'

  const labels: Record<JobStatus, string> = {
    CREATED: '시작 준비 중',
    PROBING: '영상 확인 중',
    PLANNING: '처리 계획 중',
    RUNNING: '프레임 보간 중',
    VERIFYING: '영상 검증 중',
    COMPLETED: '완료',
    FAILED: '실패',
    CANCELLED: '취소됨',
    INTERRUPTED: '중단됨',
  }
  const runtimeAvailable = isDesktopRuntime()
  let backend = $state<VideoBackend>('auto')
  let engine = $state<VideoEngineStatus | null>(null)
  let jobs = $state<JobSummary[]>([])
  let requestPending = $state(false)
  let cancelling = $state(false)
  let requestError = $state<JobErrorView | null>(null)
  let pollTimer: number | null = null
  let refreshInFlight = false

  let videoJobs = $derived(jobs.filter((job) => job.kind === 'video_interpolate'))
  let currentJob = $derived(
    videoJobs.find((job) => activeStatuses.has(job.status) || job.status === 'CREATED') ??
      videoJobs[0] ??
      null,
  )
  let isActive = $derived(
    Boolean(currentJob && (activeStatuses.has(currentJob.status) || currentJob.status === 'CREATED')),
  )
  let mediaReady = $derived(ready(engine?.media))
  let anyBackendReady = $derived(Boolean(engine && (ready(engine.gpu) || ready(engine.cpu))))
  let selectedReady = $derived(
    mediaReady &&
      (backend === 'auto'
        ? anyBackendReady
        : backend === 'vulkan_gpu'
          ? ready(engine?.gpu)
          : ready(engine?.cpu)),
  )
  let shownError = $derived(requestError ?? currentJob?.error ?? null)

  onMount(() => {
    if (!runtimeAvailable) return
    void initialize()
    return stopPolling
  })

  function ready(status: ImageBackendStatus | undefined): boolean {
    return status?.state === 'READY'
  }

  async function initialize() {
    try {
      ;[engine, jobs] = await Promise.all([getVideoEngineStatus(), listJobs()])
      chooseAvailableBackend()
      schedulePoll()
    } catch (error) {
      requestError = commandError(error)
    }
  }

  function chooseAvailableBackend() {
    if (!engine) return
    if (backend === 'vulkan_gpu' && !ready(engine.gpu)) {
      backend = ready(engine.cpu) ? 'ncnn_cpu' : 'auto'
    }
    if (backend === 'ncnn_cpu' && !ready(engine.cpu)) {
      backend = ready(engine.gpu) ? 'vulkan_gpu' : 'auto'
    }
  }

  function stopPolling() {
    if (pollTimer !== null) window.clearTimeout(pollTimer)
    pollTimer = null
  }

  function schedulePoll() {
    stopPolling()
    if (runtimeAvailable && isActive) {
      pollTimer = window.setTimeout(() => void refreshJobs(), 500)
    }
  }

  async function refreshJobs() {
    if (refreshInFlight) return
    refreshInFlight = true
    try {
      jobs = await listJobs()
    } catch (error) {
      requestError = commandError(error)
    } finally {
      refreshInFlight = false
      schedulePoll()
    }
  }

  function merge(updated: JobSummary) {
    jobs = [updated, ...jobs.filter((job) => job.job_id !== updated.job_id)]
  }

  async function selectVideo() {
    requestPending = true
    requestError = null
    try {
      // The Rust picker creates and starts the job atomically. Do not call start_job here.
      const created = await pickAndCreateVideoJob(backend)
      if (!created) return
      merge(created)
      schedulePoll()
    } catch (error) {
      requestError = commandError(error)
      try {
        engine = await getVideoEngineStatus()
        chooseAvailableBackend()
      } catch {
        // Keep the original command error visible.
      }
    } finally {
      requestPending = false
    }
  }

  async function cancelCurrentJob() {
    if (!currentJob || !isActive || cancelling) return
    cancelling = true
    requestError = null
    try {
      merge(await cancelJob(currentJob.job_id))
    } catch (error) {
      requestError = commandError(error)
      try {
        jobs = await listJobs()
      } catch {
        // Keep the cancellation error visible.
      }
    } finally {
      cancelling = false
      schedulePoll()
    }
  }

  function rateText(rate: RationalRate | undefined): string {
    if (!rate) return '—'
    const value = rate.numerator / rate.denominator
    return Number.isInteger(value) ? String(value) : value.toFixed(2)
  }
</script>

<section class="upscale-card video-card" aria-labelledby="video-title">
  <div class="engine-row">
    <div>
      <span class="status-label">VIDEO INTERPOLATION</span>
      <h2 id="video-title">영상 프레임 보간</h2>
    </div>
    <span
      class:ready={mediaReady && anyBackendReady}
      class:unavailable={engine && (!mediaReady || !anyBackendReady)}
      class="engine-badge"
    >
      {engine ? (mediaReady && anyBackendReady ? '엔진 준비됨' : '엔진 사용 불가') : '확인 중'}
    </span>
  </div>

  {#if !runtimeAvailable}
    <p class="engine-notice">데스크톱 앱에서 영상을 선택할 수 있습니다.</p>
  {:else}
    {#if engine}
      <div class="backend-status video-engine-status" aria-label="영상 실행 엔진 상태">
        <span class:ready={ready(engine.media)}>미디어 · {ready(engine.media) ? '준비됨' : '사용 불가'}</span>
        <span class:ready={ready(engine.gpu)}>GPU · {ready(engine.gpu) ? '준비됨' : '사용 불가'}</span>
        <span class:ready={ready(engine.cpu)}>CPU · {ready(engine.cpu) ? '준비됨' : '사용 불가'}</span>
      </div>
      {#if !mediaReady || !anyBackendReady}
        <div class="engine-notice" role="status">
          <strong>{engine.media.code ?? engine.gpu.code ?? engine.cpu.code ?? 'ENGINE_NOT_INSTALLED'}</strong>
          <span>{engine.media.message || engine.gpu.message || engine.cpu.message}</span>
        </div>
      {/if}
    {/if}

    <fieldset class="choice-group" disabled={requestPending || isActive}>
      <legend>실행 방식</legend>
      <div class="segmented three">
        <button
          class:selected={backend === 'auto'}
          type="button"
          onclick={() => (backend = 'auto')}
          disabled={!mediaReady || !anyBackendReady}
        >자동 {engine?.recommended_backend === 'vulkan_gpu' ? '· GPU 권장' : engine?.recommended_backend === 'ncnn_cpu' ? '· CPU 권장' : ''}</button>
        <button
          class:selected={backend === 'vulkan_gpu'}
          type="button"
          onclick={() => (backend = 'vulkan_gpu')}
          disabled={!mediaReady || !ready(engine?.gpu)}
          title={engine?.gpu.message}
        >GPU</button>
        <button
          class:selected={backend === 'ncnn_cpu'}
          type="button"
          onclick={() => (backend = 'ncnn_cpu')}
          disabled={!mediaReady || !ready(engine?.cpu)}
          title={engine?.cpu.message}
        >CPU</button>
      </div>
      <p class="field-help">자동은 준비된 장치 중 권장 경로를 사용합니다.</p>
    </fieldset>

    <button
      class="select-image select-video"
      type="button"
      onclick={selectVideo}
      disabled={!selectedReady || requestPending || isActive}
    >{requestPending ? '영상 확인 중…' : 'MP4 · MOV · MKV 선택'}</button>
  {/if}

  {#if currentJob}
    <article class="current-job video-job" aria-live="polite">
      <div class="job-heading">
        <div>
          <strong>{currentJob.input_name ?? '선택한 영상'}</strong>
          <span>{currentJob.stage ?? labels[currentJob.status]}</span>
        </div>
        <b>{labels[currentJob.status]}</b>
      </div>
      <div class="video-rate" aria-label="프레임 속도">
        <span>{rateText(currentJob.source_rate)}</span>
        <i aria-hidden="true">→</i>
        <strong>{rateText(currentJob.target_rate)} fps</strong>
        {#if currentJob.video_container}<small>{currentJob.video_container.toUpperCase()}</small>{/if}
      </div>
      <div
        class="progress-track"
        role="progressbar"
        aria-label="영상 보간 진행률"
        aria-valuenow={currentJob.progress_percent}
        aria-valuemin="0"
        aria-valuemax="100"
      ><span style={`width: ${currentJob.progress_percent}%`}></span></div>
      <p class="progress-caption">{currentJob.progress_percent}% · {currentJob.message}</p>
      {#if isActive}
        <button class="cancel-action" type="button" onclick={cancelCurrentJob} disabled={cancelling}>
          {cancelling ? '취소 중…' : '작업 취소'}
        </button>
      {:else if currentJob.status === 'COMPLETED' && currentJob.output_path}
        <div class="output-path"><span>저장 완료</span><code>{currentJob.output_path}</code></div>
      {/if}
    </article>
  {:else}
    <div class="video-empty">
      <strong>정확히 2배 더 부드럽게</strong>
      <span>지원 프레임 속도: 25 · 29.97 · 30 fps</span>
    </div>
  {/if}

  {#if shownError}
    <div class="structured-error" role="alert">
      <strong>{shownError.code}</strong><span>{shownError.message}</span>
    </div>
  {/if}
</section>
