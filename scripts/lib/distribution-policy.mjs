const forbiddenBundlePattern = /(?:fake|realesrgan|\.param$|\.bin$)/i

export function validateDistribution(tauri, sidecars, catalogs = []) {
  const configuredAssets = [
    ...(tauri.bundle?.externalBin ?? []),
    ...Object.keys(tauri.bundle?.resources ?? {}),
  ]

  for (const asset of configuredAssets) {
    if (forbiddenBundlePattern.test(asset)) {
      throw new Error(`Public bundle contains an unapproved runtime asset: ${asset}`)
    }
  }

  for (const sidecar of sidecars.sidecars ?? []) {
    if (sidecar.bundled_in_release && sidecar.approved_for_distribution !== true) {
      throw new Error(
        `Sidecar ${sidecar.id} is bundled without approved_for_distribution=true`,
      )
    }
  }

  for (const catalog of catalogs) {
    if (catalog.bundled_in_release && catalog.approved_for_distribution !== true) {
      throw new Error(
        `Runtime asset ${catalog.id} is bundled without approved_for_distribution=true`,
      )
    }
  }
}

