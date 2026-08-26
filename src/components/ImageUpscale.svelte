<script lang="ts">
  import { onMount } from 'svelte'
  import { cancelBatch, cancelJob, commandError, getImageEngineStatus, isDesktopRuntime, listJobs, pickAndCreateImageBatch, pickAndCreateImageJob, startJob } from '../lib/api/jobs'
  import { activeStatuses, type BatchRejectedInput, type ImageBackend, type ImageBackendStatus, type ImageEngineStatus, type ImageOutputFormat, type ImagePreset, type ImageScale, type JobErrorView, type JobStatus, type JobSummary, type MetadataPolicy } from '../lib/types/jobs'

  const labels: Record<JobStatus, string> = { CREATED: '대기', PROBING: '엔진 확인 중', PLANNING: '출력 준비 중', RUNNING: '업스케일 중', VERIFYING: '결과 검증 중', COMPLETED: '완료', FAILED: '실패', CANCELLED: '취소됨', INTERRUPTED: '중단됨' }
  const terminal = new Set<JobStatus>(['COMPLETED', 'FAILED', 'CANCELLED', 'INTERRUPTED'])
  const runtimeAvailable = isDesktopRuntime()
  let preset = $state<ImagePreset>('photo'), scale = $state<ImageScale>(2)
  let backend = $state<ImageBackend>('auto'), outputFormat = $state<ImageOutputFormat>('png'), metadata = $state<MetadataPolicy>('preserve')
  let engine = $state<ImageEngineStatus | null>(null), jobs = $state<JobSummary[]>([])
  let batchId = $state<string | null>(null), batchJobIds = $state<string[]>([]), rejected = $state<BatchRejectedInput[]>([])
  let batchCancelled = $state(false), batchCancelling = $state(false), batchStartErrors = $state<Record<string, JobErrorView>>({})
  let requestPending = $state(false), requestError = $state<JobErrorView | null>(null), cancellingJobId = $state<string | null>(null)
  let pollTimer: number | null = null, refreshInFlight = false, advanceInFlight = false

  let imageJobs = $derived(jobs.filter((job) => job.kind === 'image_upscale'))
  let batchJobs = $derived(batchJobIds.map((id) => imageJobs.find((job) => job.job_id === id)).filter((job): job is JobSummary => Boolean(job)))
  let batchFinished = $derived(batchJobIds.filter((id) => { const job = imageJobs.find((item) => item.job_id === id); return !job || terminal.has(job.status) || Boolean(batchStartErrors[id]) }).length)
  let batchRunning = $derived(Boolean(batchId && !batchCancelled && batchFinished < batchJobIds.length))
  let currentJob = $derived(batchJobs.find((job) => activeStatuses.has(job.status)) ?? batchJobs.find((job) => job.status === 'CREATED' && !batchStartErrors[job.job_id]) ?? imageJobs.find((job) => activeStatuses.has(job.status)) ?? imageJobs[0] ?? null)
  let isActive = $derived(Boolean(currentJob && activeStatuses.has(currentJob.status)))
  let anyReady = $derived(Boolean(engine && (ready(engine.gpu) || ready(engine.cpu))))
  let selectedReady = $derived(backend === 'auto' ? anyReady : backend === 'vulkan_gpu' ? ready(engine?.gpu) : ready(engine?.cpu))
  let controlsDisabled = $derived(requestPending || isActive || batchRunning)
  let shownError = $derived(requestError ?? currentJob?.error ?? null)

  onMount(() => { if (!runtimeAvailable) return; void initialize(); return stopPolling })
  function ready(status: ImageBackendStatus | undefined): boolean { return status?.state === 'READY' }
  async function initialize() { try { ;[engine, jobs] = await Promise.all([getImageEngineStatus(), listJobs()]); chooseBackend(); schedulePoll() } catch (error) { requestError = commandError(error) } }
  function chooseBackend() { if (!engine) return; if (backend === 'vulkan_gpu' && !ready(engine.gpu)) backend = ready(engine.cpu) ? 'ort_cpu' : 'auto'; if (backend === 'ort_cpu' && !ready(engine.cpu)) backend = ready(engine.gpu) ? 'vulkan_gpu' : 'auto' }
  function stopPolling() { if (pollTimer !== null) window.clearTimeout(pollTimer); pollTimer = null }
  function schedulePoll() { stopPolling(); if (runtimeAvailable && (jobs.some((job) => activeStatuses.has(job.status)) || batchRunning)) pollTimer = window.setTimeout(() => void refreshJobs(), 500) }
  async function refreshJobs() { if (refreshInFlight) return; refreshInFlight = true; try { jobs = await listJobs(); clearFinishedCancellation(); await advanceBatch() } catch (error) { requestError = commandError(error) } finally { refreshInFlight = false; schedulePoll() } }
  function merge(updated: JobSummary[]) { const ids = new Set(updated.map((job) => job.job_id)); jobs = [...updated, ...jobs.filter((job) => !ids.has(job.job_id))] }

  async function selectImage() {
    requestPending = true; requestError = null; let created: JobSummary | null = null
    try { created = await pickAndCreateImageJob(preset, scale, backend, outputFormat, metadata); if (!created) return; merge([created]); merge([await startJob(created.job_id)]); schedulePoll() }
    catch (error) {
      requestError = commandError(error)
      if (created) {
        try { merge([await cancelJob(created.job_id)]) } catch { /* resync below keeps the original start error visible */ }
      }
      await resync(Boolean(created))
    }
    finally { requestPending = false }
  }
  async function selectBatch() {
    requestPending = true; requestError = null
    try {
      const created = await pickAndCreateImageBatch(preset, scale, backend, outputFormat, metadata); if (!created) return
      batchId = created.batch_id; batchJobIds = created.jobs.slice().sort((a, b) => (a.batch_index ?? 0) - (b.batch_index ?? 0)).map((job) => job.job_id)
      rejected = created.rejected; batchCancelled = false; batchStartErrors = {}; merge(created.jobs); await advanceBatch(); schedulePoll()
    } catch (error) { requestError = commandError(error); await resync(false) }
    finally { requestPending = false }
  }
  async function advanceBatch() {
    if (!batchId || batchCancelled || advanceInFlight || batchJobs.some((job) => activeStatuses.has(job.status))) return
    const next = batchJobs.find((job) => job.status === 'CREATED' && !batchStartErrors[job.job_id]); if (!next) return
    advanceInFlight = true; let retryWhenIdle = false
    try { merge([await startJob(next.job_id)]); requestError = null }
    catch (error) {
      const failure = commandError(error); requestError = failure
      if (failure.code === 'JOB_BUSY') {
        retryWhenIdle = true
        try { jobs = await listJobs() } catch { /* retry from the known queue on the next poll */ }
      } else {
        batchStartErrors = { ...batchStartErrors, [next.job_id]: failure }
        try { merge([await cancelJob(next.job_id)]) } catch { try { jobs = await listJobs() } catch { /* retain the start error */ } }
      }
    }
    finally { advanceInFlight = false }
    if (retryWhenIdle) return
    if (!batchJobs.some((job) => activeStatuses.has(job.status))) await advanceBatch()
  }
  function batchJob(id: string): JobSummary | undefined { return imageJobs.find((job) => job.job_id === id) }
  async function cancelCurrentBatch() { if (!batchId || batchCancelling) return; batchCancelling = true; requestError = null; try { await cancelBatch(batchId); batchCancelled = true; jobs = await listJobs() } catch (error) { requestError = commandError(error) } finally { batchCancelling = false; schedulePoll() } }
  async function cancelCurrentJob() {
    if (!currentJob || !activeStatuses.has(currentJob.status) || cancellingJobId === currentJob.job_id) return
    cancellingJobId = currentJob.job_id; requestPending = true; requestError = null
    try { merge([await cancelJob(currentJob.job_id)]); clearFinishedCancellation(); schedulePoll() }
    catch (error) { requestError = commandError(error); try { jobs = await listJobs(); clearFinishedCancellation() } catch { /* retain error */ } }
    finally { requestPending = false }
  }
  function clearFinishedCancellation() { if (!cancellingJobId) return; const job = jobs.find((item) => item.job_id === cancellingJobId); if (!job || !activeStatuses.has(job.status)) cancellingJobId = null }
  async function resync(created: boolean) { if (created) try { jobs = await listJobs() } catch { /* retain error */ }; try { engine = await getImageEngineStatus(); chooseBackend() } catch { /* retain error */ } }
</script>

<section class="upscale-card" aria-labelledby="upscale-title">
  <div class="engine-row"><div><span class="status-label">IMAGE ENGINE</span><h2 id="upscale-title">이미지 업스케일</h2></div><span class:ready={anyReady} class:unavailable={!anyReady} class="engine-badge">{engine ? (anyReady ? '엔진 준비됨' : '엔진 사용 불가') : '확인 중'}</span></div>
  {#if !runtimeAvailable}<p class="engine-notice">데스크톱 앱에서 이미지를 선택할 수 있습니다.</p>
  {:else}
    {#if engine}
      <div class="backend-status" aria-label="실행 엔진 상태"><span class:ready={ready(engine.gpu)}>GPU · {ready(engine.gpu) ? '준비됨' : '사용 불가'}</span><span class:ready={ready(engine.cpu)}>CPU · {ready(engine.cpu) ? '준비됨' : '사용 불가'}</span></div>
      {#if !anyReady}<div class="engine-notice" role="status"><strong>{engine.gpu.code ?? engine.cpu.code ?? 'ENGINE_NOT_INSTALLED'}</strong><span>{engine.gpu.message || engine.cpu.message}</span></div>{/if}
    {/if}
    <div class="settings-grid">
      <fieldset class="choice-group" disabled={controlsDisabled}><legend>이미지 종류</legend><div class="segmented"><button class:selected={preset === 'photo'} type="button" onclick={() => (preset = 'photo')}>사진</button><button class:selected={preset === 'anime'} type="button" onclick={() => (preset = 'anime')}>애니</button></div></fieldset>
      <fieldset class="choice-group" disabled={controlsDisabled}><legend>확대 배율</legend><div class="segmented"><button class:selected={scale === 2} type="button" onclick={() => (scale = 2)}>2배</button><button class:selected={scale === 4} type="button" onclick={() => (scale = 4)}>4배</button></div></fieldset>
    </div>
    <fieldset class="choice-group" disabled={controlsDisabled}><legend>실행 방식</legend><div class="segmented three"><button class:selected={backend === 'auto'} type="button" onclick={() => (backend = 'auto')} disabled={!anyReady}>자동 {engine?.recommended_backend === 'vulkan_gpu' ? '· GPU 권장' : engine?.recommended_backend === 'ort_cpu' ? '· CPU 권장' : ''}</button><button class:selected={backend === 'vulkan_gpu'} type="button" onclick={() => (backend = 'vulkan_gpu')} disabled={!ready(engine?.gpu)} title={engine?.gpu.message}>GPU</button><button class:selected={backend === 'ort_cpu'} type="button" onclick={() => (backend = 'ort_cpu')} disabled={!ready(engine?.cpu)} title={engine?.cpu.message}>CPU</button></div><p class="field-help">자동은 이 Mac에서 준비된 가장 빠른 방식을 선택합니다.</p></fieldset>
    <div class="settings-grid">
      <fieldset class="choice-group" disabled={controlsDisabled}><legend>출력 형식</legend><div class="segmented three"><button class:selected={outputFormat === 'png'} type="button" onclick={() => (outputFormat = 'png')}>PNG</button><button class:selected={outputFormat === 'jpeg'} type="button" onclick={() => (outputFormat = 'jpeg')}>JPEG</button><button class:selected={outputFormat === 'webp'} type="button" onclick={() => (outputFormat = 'webp')}>WebP</button></div></fieldset>
      <fieldset class="choice-group" disabled={controlsDisabled}><legend>색상·촬영 정보</legend><div class="segmented"><button class:selected={metadata === 'preserve'} type="button" onclick={() => (metadata = 'preserve')}>유지</button><button class:selected={metadata === 'strip'} type="button" onclick={() => (metadata = 'strip')}>제거</button></div></fieldset>
    </div>
    <div class="picker-actions"><button class="select-image" type="button" onclick={selectImage} disabled={!runtimeAvailable || !selectedReady || controlsDisabled}>{requestPending ? '처리 중…' : '이미지 선택'}</button><button class="select-folder" type="button" onclick={selectBatch} disabled={!runtimeAvailable || !selectedReady || controlsDisabled}>폴더 일괄</button></div>
  {/if}

  {#if batchId}
    <article class="batch-panel" aria-live="polite"><div class="batch-heading"><strong>일괄 작업 {batchFinished}/{batchJobIds.length}</strong><span>{batchRunning ? '한 파일씩 안전하게 처리 중' : batchCancelled ? '일괄 작업 취소됨' : '일괄 작업 완료'}</span></div><div class="batch-list">{#each batchJobIds as jobId}{@const job = batchJob(jobId)}<div class:failed={!job || job.status === 'FAILED' || Boolean(batchStartErrors[jobId])}><span>{job?.input_name ?? '격리된 작업 기록'}</span><b>{!job ? '기록 격리됨' : batchStartErrors[jobId] ? '시작 실패' : labels[job.status]}</b></div>{/each}</div>{#if rejected.length}<p class="rejected-summary">선택하지 못한 파일 {rejected.length}개 · {rejected.map((item) => item.input_name).join(', ')}</p>{/if}{#if batchRunning}<button class="cancel-action" type="button" onclick={cancelCurrentBatch} disabled={batchCancelling}>{batchCancelling ? '전체 취소 중…' : '일괄 작업 취소'}</button>{/if}</article>
  {:else if currentJob}
    <article class="current-job" aria-live="polite"><div class="job-heading"><div><strong>{currentJob.input_name ?? '선택한 이미지'}</strong><span>{currentJob.stage ?? labels[currentJob.status]}</span></div><b>{labels[currentJob.status]}</b></div><div class="progress-track" role="progressbar" aria-label="업스케일 진행률" aria-valuenow={currentJob.progress_percent} aria-valuemin="0" aria-valuemax="100"><span style={`width: ${currentJob.progress_percent}%`}></span></div>{#if isActive}<button class="cancel-action" type="button" onclick={cancelCurrentJob} disabled={requestPending || cancellingJobId === currentJob.job_id}>{cancellingJobId === currentJob.job_id ? '취소 중…' : '작업 취소'}</button>{:else if currentJob.status === 'COMPLETED' && currentJob.output_path}<div class="output-path"><span>저장 완료</span><code>{currentJob.output_path}</code></div>{/if}</article>
  {/if}
  {#if shownError}<div class="structured-error" role="alert"><strong>{shownError.code}</strong><span>{shownError.message}</span></div>{/if}
</section>
