import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, mkdir, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { validateBundleContents } from './check-bundle.mjs'

const hash = (value) => createHash('sha256').update(value).digest('hex')

test('accepts an app bundle without cataloged runtime assets', async () => {
  const bundle = await fixtureBundle('safe application')
  const catalog = fixtureCatalog(Buffer.from('engine'))
  assert.equal(await validateBundleContents(bundle, catalog), 1)
})

test('rejects a renamed engine by its catalog hash', async () => {
  const engine = Buffer.from('engine')
  const bundle = await fixtureBundle(engine, 'innocent-name')
  await assert.rejects(
    validateBundleContents(bundle, fixtureCatalog(engine)),
    /cataloged runtime asset/,
  )
})

test('rejects a fake runner by bundle path', async () => {
  const bundle = await fixtureBundle('not the actual binary', 'zoos-runner-fake')
  await assert.rejects(
    validateBundleContents(bundle, fixtureCatalog(Buffer.from('engine'))),
    /forbidden runtime asset/,
  )
})

test('rejects renamed ORT and ONNX assets by catalog hash', async () => {
  const model = Buffer.from('onnx-model')
  const bundle = await fixtureBundle(model, 'innocent-name')
  await assert.rejects(
    validateBundleContents(bundle, [
      fixtureCatalog(Buffer.from('engine')),
      { files: [{ size: model.length, sha256: hash(model) }] },
    ]),
    /cataloged runtime asset/,
  )
})

test('rejects ORT wrapper and dylib paths', async () => {
  for (const name of ['zoos-runner-ort', 'libonnxruntime.1.29.0.dylib']) {
    const bundle = await fixtureBundle('not a real binary', name)
    await assert.rejects(
      validateBundleContents(bundle, fixtureCatalog(Buffer.from('engine'))),
      /forbidden runtime asset/,
    )
  }
})

test('rejects symbolic links instead of skipping bundle entries', async () => {
  const bundle = await fixtureBundle('safe application')
  await symlink('/tmp/not-installed-onnx-model', join(bundle, 'model.onnx'))
  await assert.rejects(
    validateBundleContents(bundle, fixtureCatalog(Buffer.from('engine'))),
    /symbolic link/,
  )
})

function fixtureCatalog(engine) {
  return {
    source: { archive_size: 1, sha256: '0'.repeat(64) },
    files: [{ size: engine.length, sha256: hash(engine) }],
  }
}

async function fixtureBundle(contents, name = 'zoos-upscale') {
  const root = await mkdtemp(join(tmpdir(), 'zoos-bundle-'))
  const executableDirectory = join(root, 'Contents', 'MacOS')
  await mkdir(executableDirectory, { recursive: true })
  await writeFile(join(executableDirectory, name), contents)
  return root
}
