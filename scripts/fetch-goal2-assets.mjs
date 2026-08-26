import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { fetchAndInstallRife, fetchFfmpegSource } from './lib/goal2-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readCatalog = async (name) => JSON.parse(await readFile(join(repositoryRoot, 'assets/catalog', name), 'utf8'))
const ffmpegCatalog = await readCatalog('ffmpeg-macos-arm64.json')
const rifeCatalog = await readCatalog('rife-ncnn-vulkan-macos.json')

console.log('Fetching verified Goal 2 development sources. No asset is approved for distribution.')
const source = await fetchFfmpegSource(ffmpegCatalog, join(repositoryRoot, '.cache/source-assets'))
const rife = await fetchAndInstallRife(rifeCatalog, join(repositoryRoot, '.cache/runtime-assets'))
console.log(`Verified FFmpeg source at ${source}`)
console.log(`Verified RIFE engine and rife-v4.6 model at ${rife}`)
