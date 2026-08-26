import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { fetchFfmpegSource } from './lib/goal2-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const catalog = JSON.parse(await readFile(join(repositoryRoot, 'assets/catalog/ffmpeg-macos-arm64.json'), 'utf8'))

console.log('Fetching verified Goal 2 development sources. No asset is approved for distribution.')
const source = await fetchFfmpegSource(catalog, join(repositoryRoot, '.cache/source-assets'))
console.log(`Verified FFmpeg source at ${source}`)
