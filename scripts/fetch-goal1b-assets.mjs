import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { fetchAndInstallOrt, fetchAndInstallWeights } from './lib/goal1b-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readCatalog = async (name) => JSON.parse(await readFile(join(repositoryRoot, 'assets', 'catalog', name), 'utf8'))

const runtimeRoot = join(repositoryRoot, '.cache', 'runtime-assets')
const modelRoot = join(repositoryRoot, '.cache', 'model-assets')

console.log('Preparing verified Goal 1B development assets. No asset is approved for distribution.')
const runtime = await fetchAndInstallOrt(await readCatalog('onnxruntime-macos-arm64.json'), runtimeRoot)
const weights = await fetchAndInstallWeights(await readCatalog('realesrgan-pytorch-weights.json'), modelRoot)
console.log(`Verified ONNX Runtime at ${runtime}`)
console.log(`Verified Real-ESRGAN source weights at ${weights}`)
