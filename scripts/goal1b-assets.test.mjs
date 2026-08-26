import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { gzipSync } from 'node:zlib'
import { mkdir, mkdtemp, readFile, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  assertSafeArchivePath,
  extractAllowedTarGzip,
  fetchAndInstallOrt,
  fetchAndInstallWeights,
  validateArm64MachO,
  validateOrtCatalog,
  validateWeightCatalog,
} from './lib/goal1b-assets.mjs'

const sha256 = (value) => createHash('sha256').update(value).digest('hex')

test('tracked Goal 1B catalogs pin official artifacts and remain development-only', async () => {
  const ort = JSON.parse(await readFile(new URL('../assets/catalog/onnxruntime-macos-arm64.json', import.meta.url), 'utf8'))
  const weights = JSON.parse(await readFile(new URL('../assets/catalog/realesrgan-pytorch-weights.json', import.meta.url), 'utf8'))
  validateOrtCatalog(ort)
  validateWeightCatalog(weights)
  assert.equal(ort.source.sha256, 'd0706fc34f315d8c88639d0a8c81f2e09e815f282cabed3493c06a054352cf92')
  assert.deepEqual(weights.files.map((file) => file.sha256), [
    '4fa0d38905f75ac06eb49a7951b426670021be3018265fd191d2125df9d682f1',
    'f872d837d3c90ed2e05227bed711af5671a6fd1c9f7d7e91c911a61f155e99da',
  ])
  assert.equal(ort.approved_for_distribution, false)
  assert.equal(weights.approved_for_distribution, false)
})

test('rejects absolute, traversal, Windows, and backslash archive paths', () => {
  for (const path of ['/absolute', '../escape', 'a/../escape', 'C:/escape', 'a\\b']) {
    assert.throws(() => assertSafeArchivePath(path), /Unsafe asset path/)
  }
})

test('extracts only the allowlisted versioned dylib and verifies hash', () => {
  const dylib = arm64MachO()
  const archivePath = './runtime/lib/libonnxruntime.1.29.0.dylib'
  const archive = tarGzip([
    { name: './runtime/', type: '5', contents: Buffer.alloc(0) },
    { name: archivePath, contents: dylib },
    { name: './runtime/lib/unapproved.dylib', contents: Buffer.from('ignored') },
    { name: './runtime/lib/libonnxruntime.1.dylib', type: '2', linkName: 'libonnxruntime.1.29.0.dylib', contents: Buffer.alloc(0) },
  ])
  const files = [{ archive_path: archivePath, destination: 'lib/runtime.dylib', size: dylib.length, sha256: sha256(dylib) }]
  const extracted = extractAllowedTarGzip(archive, files)
  assert.deepEqual([...extracted], [['lib/runtime.dylib', dylib]])
})

test('rejects traversal and an allowlisted symlink before extraction', () => {
  const target = { archive_path: './runtime/lib/runtime.dylib', destination: 'lib/runtime.dylib', size: 1, sha256: sha256(Buffer.from('x')) }
  assert.throws(() => extractAllowedTarGzip(tarGzip([{ name: '../escape', contents: Buffer.from('x') }]), [target]), /Unsafe asset path/)
  assert.throws(
    () => extractAllowedTarGzip(tarGzip([{ name: target.archive_path, type: '2', linkName: '../../escape', contents: Buffer.alloc(0) }]), [target]),
    /Link is forbidden/,
  )
})

test('rejects wrong file hash and non-arm64 Mach-O', () => {
  const dylib = arm64MachO()
  const path = './runtime/lib/runtime.dylib'
  assert.throws(
    () => extractAllowedTarGzip(tarGzip([{ name: path, contents: dylib }]), [{ archive_path: path, destination: 'lib/runtime.dylib', size: dylib.length, sha256: '0'.repeat(64) }]),
    /SHA-256 mismatch/,
  )
  const wrongArchitecture = arm64MachO()
  wrongArchitecture.writeUInt32LE(0x01000007, 4)
  assert.throws(() => validateArm64MachO(wrongArchitecture), /not an arm64/)
})

test('installs verified runtime and reuses it fully offline', async () => {
  const dylib = arm64MachO()
  const archivePath = './runtime/lib/runtime.dylib'
  const archive = tarGzip([{ name: archivePath, contents: dylib }])
  const catalog = ortCatalog(archive, archivePath, dylib)
  const cache = await temporaryDirectory('zoos-ort')
  let calls = 0
  const installed = await fetchAndInstallOrt(catalog, cache, async () => {
    calls += 1
    return new Response(archive, { status: 200 })
  })
  assert.deepEqual(await readFile(join(installed, 'lib', 'libonnxruntime.1.29.0.dylib')), dylib)
  assert.equal(await fetchAndInstallOrt(catalog, cache, async () => { throw new Error('network forbidden') }), installed)
  assert.equal(calls, 1)
})

test('rejects symlinks in an existing cache instead of using the network', async () => {
  const dylib = arm64MachO()
  const archivePath = './runtime/lib/runtime.dylib'
  const archive = tarGzip([{ name: archivePath, contents: dylib }])
  const catalog = ortCatalog(archive, archivePath, dylib)
  const cache = await temporaryDirectory('zoos-ort-symlink')
  const installed = await fetchAndInstallOrt(catalog, cache, async () => new Response(archive, { status: 200 }))
  await writeFile(join(installed, 'unexpected'), 'x')
  await symlink('unexpected', join(installed, 'link'))
  await assert.rejects(fetchAndInstallOrt(catalog, cache, async () => { throw new Error('network forbidden') }), /Symbolic link is forbidden/)
})

test('rejects a symlink used as the cache destination root', async () => {
  const dylib = arm64MachO()
  const archivePath = './runtime/lib/runtime.dylib'
  const archive = tarGzip([{ name: archivePath, contents: dylib }])
  const catalog = ortCatalog(archive, archivePath, dylib)
  const cache = await temporaryDirectory('zoos-ort-root-symlink')
  const outside = await temporaryDirectory('zoos-ort-outside')
  const parent = join(cache, catalog.id)
  await mkdir(parent, { recursive: true })
  await symlink(outside, join(parent, catalog.version))
  await assert.rejects(fetchAndInstallOrt(catalog, cache, async () => { throw new Error('network forbidden') }), /Symbolic link is forbidden/)
})

test('installs both verified weights and reuses them fully offline', async () => {
  const photo = Buffer.from('photo-weight')
  const anime = Buffer.from('anime-weight')
  const catalog = weightCatalog(photo, anime)
  const cache = await temporaryDirectory('zoos-weights')
  let calls = 0
  const installed = await fetchAndInstallWeights(catalog, cache, async (url) => {
    calls += 1
    return new Response(url.endsWith('photo.pth') ? photo : anime, { status: 200 })
  })
  assert.deepEqual(await readFile(join(installed, 'weights', 'photo.pth')), photo)
  assert.equal(await fetchAndInstallWeights(catalog, cache, async () => { throw new Error('network forbidden') }), installed)
  assert.equal(calls, 2)
})

function ortCatalog(archive, archivePath, dylib) {
  return {
    schema_version: 1,
    id: 'onnxruntime-macos-arm64',
    version: '1.29.0',
    approved_for_distribution: false,
    bundled_in_release: false,
    source: { url: 'https://github.com/microsoft/onnxruntime/releases/download/v1.29.0/test.tgz', archive_size: archive.length, sha256: sha256(archive) },
    files: [{ archive_path: archivePath, destination: 'lib/libonnxruntime.1.29.0.dylib', architecture: 'arm64', size: dylib.length, sha256: sha256(dylib) }],
  }
}

function weightCatalog(photo, anime) {
  return {
    schema_version: 1,
    id: 'realesrgan-pytorch-weights',
    version: 'fixture',
    approved_for_distribution: false,
    bundled_in_release: false,
    source: { repository: 'https://github.com/xinntao/Real-ESRGAN' },
    files: [
      { id: 'photo', release: 'v1', url: 'https://github.com/xinntao/Real-ESRGAN/releases/download/v1/photo.pth', destination: 'weights/photo.pth', size: photo.length, sha256: sha256(photo) },
      { id: 'anime', release: 'v2', url: 'https://github.com/xinntao/Real-ESRGAN/releases/download/v2/anime.pth', destination: 'weights/anime.pth', size: anime.length, sha256: sha256(anime) },
    ],
  }
}

function arm64MachO() {
  const buffer = Buffer.alloc(16)
  buffer.writeUInt32LE(0xfeedfacf, 0)
  buffer.writeUInt32LE(0x0100000c, 4)
  return buffer
}

function tarGzip(entries) {
  const blocks = []
  for (const entry of entries) {
    const contents = Buffer.from(entry.contents)
    const header = Buffer.alloc(512)
    header.write(entry.name, 0, 100, 'utf8')
    header.write('0000644\0', 100, 'ascii')
    header.write(`${contents.length.toString(8).padStart(11, '0')}\0`, 124, 'ascii')
    header[156] = (entry.type ?? '0').charCodeAt(0)
    if (entry.linkName) header.write(entry.linkName, 157, 100, 'utf8')
    blocks.push(header, contents, Buffer.alloc((512 - (contents.length % 512)) % 512))
  }
  blocks.push(Buffer.alloc(1024))
  return gzipSync(Buffer.concat(blocks), { level: 9, mtime: 0 })
}

async function temporaryDirectory(name) {
  return mkdtemp(join(tmpdir(), `${name}-`))
}
