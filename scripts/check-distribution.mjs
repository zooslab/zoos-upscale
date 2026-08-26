import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

import { validateDistribution } from './lib/distribution-policy.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const readJson = (path) => JSON.parse(readFileSync(join(repositoryRoot, path), 'utf8'))

const tauri = readJson('src-tauri/tauri.conf.json')
const sidecars = readJson('sidecars/manifest.json')
const realEsrgan = readJson('assets/catalog/realesrgan-ncnn-vulkan-macos.json')
const onnxRuntime = readJson('assets/catalog/onnxruntime-macos-arm64.json')
const sourceWeights = readJson('assets/catalog/realesrgan-pytorch-weights.json')
const onnxModels = readJson('assets/catalog/realesrgan-onnx-models.json')
const ffmpeg = readJson('assets/catalog/ffmpeg-macos-arm64.json')
validateDistribution(tauri, sidecars, [realEsrgan, onnxRuntime, sourceWeights, onnxModels, ffmpeg])

console.log('Public bundle contains no unapproved sidecars, engines, or models')
