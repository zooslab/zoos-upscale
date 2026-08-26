<script lang="ts">
  import JobLab from './components/JobLab.svelte'
  import ImageUpscale from './components/ImageUpscale.svelte'

  const showGoal0Lab = import.meta.env.DEV
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
    <section class="hero" aria-labelledby="hero-title">
      <div class="hero-copy">
        <span class="eyebrow">LOCAL IMAGE UPSCALER</span>
        <h1 id="hero-title" aria-label="이미지를 더 크게, 선명하게.">
          이미지를 더 크게,<br />선명하게.
        </h1>
        <p>
          사진과 애니 이미지를 Mac의 GPU에서 직접 2배 또는 4배로 키웁니다.
          원본은 그대로 두고 PNG 결과를 옆 폴더에 안전하게 저장합니다.
        </p>
      </div>
      <ImageUpscale />
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
