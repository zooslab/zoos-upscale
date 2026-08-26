import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { buildFfmpeg } from './lib/goal2-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readJson = async (path) => JSON.parse(await readFile(join(repositoryRoot, path), 'utf8'))
const catalog = await readJson('assets/catalog/ffmpeg-macos-arm64.json')
const evidence = await readJson('assets/evidence/goal2-ffmpeg-build.json')

console.log('Building verified FFmpeg offline. The output remains development-cache-only.')
const runtime = await buildFfmpeg(
  catalog,
  evidence,
  join(repositoryRoot, '.cache/source-assets'),
  join(repositoryRoot, '.cache/runtime-assets'),
)
console.log(`Verified FFmpeg and ffprobe at ${runtime}`)
