import assert from 'node:assert/strict'
import { gzipSync } from 'node:zlib'
import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'
import test from 'node:test'

import {
  inspectTarGzip,
  validateArm64MachO,
  validateFfmpegCatalog,
} from './lib/goal2-assets.mjs'

const repositoryRoot = fileURLToPath(new URL('../', import.meta.url))
const catalog = JSON.parse(await readFile(join(repositoryRoot, 'assets/catalog/ffmpeg-macos-arm64.json'), 'utf8'))

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
    /Unsafe asset path/,
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
