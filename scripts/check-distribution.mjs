import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { validateDistribution } from './lib/distribution-policy.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readJson = (path) => JSON.parse(readFileSync(join(repositoryRoot, path), 'utf8'))

const tauri = readJson('src-tauri/tauri.conf.json')
const sidecars = readJson('sidecars/manifest.json')
validateDistribution(tauri, sidecars)

console.log('Public bundle contains no unapproved sidecars, engines, or models')
