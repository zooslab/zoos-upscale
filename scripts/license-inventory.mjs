import { spawnSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const inventoryPath = join(repositoryRoot, 'licenses', 'javascript-production.json')
const checkOnly = process.argv.includes('--check')
const approvedLicenses = new Set(['Apache-2.0', 'Apache-2.0 OR MIT', 'MIT'])

const result = spawnSync('pnpm', ['licenses', 'list', '--json', '--prod'], {
  cwd: repositoryRoot,
  encoding: 'utf8',
  maxBuffer: 16 * 1024 * 1024,
})
if (result.status !== 0) {
  process.stderr.write(result.stderr)
  process.exit(result.status ?? 1)
}

const report = JSON.parse(result.stdout)
const dependencies = []
for (const [license, packages] of Object.entries(report)) {
  if (!approvedLicenses.has(license)) {
    throw new Error(`Unapproved production JavaScript license: ${license}`)
  }
  for (const dependency of packages) {
    for (const version of dependency.versions) {
      dependencies.push({
        name: dependency.name,
        version,
        license,
        homepage: dependency.homepage ?? null,
      })
    }
  }
}

dependencies.sort((left, right) =>
  left.name.localeCompare(right.name) || left.version.localeCompare(right.version),
)
const contents = `${JSON.stringify({ schema_version: 1, dependencies }, null, 2)}\n`

if (checkOnly) {
  const existing = readFileSync(inventoryPath, 'utf8')
  if (existing !== contents) {
    throw new Error(
      'JavaScript license inventory is stale. Run `pnpm license:inventory` and commit the result.',
    )
  }
  console.log('JavaScript license inventory is current')
} else {
  writeFileSync(inventoryPath, contents)
  console.log('Updated licenses/javascript-production.json')
}

