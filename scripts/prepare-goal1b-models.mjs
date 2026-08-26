import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const weightCatalog = JSON.parse(await readFile(join(repositoryRoot, 'assets', 'catalog', 'realesrgan-pytorch-weights.json'), 'utf8'))
const weightsDirectory = join(repositoryRoot, '.cache', 'model-assets', weightCatalog.id, weightCatalog.version, 'weights')
const outputRoot = join(repositoryRoot, '.cache', 'model-assets', 'realesrgan-onnx', 'goal1b-v1')

const argumentsList = [
  'run', '--project', join(repositoryRoot, 'tools', 'model'), '--extra', 'export', '--locked',
  'python', join(repositoryRoot, 'tools', 'model', 'export_realesrgan.py'),
  '--weights-dir', weightsDirectory,
  '--output-dir', join(outputRoot, 'models'),
  '--evidence', join(outputRoot, 'export-evidence.json'),
  '--catalog', join(repositoryRoot, 'assets', 'catalog', 'realesrgan-onnx-models.json'),
]

console.log('Exporting Goal 1B models from the verified development weight cache.')
const child = spawn('uv', argumentsList, { cwd: repositoryRoot, stdio: 'inherit', shell: false })
child.on('error', (error) => {
  console.error(`Failed to start uv: ${error.message}`)
  process.exitCode = 1
})
child.on('exit', (code, signal) => {
  if (signal) {
    console.error(`Model export terminated by ${signal}`)
    process.exitCode = 1
  } else {
    process.exitCode = code ?? 1
  }
})
