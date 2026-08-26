import assert from 'node:assert/strict'
import { gzipSync } from 'node:zlib'
import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'
import test from 'node:test'

import {
  extractAllowedZip,
  inspectTarGzip,
  validateArm64MachO,
  validateFfmpegCatalog,
  validateRifeCatalog,
  validateUniversalMachO,
} from './lib/goal2-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const catalog = JSON.parse(await readFile(join(repositoryRoot, 'assets/catalog/ffmpeg-macos-arm64.json'), 'utf8'))
const rifeCatalog = JSON.parse(await readFile(join(repositoryRoot, 'assets/catalog/rife-ncnn-vulkan-macos.json'), 'utf8'))

test('FFmpeg catalog pins an LGPL-only offline macOS arm64 build', () => {
  assert.equal(validateFfmpegCatalog(catalog), catalog)
  assert.equal(catalog.approved_for_distribution, false)
  assert.equal(catalog.bundled_in_release, false)
  assert.ok(catalog.build.configure_argv.includes('--disable-gpl'))
  assert.ok(catalog.build.configure_argv.includes('--disable-network'))
})

test('FFmpeg tar inspection accepts a regular file below the pinned root', () => {
  const result = inspectTarGzip(tarGzip([{ path: 'ffmpeg-9.0.1/configure', contents: 'ok' }]))
  assert.equal(result.entries.length, 1)
})

test('FFmpeg tar inspection rejects traversal and links', () => {
  assert.throws(
    () => inspectTarGzip(tarGzip([{ path: 'ffmpeg-9.0.1/../escape', contents: 'bad' }])),
    /Unsafe asset path|invalid relative path/,
  )
  assert.throws(
    () => inspectTarGzip(tarGzip([{ path: 'ffmpeg-9.0.1/link', contents: '', type: '2' }])),
    /links and special entries are forbidden/,
  )
})

test('Mach-O validation requires arm64 and macOS 14', () => {
  const binary = Buffer.alloc(64)
  binary.writeUInt32LE(0xfeedfacf, 0)
  binary.writeUInt32LE(0x0100000c, 4)
  binary.writeUInt32LE(1, 16)
  binary.writeUInt32LE(0x32, 32)
  binary.writeUInt32LE(24, 36)
  binary.writeUInt32LE(0x000e0000, 44)
  assert.doesNotThrow(() => validateArm64MachO(binary, true))
  binary.writeUInt32LE(0x000d0000, 44)
  assert.throws(() => validateArm64MachO(binary, true), /macOS 14/)
})

test('RIFE catalog pins only a universal engine and rife-v4.6 model', () => {
  assert.equal(validateRifeCatalog(rifeCatalog), rifeCatalog)
  assert.equal(rifeCatalog.approved_for_distribution, false)
  assert.equal(rifeCatalog.license.model_weights, 'UNVERIFIED')
  assert.deepEqual(
    rifeCatalog.files.map((file) => file.destination),
    ['bin/rife-ncnn-vulkan', 'models/rife-v4.6/flownet.bin', 'models/rife-v4.6/flownet.param'],
  )
})

test('RIFE Mach-O validation requires arm64 and x86_64 slices', () => {
  assert.doesNotThrow(() => validateUniversalMachO(universalMachO()))
  const invalid = universalMachO()
  invalid.writeUInt32BE(0x01000007, 28)
  invalid.writeUInt32LE(0x01000007, 64)
  assert.throws(() => validateUniversalMachO(invalid), /arm64 and x86_64/)
})

test('RIFE ZIP inspection rejects traversal and symbolic links before extraction', async () => {
  await assert.rejects(
    extractAllowedZip(storedZip('../escape', 0o100644), rifeCatalog.files),
    /Unsafe asset path|invalid relative path/,
  )
  await assert.rejects(
    extractAllowedZip(storedZip('rife-ncnn-vulkan-20221029-macos/link', 0o120777), rifeCatalog.files),
    /Symbolic link is forbidden/,
  )
})

function tarGzip(entries) {
  const chunks = []
  for (const entry of entries) {
    const contents = Buffer.from(entry.contents)
    const header = Buffer.alloc(512)
    header.write(entry.path, 0, 100, 'utf8')
    writeOctal(header, 100, 8, 0o755)
    writeOctal(header, 124, 12, contents.length)
    header[156] = (entry.type ?? '0').charCodeAt(0)
    chunks.push(header, contents, Buffer.alloc((512 - (contents.length % 512)) % 512))
  }
  chunks.push(Buffer.alloc(1024))
  return gzipSync(Buffer.concat(chunks))
}

function writeOctal(buffer, offset, length, value) {
  const text = value.toString(8).padStart(length - 1, '0')
  buffer.write(text, offset, length - 1, 'ascii')
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

function storedZip(path, mode) {
  const name = Buffer.from(path)
  const local = Buffer.alloc(30)
  local.writeUInt32LE(0x04034b50, 0)
  local.writeUInt16LE(20, 4)
  local.writeUInt16LE(name.length, 26)

  const central = Buffer.alloc(46)
  central.writeUInt32LE(0x02014b50, 0)
  central.writeUInt16LE(0x031e, 4)
  central.writeUInt16LE(20, 6)
  central.writeUInt16LE(name.length, 28)
  central.writeUInt32LE(mode * 65536, 38)

  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(1, 8)
  end.writeUInt16LE(1, 10)
  end.writeUInt32LE(central.length + name.length, 12)
  end.writeUInt32LE(local.length + name.length, 16)
  return Buffer.concat([local, name, central, name, end])
}
