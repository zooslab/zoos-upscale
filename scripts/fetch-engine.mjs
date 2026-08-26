import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { fetchAndInstall } from './lib/runtime-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const catalogPath = join(
  repositoryRoot,
  'assets',
  'catalog',
  'realesrgan-ncnn-vulkan-macos.json',
)
const cacheRoot = join(repositoryRoot, '.cache', 'runtime-assets')
const catalog = JSON.parse(await readFile(catalogPath, 'utf8'))

console.log(`Preparing verified development assets in ${cacheRoot}`)
const installed = await fetchAndInstall(catalog, cacheRoot)
console.log(`Verified Real-ESRGAN development assets at ${installed}`)
