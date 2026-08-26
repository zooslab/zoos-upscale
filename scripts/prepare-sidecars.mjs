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

const cargoArguments = ['build', '--locked', '-p', 'zoos-runner-fake']
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
const buildBinaryName = `zoos-runner-fake-bin${executableSuffix}`
const source = join(repositoryRoot, 'target', profile, buildBinaryName)
const binariesDirectory = join(repositoryRoot, 'src-tauri', 'binaries')
const destination = join(
  binariesDirectory,
  `zoos-runner-fake-${targetTriple}${executableSuffix}`,
)

mkdirSync(binariesDirectory, { recursive: true })
copyFileSync(source, destination)
if (!executableSuffix) {
  chmodSync(destination, 0o755)
}

console.log(`Prepared ${relative(repositoryRoot, destination)}`)
