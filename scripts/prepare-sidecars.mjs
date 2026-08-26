import { chmodSync, copyFileSync, mkdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { join, relative } from 'node:path'
import { spawnSync } from 'node:child_process'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const release = process.argv.includes('--release')
const profile = release ? 'release' : 'debug'

const hostResult = spawnSync('rustc', ['--print', 'host-tuple'], {
  cwd: repositoryRoot,
  encoding: 'utf8',
})
if (hostResult.status !== 0) {
  throw new Error(hostResult.stderr || 'Could not determine the Rust host target')
}

const targetTriple = hostResult.stdout.trim()
if (!/^[a-zA-Z0-9_.-]+$/.test(targetTriple)) {
  throw new Error(`Rust returned an invalid host target: ${targetTriple}`)
}

const sidecars = [
  ['zoos-runner-fake', 'zoos-runner-fake-bin'],
  ['zoos-runner-realesrgan', 'zoos-runner-realesrgan-bin'],
  ['zoos-runner-ort', 'zoos-runner-ort-bin'],
]
const cargoArguments = ['build', '--locked']
for (const [packageName] of sidecars) cargoArguments.push('-p', packageName)
if (release) {
  cargoArguments.push('--release')
}
const buildResult = spawnSync('cargo', cargoArguments, {
  cwd: repositoryRoot,
  stdio: 'inherit',
})
if (buildResult.status !== 0) {
  process.exit(buildResult.status ?? 1)
}

const executableSuffix = targetTriple.includes('windows') ? '.exe' : ''
const binariesDirectory = join(repositoryRoot, 'src-tauri', 'binaries')
mkdirSync(binariesDirectory, { recursive: true })
for (const [packageName, binaryName] of sidecars) {
  const source = join(repositoryRoot, 'target', profile, `${binaryName}${executableSuffix}`)
  const destination = join(
    binariesDirectory,
    `${packageName}-${targetTriple}${executableSuffix}`,
  )
  copyFileSync(source, destination)
  if (!executableSuffix) chmodSync(destination, 0o755)
  console.log(`Prepared ${relative(repositoryRoot, destination)}`)
}
