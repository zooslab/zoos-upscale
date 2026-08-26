const forbiddenBundlePattern = /(?:fake|realesrgan|zoos-runner-ort|onnxruntime|ffmpeg|ffprobe|\.param$|\.bin$|\.onnx$|\.pth$|\.dylib$)/i

export function validateDistribution(tauri, sidecars, catalogs = []) {
  const resourceConfiguration = tauri.bundle?.resources ?? []
  const resources = Array.isArray(resourceConfiguration)
    ? resourceConfiguration
    : Object.keys(resourceConfiguration)
  const configuredAssets = [
    ...(tauri.bundle?.externalBin ?? []),
    ...resources,
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
