<script lang="ts">
  import JobLab from './components/JobLab.svelte'
  import ImageUpscale from './components/ImageUpscale.svelte'
  import VideoInterpolate from './components/VideoInterpolate.svelte'

  const showGoal0Lab = import.meta.env.DEV
  let mode = $state<'image' | 'video'>('image')
  const principles = [
    { label: '로컬 처리', detail: '파일을 외부 서버로 보내지 않습니다.' },
    { label: '원본 보호', detail: '결과는 새 파일로만 저장합니다.' },
    { label: '복구 가능', detail: '확인된 지점을 기록해 중단에 대비합니다.' },
  ] as const
</script>

<svelte:head>
  <title>Zoos Upscale</title>
</svelte:head>

<div class="app-shell">
  <header class="topbar">
    <a class="brand" href="/" aria-label="Zoos Upscale 홈">
      <span class="brand-mark" aria-hidden="true">Z</span>
      <span>
        <strong>Zoos Upscale</strong>
        <small>Local AI upscaler</small>
      </span>
    </a>
    <span class="privacy-badge">
      <span class="privacy-dot" aria-hidden="true"></span>
      로컬 전용
    </span>
  </header>

  <main>
    <nav class="mode-switch" aria-label="처리할 미디어 선택">
      <button type="button" class:active={mode === 'image'} aria-pressed={mode === 'image'} onclick={() => (mode = 'image')}>
        <span aria-hidden="true">▧</span><strong>이미지</strong><small>2배 · 4배 확대</small>
      </button>
      <button type="button" class:active={mode === 'video'} aria-pressed={mode === 'video'} onclick={() => (mode = 'video')}>
        <span aria-hidden="true">▷</span><strong>영상</strong><small>프레임 2배 보간</small>
      </button>
    </nav>
    <section class="hero" aria-labelledby="hero-title">
      <div class="hero-copy">
        {#if mode === 'image'}
          <span class="eyebrow">LOCAL IMAGE UPSCALER</span>
          <h1 id="hero-title" aria-label="이미지를 더 크게, 선명하게.">
            이미지를 더 크게,<br />선명하게.
          </h1>
          <p>
            사진과 애니 이미지를 GPU 또는 CPU로 2배·4배 키웁니다.
            한 장이나 폴더를 선택하면 원본은 그대로 두고 결과를 안전하게 저장합니다.
          </p>
        {:else}
          <span class="eyebrow">LOCAL VIDEO INTERPOLATION</span>
          <h1 id="hero-title" aria-label="영상을 더 부드럽게.">
            영상을 더<br />부드럽게.
          </h1>
          <p>
            25 · 29.97 · 30 fps 영상을 정확히 2배로 보간합니다.
            오디오와 지원 자막은 보존하고 원본 옆에 새 영상으로 안전하게 저장합니다.
          </p>
        {/if}
      </div>
      {#if mode === 'image'}<ImageUpscale />{:else}<VideoInterpolate />{/if}
    </section>

    {#if showGoal0Lab}
      <details class="diagnostics">
        <summary>개발자 진단 · Runner Lab</summary>
        <JobLab />
      </details>
    {/if}

    <section class="principles" aria-label="제품 원칙">
      {#each principles as principle, index}
        <article>
          <span class="principle-number">0{index + 1}</span>
          <div>
            <h2>{principle.label}</h2>
            <p>{principle.detail}</p>
          </div>
        </article>
      {/each}
    </section>
  </main>

  <footer>
    <span>Tauri v2 · Rust · Svelte 5</span>
    <span>Python-free runtime</span>
  </footer>
</div>
