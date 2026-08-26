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
const catalogPath = join(
  repositoryRoot,
  'assets',
  'catalog',
  'realesrgan-ncnn-vulkan-macos.json',
)

export async function validateBundleContents(bundleRoot, catalog) {
  const forbiddenName = /(?:zoos-runner-(?:fake|realesrgan)|realesrgan|\.param$|\.bin$|\.zip$)/i
  const forbiddenHashes = new Set([
    catalog.source.sha256,
    ...catalog.files.map((file) => file.sha256),
  ])
  const forbiddenSizes = new Set([
    catalog.source.archive_size,
    ...catalog.files.map((file) => file.size),
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
    if (info.isSymbolicLink()) continue
    if (info.isDirectory()) files.push(...(await listBundleFiles(path)))
    else if (info.isFile()) files.push(path)
  }
  return files
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const catalog = JSON.parse(await readFile(catalogPath, 'utf8'))
  const inspected = await validateBundleContents(defaultBundleRoot, catalog)
  console.log(`Verified ${inspected} production bundle files; no engine or model assets found`)
}
