import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { validateDistribution } from './lib/distribution-policy.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readJson = (path) => JSON.parse(readFileSync(join(repositoryRoot, path), 'utf8'))

const tauri = readJson('src-tauri/tauri.conf.json')
const sidecars = readJson('sidecars/manifest.json')
const realEsrgan = readJson('assets/catalog/realesrgan-ncnn-vulkan-macos.json')
validateDistribution(tauri, sidecars, [realEsrgan])

console.log('Public bundle contains no unapproved sidecars, engines, or models')
