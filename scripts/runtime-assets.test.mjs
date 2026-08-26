import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { chmod, mkdtemp, readFile, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  assertSafeRelativePath,
  fetchAndInstall,
  installArchiveBuffer,
  validateCatalog,
  validateUniversalMachO,
} from './lib/runtime-assets.mjs'

const sha256 = (value) => createHash('sha256').update(value).digest('hex')

test('tracked Real-ESRGAN catalog pins the approved development archive and five files', async () => {
  const catalogUrl = new URL(
    '../assets/catalog/realesrgan-ncnn-vulkan-macos.json',
    import.meta.url,
  )
  const catalog = JSON.parse(await readFile(catalogUrl, 'utf8'))
  validateCatalog(catalog)
  assert.equal(
    catalog.source.url,
    'https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/realesrgan-ncnn-vulkan-20220424-macos.zip',
  )
  assert.equal(
    catalog.source.sha256,
    'e0ad05580abfeb25f8d8fb55aaf7bedf552c375b5b4d9bd3c8d59764d2cc333a',
  )
  assert.deepEqual(
    Object.fromEntries(catalog.files.map((file) => [file.archive_path, file.sha256])),
    {
      'realesrgan-ncnn-vulkan':
        'c1c35d92079085de96b9d547fd7e4464bc8a2e9ccf28d7b8c712d72ade91b7cc',
      'models/realesrgan-x4plus.param':
        '35330ececcea33b6c397a72548e788d5d53becee4734c50b7fada36e89f10a86',
      'models/realesrgan-x4plus.bin':
        '713ee713b0353afaa27976f0563a64a5043bd70b9bd8936c2e26e25ebcdbcddf',
      'models/realesrgan-x4plus-anime.param':
        '2b8fb6e0ae4d2d85704ca08c119a2f5ea40add4f2ecd512eb7f4cd44b6127ed4',
      'models/realesrgan-x4plus-anime.bin':
        'fe01c269cfd10cdef8e018ab66ebe750cf79c7af4d1f9c16c737e1295229bacc',
    },
  )
})

test('rejects absolute, traversal, drive, and backslash paths', () => {
  for (const path of ['/absolute', '../escape', 'models/../escape', 'C:/escape', 'a\\b']) {
    assert.throws(() => assertSafeRelativePath(path), /Unsafe archive path/)
  }
})

test('requires both arm64 and x86_64 Mach-O slices', () => {
  assert.doesNotThrow(() => validateUniversalMachO(universalMachO()))
  const armOnly = universalMachO()
  armOnly.writeUInt32BE(0x0100000c, 8)
  armOnly.writeUInt32LE(0x0100000c, 52)
  assert.throws(() => validateUniversalMachO(armOnly), /both arm64 and x86_64/)

  const invalidSlice = universalMachO()
  invalidSlice.writeUInt32BE(invalidSlice.length + 1, 16)
  assert.throws(() => validateUniversalMachO(invalidSlice), /invalid architecture slice/)
})

test('installs five verified files through an injected fetch implementation', async () => {
  const fixture = createFixture()
  const cache = await temporaryDirectory('zoos-runtime-assets-success')
  let requests = 0
  const fetchMock = async (url) => {
    requests += 1
    assert.equal(url, fixture.catalog.source.url)
    return new Response(fixture.archive, { status: 200 })
  }

  const destination = await fetchAndInstall(fixture.catalog, cache, fetchMock)
  assert.equal(requests, 1)
  assert.deepEqual(
    await readFile(join(destination, 'models', 'photo.param')),
    Buffer.from('photo-param'),
  )
  const engine = join(destination, 'bin', 'engine')
  assert.equal((await stat(engine)).mode & 0o777, 0o755)

  await chmod(engine, 0o644)

  const reused = await fetchAndInstall(fixture.catalog, cache, async () => {
    throw new Error('verified cache must be reused without network access')
  })
  assert.equal(reused, destination)
  assert.equal((await stat(engine)).mode & 0o777, 0o755)
})

test('rejects extra files in an existing verified cache', async () => {
  const fixture = createFixture()
  const cache = await temporaryDirectory('zoos-runtime-assets-extra')
  const destination = await fetchAndInstall(
    fixture.catalog,
    cache,
    async () => new Response(fixture.archive, { status: 200 }),
  )
  await writeFile(join(destination, 'unexpected.txt'), 'not allowlisted')
  await assert.rejects(
    fetchAndInstall(fixture.catalog, cache, async () => {
      throw new Error('network must not be reached for an invalid existing cache')
    }),
    /cache is incomplete/,
  )
})

test('rejects a response that exceeds the pinned archive size while streaming', async () => {
  const fixture = createFixture()
  const cache = await temporaryDirectory('zoos-runtime-assets-oversized')
  const oversized = Buffer.concat([fixture.archive, Buffer.from([0])])
  await assert.rejects(
    fetchAndInstall(
      fixture.catalog,
      cache,
      async () => new Response(oversized, { status: 200 }),
    ),
    /exceeded pinned size/,
  )
})

test('rejects a traversal entry even when it is not allowlisted', async () => {
  const fixture = createFixture([{ name: '../escape', contents: Buffer.from('bad') }])
  const cache = await temporaryDirectory('zoos-runtime-assets-traversal')
  await assert.rejects(
    installArchiveBuffer(fixture.catalog, fixture.archive, cache),
    /Unsafe archive path|invalid relative path/,
  )
})

test('rejects a symbolic link entry', async () => {
  const fixture = createFixture([
    { name: 'ignored-link', contents: Buffer.from('target'), mode: 0o120777 },
  ])
  const cache = await temporaryDirectory('zoos-runtime-assets-symlink')
  await assert.rejects(
    installArchiveBuffer(fixture.catalog, fixture.archive, cache),
    /Symbolic link is forbidden/,
  )
})

test('rejects an allowlisted file with a mismatched hash', async () => {
  const fixture = createFixture()
  fixture.catalog.files[1].sha256 = '0'.repeat(64)
  const cache = await temporaryDirectory('zoos-runtime-assets-hash')
  await assert.rejects(
    installArchiveBuffer(fixture.catalog, fixture.archive, cache),
    /SHA-256 mismatch/,
  )
})

test('catalog remains development-only and requires exactly five files', () => {
  const fixture = createFixture()
  assert.equal(validateCatalog(fixture.catalog), fixture.catalog)
  fixture.catalog.approved_for_distribution = true
  assert.throws(() => validateCatalog(fixture.catalog), /must not be approved/)
})

function createFixture(extraEntries = []) {
  const files = [
    { archive_path: 'engine', destination: 'bin/engine', kind: 'engine', executable: true, contents: universalMachO() },
    { archive_path: 'models/photo.param', destination: 'models/photo.param', kind: 'model', executable: false, contents: Buffer.from('photo-param') },
    { archive_path: 'models/photo.bin', destination: 'models/photo.bin', kind: 'model', executable: false, contents: Buffer.from('photo-bin') },
    { archive_path: 'models/anime.param', destination: 'models/anime.param', kind: 'model', executable: false, contents: Buffer.from('anime-param') },
    { archive_path: 'models/anime.bin', destination: 'models/anime.bin', kind: 'model', executable: false, contents: Buffer.from('anime-bin') },
  ]
  const archive = createStoredZip([
    ...extraEntries,
    ...files.map((file) => ({ name: file.archive_path, contents: file.contents })),
  ])
  const catalog = {
    schema_version: 1,
    id: 'fixture-engine',
    version: '1.0.0',
    approved_for_distribution: false,
    bundled_in_release: false,
    source: {
      url: 'https://github.com/xinntao/Real-ESRGAN/releases/download/test/fixture.zip',
      archive_size: archive.length,
      sha256: sha256(archive),
    },
    files: files.map(({ contents, ...file }) => ({
      ...file,
      size: contents.length,
      sha256: sha256(contents),
    })),
  }
  return { archive, catalog }
}

function universalMachO() {
  const buffer = Buffer.alloc(72)
  buffer.writeUInt32BE(0xcafebabe, 0)
  buffer.writeUInt32BE(2, 4)
  buffer.writeUInt32BE(0x01000007, 8)
  buffer.writeUInt32BE(48, 16)
  buffer.writeUInt32BE(12, 20)
  buffer.writeUInt32BE(0x0100000c, 28)
  buffer.writeUInt32BE(60, 36)
  buffer.writeUInt32BE(12, 40)
  buffer.writeUInt32LE(0xfeedfacf, 48)
  buffer.writeUInt32LE(0x01000007, 52)
  buffer.writeUInt32LE(0xfeedfacf, 60)
  buffer.writeUInt32LE(0x0100000c, 64)
  return buffer
}

function createStoredZip(entries) {
  const localParts = []
  const centralParts = []
  let offset = 0
  for (const entry of entries) {
    const name = Buffer.from(entry.name)
    const contents = Buffer.from(entry.contents)
    const checksum = crc32(contents)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt32LE(checksum, 14)
    local.writeUInt32LE(contents.length, 18)
    local.writeUInt32LE(contents.length, 22)
    local.writeUInt16LE(name.length, 26)
    localParts.push(local, name, contents)

    const central = Buffer.alloc(46)
    central.writeUInt32LE(0x02014b50, 0)
    central.writeUInt16LE(0x0314, 4)
    central.writeUInt16LE(20, 6)
    central.writeUInt32LE(checksum, 16)
    central.writeUInt32LE(contents.length, 20)
    central.writeUInt32LE(contents.length, 24)
    central.writeUInt16LE(name.length, 28)
    central.writeUInt32LE(((entry.mode ?? 0o100644) << 16) >>> 0, 38)
    central.writeUInt32LE(offset, 42)
    centralParts.push(central, name)
    offset += local.length + name.length + contents.length
  }

  const centralDirectory = Buffer.concat(centralParts)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(entries.length, 8)
  end.writeUInt16LE(entries.length, 10)
  end.writeUInt32LE(centralDirectory.length, 12)
  end.writeUInt32LE(offset, 16)
  return Buffer.concat([...localParts, centralDirectory, end])
}

function crc32(buffer) {
  let crc = 0xffffffff
  for (const byte of buffer) {
    crc ^= byte
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1))
    }
  }
  return (crc ^ 0xffffffff) >>> 0
}

async function temporaryDirectory(name) {
  return mkdtemp(join(tmpdir(), `${name}-`))
}
