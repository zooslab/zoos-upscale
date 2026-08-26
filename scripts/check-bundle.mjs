import { createHash } from 'node:crypto'
import { lstat, readFile, readdir } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join, relative, sep } from 'node:path'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const defaultBundleRoot = join(
  repositoryRoot,
  'target',
  'release',
  'bundle',
  'macos',
  'Zoos Upscale.app',
)
const catalogPaths = [
  'realesrgan-ncnn-vulkan-macos.json',
  'onnxruntime-macos-arm64.json',
  'realesrgan-pytorch-weights.json',
  'realesrgan-onnx-models.json',
  'ffmpeg-macos-arm64.json',
].map((name) => join(repositoryRoot, 'assets', 'catalog', name))

export async function validateBundleContents(bundleRoot, catalogValue) {
  const catalogs = Array.isArray(catalogValue) ? catalogValue : [catalogValue]
  const forbiddenName = /(?:zoos-runner-(?:fake|realesrgan|ort)|realesrgan|onnxruntime|ffmpeg|ffprobe|\.param$|\.bin$|\.onnx$|\.pth$|\.dylib$|\.zip$|\.tar\.gz$)/i
  const catalogFiles = catalogs.flatMap((catalog) => catalog.files ?? [])
  const catalogSources = catalogs
    .map((catalog) => catalog.source)
    .filter((source) => source?.sha256 && source?.archive_size)
  const forbiddenHashes = new Set([
    ...catalogSources.map((source) => source.sha256),
    ...catalogFiles.map((file) => file.sha256),
  ])
  const forbiddenSizes = new Set([
    ...catalogSources.map((source) => source.archive_size),
    ...catalogFiles.map((file) => file.size),
  ])

  let inspectedFiles = 0
  for (const path of await listBundleFiles(bundleRoot)) {
    inspectedFiles += 1
    const displayPath = relative(bundleRoot, path).split(sep).join('/')
    if (forbiddenName.test(displayPath)) {
      throw new Error(`Production bundle contains a forbidden runtime asset: ${displayPath}`)
    }
    const info = await lstat(path)
    if (!forbiddenSizes.has(info.size)) continue
    const hash = createHash('sha256').update(await readFile(path)).digest('hex')
    if (forbiddenHashes.has(hash)) {
      throw new Error(`Production bundle contains a cataloged runtime asset: ${displayPath}`)
    }
  }
  if (inspectedFiles === 0) throw new Error('Production bundle contains no files')
  return inspectedFiles
}

async function listBundleFiles(directory) {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    const info = await lstat(path)
    if (info.isSymbolicLink()) {
      throw new Error(`Production bundle contains a symbolic link: ${path}`)
    }
    if (info.isDirectory()) files.push(...(await listBundleFiles(path)))
    else if (info.isFile()) files.push(path)
  }
  return files
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const catalogs = await Promise.all(
    catalogPaths.map(async (path) => JSON.parse(await readFile(path, 'utf8'))),
  )
  const inspected = await validateBundleContents(defaultBundleRoot, catalogs)
  console.log(`Verified ${inspected} production bundle files; no engine or model assets found`)
}
