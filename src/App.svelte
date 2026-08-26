<script lang="ts">
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
        <span class="eyebrow">FIRST VERTICAL SLICE</span>
        <h1 id="hero-title" aria-label="선명하게 키우고, 더 부드럽게.">
          선명하게 키우고,<br />더 부드럽게.
        </h1>
        <p>
          사진과 영상을 내 Mac에서 직접 개선하는 가벼운 데스크톱 앱을
          만들고 있습니다.
        </p>
      </div>

      <div class="status-card" aria-label="현재 개발 상태">
        <div class="status-card__header">
          <span class="window-controls" aria-hidden="true">
            <i></i><i></i><i></i>
          </span>
          <span>Goal 0</span>
        </div>
        <div class="status-card__body">
          <div class="status-icon" aria-hidden="true">
            <span>↗</span>
          </div>
          <div>
            <span class="status-label">현재 단계</span>
            <h2>앱 Shell 구성 중</h2>
            <p>Tauri·Rust·Svelte 기반을 먼저 검증합니다.</p>
          </div>
          <div class="progress-track" aria-label="Goal 0 진행 준비 중">
            <span></span>
          </div>
          <div class="next-step">
            <span>다음 검증</span>
            <strong>Fake Runner 성공 · 실패 · 취소</strong>
          </div>
        </div>
      </div>
    </section>

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
