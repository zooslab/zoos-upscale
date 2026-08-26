import { createHash, randomUUID } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { gunzipSync } from 'node:zlib'
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, posix, resolve, sep } from 'node:path'

const SHA256_PATTERN = /^[a-f0-9]{64}$/
const TAR_BLOCK_SIZE = 512
const MAX_FFMPEG_TAR_SIZE = 128 * 1024 * 1024
const MH_MAGIC_64 = 0xfeedfacf
const CPU_TYPE_ARM64 = 0x0100000c
const LC_BUILD_VERSION = 0x32
const MACOS_14 = 0x000e0000
const FFMPEG_SOURCE_NAME = 'ffmpeg-9.0.1.tar.gz'

export function validateFfmpegCatalog(catalog) {
  validateDevelopmentOnly(catalog)
  if (
    catalog.id !== 'ffmpeg-macos-arm64' ||
    catalog.version !== '9.0.1' ||
    catalog.architecture !== 'arm64' ||
    catalog.minimum_macos !== '14.0'
  ) {
    throw new Error('Unexpected FFmpeg catalog identity')
  }
  if (
    catalog.license !== 'LGPL-2.1-or-later' ||
    catalog.source?.url !== 'https://ffmpeg.org/releases/ffmpeg-9.0.1.tar.gz' ||
    catalog.source?.signature_url !== 'https://ffmpeg.org/releases/ffmpeg-9.0.1.tar.gz.asc' ||
    catalog.source?.signing_key_fingerprint !== 'FCF986EA15E6E293A5644F10B4322F04D67658D8'
  ) {
    throw new Error('FFmpeg source or license provenance is not approved')
  }
  validateSizedHash(catalog.source.sha256, catalog.source.archive_size, 'FFmpeg source')

  const requiredFlags = [
    '--extra-cflags=-mmacosx-version-min=14.0',
    '--extra-ldflags=-mmacosx-version-min=14.0',
    '--disable-autodetect',
    '--disable-gpl',
    '--disable-nonfree',
    '--disable-doc',
    '--disable-debug',
    '--disable-ffplay',
    '--disable-network',
    '--disable-devices',
    '--enable-static',
    '--disable-shared',
    '--enable-zlib',
    '--enable-videotoolbox',
  ]
  if (!Array.isArray(catalog.build?.configure_argv)) throw new Error('Missing FFmpeg configure recipe')
  for (const flag of requiredFlags) {
    if (!catalog.build.configure_argv.includes(flag)) throw new Error(`Missing FFmpeg configure flag: ${flag}`)
  }
  if (catalog.build.license_result !== 'LGPL version 2.1 or later') {
    throw new Error('FFmpeg build is not pinned to the LGPL result')
  }
  if (!Array.isArray(catalog.files) || catalog.files.length !== 2) {
    throw new Error('FFmpeg catalog must contain ffmpeg and ffprobe only')
  }
  const destinations = new Set(catalog.files.map((file) => file.destination))
  if (!destinations.has('bin/ffmpeg') || !destinations.has('bin/ffprobe')) {
    throw new Error('FFmpeg catalog output allowlist is invalid')
  }
  for (const file of catalog.files) {
    assertSafeRelativePath(file.destination)
    validateSizedHash(file.sha256, file.size, file.destination)
    if (file.architecture !== 'arm64' || file.executable !== true) {
      throw new Error(`FFmpeg output is not an arm64 executable: ${file.destination}`)
    }
  }
  return catalog
}

export async function fetchFfmpegSource(
  catalogValue,
  sourceRoot,
  fetchImplementation = globalThis.fetch,
) {
  const catalog = validateFfmpegCatalog(catalogValue)
  await assertSafeCacheRoot(sourceRoot)
  const destination = ffmpegSourceDirectory(catalog, sourceRoot)
  const archivePath = join(destination, FFMPEG_SOURCE_NAME)
  if (await verifySourceCache(catalog, destination)) return archivePath
  if (await exists(destination)) throw new Error(`Existing FFmpeg source cache is incomplete: ${destination}`)

  const archive = await fetchVerifiedBuffer(
    catalog.source.url,
    catalog.source.sha256,
    catalog.source.archive_size,
    fetchImplementation,
  )
  // Validate every path and type before persisting a network artifact.
  inspectTarGzip(archive)
  const staging = `${destination}.staging-${randomUUID()}`
  await mkdir(staging, { recursive: true })
  try {
    await writeFile(join(staging, FFMPEG_SOURCE_NAME), archive, { mode: 0o644 })
    await writeFile(join(staging, 'catalog.json'), `${JSON.stringify(catalog, null, 2)}\n`)
    await mkdir(dirname(destination), { recursive: true })
    await rename(staging, destination)
    return archivePath
  } catch (error) {
    await rm(staging, { recursive: true, force: true })
    throw error
  }
}

export async function buildFfmpeg(catalogValue, evidence, sourceRoot, runtimeRoot) {
  const catalog = validateFfmpegCatalog(catalogValue)
  validateFfmpegEvidence(catalog, evidence)
  await assertSafeCacheRoot(sourceRoot)
  await assertSafeCacheRoot(runtimeRoot)
  const sourceDirectory = ffmpegSourceDirectory(catalog, sourceRoot)
  if (!(await verifySourceCache(catalog, sourceDirectory))) {
    throw new Error('Verified FFmpeg source is missing; run `pnpm goal2:fetch` first')
  }
  const destination = join(runtimeRoot, catalog.id, catalog.version)
  if (await verifyFfmpegInstallation(catalog, evidence, destination)) return destination
  if (await exists(destination)) throw new Error(`Existing FFmpeg runtime cache is incomplete: ${destination}`)

  const archive = await readFile(join(sourceDirectory, FFMPEG_SOURCE_NAME))
  verifyBuffer(archive, catalog.source.sha256, catalog.source.archive_size, 'FFmpeg source')
  const buildRoot = await mkdtemp(join(tmpdir(), 'zoos-ffmpeg-build-'))
  try {
    await extractTarGzip(archive, buildRoot)
    const source = join(buildRoot, `ffmpeg-${catalog.version}`)
    run(join(source, 'configure'), catalog.build.configure_argv, source)
    run('/usr/bin/make', catalog.build.make_argv, source)

    const binaries = new Map()
    for (const file of catalog.files) {
      const binary = await readFile(join(source, file.destination.slice('bin/'.length)))
      verifyBuffer(binary, file.sha256, file.size, file.destination)
      validateArm64MachO(binary, true)
      binaries.set(file.destination, binary)
    }
    verifyFfmpegReceipt(source, evidence)

    const staging = `${destination}.staging-${randomUUID()}`
    await mkdir(join(staging, 'bin'), { recursive: true })
    try {
      for (const file of catalog.files) {
        const output = safeDestination(staging, file.destination)
        await writeFile(output, binaries.get(file.destination), { mode: 0o755 })
        await chmod(output, 0o755)
      }
      await writeFile(join(staging, 'catalog.json'), `${JSON.stringify(catalog, null, 2)}\n`)
      await writeFile(join(staging, 'build-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`)
      await mkdir(dirname(destination), { recursive: true })
      await rename(staging, destination)
    } catch (error) {
      await rm(staging, { recursive: true, force: true })
      throw error
    }
    return destination
  } finally {
    await rm(buildRoot, { recursive: true, force: true })
  }
}

export function inspectTarGzip(archive) {
  const tar = decompressTar(archive)
  const entries = []
  let offset = 0
  const seen = new Set()
  while (offset + TAR_BLOCK_SIZE <= tar.length) {
    const header = tar.subarray(offset, offset + TAR_BLOCK_SIZE)
    if (header.every((byte) => byte === 0)) break
    const name = tarString(header.subarray(0, 100))
    const prefix = tarString(header.subarray(345, 500))
    const rawPath = prefix ? `${prefix}/${name}` : name
    const type = header[156]
    const directory = type === 0x35
    const archivePath = assertSafeRelativePath(directory && rawPath.endsWith('/') ? rawPath.slice(0, -1) : rawPath)
    if (type !== 0 && type !== 0x30 && type !== 0x35) {
      throw new Error(`Tar links and special entries are forbidden: ${archivePath}`)
    }
    if (seen.has(archivePath)) throw new Error(`Duplicate tar entry: ${archivePath}`)
    seen.add(archivePath)
    const size = parseTarOctal(header.subarray(124, 136), archivePath)
    const dataStart = offset + TAR_BLOCK_SIZE
    const dataEnd = dataStart + size
    if (dataEnd > tar.length) throw new Error(`Truncated tar entry: ${archivePath}`)
    entries.push({ archivePath, directory, mode: parseTarOctal(header.subarray(100, 108), archivePath), dataStart, dataEnd })
    offset = dataStart + Math.ceil(size / TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
  }
  if (
    entries.length === 0 ||
    !entries.every(
      (entry) => entry.archivePath === 'ffmpeg-9.0.1' || entry.archivePath.startsWith('ffmpeg-9.0.1/'),
    )
  ) {
    throw new Error('FFmpeg archive has an unexpected root')
  }
  return { tar, entries }
}

async function extractTarGzip(archive, destination) {
  const { tar, entries } = inspectTarGzip(archive)
  for (const entry of entries) {
    const output = safeDestination(destination, entry.archivePath)
    if (entry.directory) {
      await mkdir(output, { recursive: true })
    } else {
      await mkdir(dirname(output), { recursive: true })
      await writeFile(output, tar.subarray(entry.dataStart, entry.dataEnd), { mode: entry.mode & 0o777 })
    }
  }
}

function validateFfmpegEvidence(catalog, evidence) {
  if (
    evidence?.schema_version !== 1 ||
    evidence.catalog_id !== catalog.id ||
    evidence.catalog_version !== catalog.version ||
    evidence.version !== catalog.version ||
    evidence.license !== catalog.build.license_result ||
    evidence.host?.architecture !== 'arm64' ||
    evidence.host?.minimum_macos !== catalog.minimum_macos
  ) {
    throw new Error('FFmpeg build evidence does not match the catalog')
  }
  for (const [name, hash] of Object.entries(evidence.output_hashes ?? {})) {
    if (!SHA256_PATTERN.test(hash)) throw new Error(`Invalid FFmpeg ${name} evidence hash`)
  }
  for (const file of catalog.files) {
    const name = file.destination.slice('bin/'.length)
    if (evidence.files?.[name]?.sha256 !== file.sha256 || evidence.files?.[name]?.size !== file.size) {
      throw new Error(`FFmpeg evidence does not pin ${name}`)
    }
  }
}

function verifyFfmpegReceipt(source, evidence) {
  const ffmpeg = join(source, 'ffmpeg')
  const version = run(ffmpeg, ['-version'], source).stdout
  if (!version.startsWith('ffmpeg version 9.0.1') || !version.includes(evidence.host.compiler)) {
    throw new Error('FFmpeg version/compiler receipt mismatch')
  }
  const checks = {
    buildconf: ['-hide_banner', '-buildconf'],
    codecs: ['-hide_banner', '-codecs'],
    formats: ['-hide_banner', '-formats'],
    encoders: ['-hide_banner', '-encoders'],
    decoders: ['-hide_banner', '-decoders'],
  }
  for (const [name, args] of Object.entries(checks)) {
    const output = run(ffmpeg, args, source).stdout
    const hash = sha256(Buffer.from(output))
    if (hash !== evidence.output_hashes[name]) throw new Error(`FFmpeg ${name} receipt mismatch`)
  }
}

async function verifySourceCache(catalog, destination) {
  try {
    const names = (await readdir(destination)).sort()
    if (names.join(',') !== `catalog.json,${FFMPEG_SOURCE_NAME}`) return false
    const installedCatalog = JSON.parse(await readFile(join(destination, 'catalog.json'), 'utf8'))
    if (JSON.stringify(installedCatalog) !== JSON.stringify(catalog)) return false
    const archive = await readFile(join(destination, FFMPEG_SOURCE_NAME))
    verifyBuffer(archive, catalog.source.sha256, catalog.source.archive_size, FFMPEG_SOURCE_NAME)
    inspectTarGzip(archive)
    return true
  } catch {
    return false
  }
}

async function verifyFfmpegInstallation(catalog, evidence, destination) {
  try {
    const allowed = new Set(['catalog.json', 'build-evidence.json', ...catalog.files.map((file) => file.destination)])
    const files = await listFiles(destination)
    if (files.length !== allowed.size || files.some((file) => !allowed.has(file))) return false
    if (JSON.stringify(JSON.parse(await readFile(join(destination, 'catalog.json'), 'utf8'))) !== JSON.stringify(catalog)) return false
    if (JSON.stringify(JSON.parse(await readFile(join(destination, 'build-evidence.json'), 'utf8'))) !== JSON.stringify(evidence)) return false
    for (const file of catalog.files) {
      const path = safeDestination(destination, file.destination)
      const info = await lstat(path)
      if (!info.isFile() || info.isSymbolicLink()) return false
      const binary = await readFile(path)
      verifyBuffer(binary, file.sha256, file.size, file.destination)
      validateArm64MachO(binary, true)
      await chmod(path, 0o755)
    }
    return true
  } catch {
    return false
  }
}

export function validateArm64MachO(buffer, requireMacos14 = false) {
  if (buffer.length < 32 || buffer.readUInt32LE(0) !== MH_MAGIC_64 || buffer.readUInt32LE(4) !== CPU_TYPE_ARM64) {
    throw new Error('Binary is not a thin arm64 Mach-O')
  }
  if (!requireMacos14) return
  const commandCount = buffer.readUInt32LE(16)
  let offset = 32
  for (let index = 0; index < commandCount; index += 1) {
    if (offset + 8 > buffer.length) break
    const command = buffer.readUInt32LE(offset)
    const size = buffer.readUInt32LE(offset + 4)
    if (size < 8 || offset + size > buffer.length) break
    if (command === LC_BUILD_VERSION) {
      if (size < 24 || buffer.readUInt32LE(offset + 12) !== MACOS_14) {
        throw new Error('Binary does not require the pinned macOS 14.0 minimum')
      }
      return
    }
    offset += size
  }
  throw new Error('Binary has no valid LC_BUILD_VERSION command')
}

export function assertSafeRelativePath(value) {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\\') || value.startsWith('/') || /^[A-Za-z]:/.test(value)) {
    throw new Error(`Unsafe asset path: ${value}`)
  }
  const parts = value.split('/')
  if (parts.some((part) => part === '' || part === '.' || part === '..') || posix.normalize(value) !== value) {
    throw new Error(`Unsafe asset path: ${value}`)
  }
  return value
}

function validateDevelopmentOnly(catalog) {
  if (catalog?.schema_version !== 1) throw new Error('Unsupported Goal 2 asset catalog version')
  if (catalog.approved_for_distribution !== false || catalog.bundled_in_release !== false) {
    throw new Error('Goal 2 assets must remain development-only')
  }
}

function validateSizedHash(hash, size, label) {
  if (!SHA256_PATTERN.test(hash) || !Number.isSafeInteger(size) || size <= 0) {
    throw new Error(`Invalid size or SHA-256 for ${label}`)
  }
}

async function fetchVerifiedBuffer(url, expectedHash, expectedSize, fetchImplementation) {
  const response = await fetchImplementation(url, { redirect: 'follow' })
  if (!response.ok) throw new Error(`Asset download failed with HTTP ${response.status}`)
  const declared = response.headers?.get?.('content-length')
  if (declared !== null && declared !== undefined && Number(declared) !== expectedSize) {
    throw new Error('Asset Content-Length does not match the pinned size')
  }
  const reader = response.body?.getReader?.()
  if (!reader) {
    const buffer = Buffer.from(await response.arrayBuffer())
    verifyBuffer(buffer, expectedHash, expectedSize, url)
    return buffer
  }
  const chunks = []
  let length = 0
  while (true) {
    const { done, value } = await reader.read()
    if (done) break
    length += value.byteLength
    if (length > expectedSize) {
      await reader.cancel().catch(() => {})
      throw new Error('Asset download exceeded the pinned size')
    }
    chunks.push(Buffer.from(value))
  }
  const buffer = Buffer.concat(chunks, length)
  verifyBuffer(buffer, expectedHash, expectedSize, url)
  return buffer
}

function verifyBuffer(buffer, expectedHash, expectedSize, label) {
  if (buffer.length !== expectedSize) throw new Error(`Unexpected size for ${label}`)
  if (sha256(buffer) !== expectedHash) throw new Error(`SHA-256 mismatch for ${label}`)
}

function sha256(buffer) {
  return createHash('sha256').update(buffer).digest('hex')
}

function decompressTar(archive) {
  try {
    return gunzipSync(archive, { maxOutputLength: MAX_FFMPEG_TAR_SIZE })
  } catch (error) {
    throw new Error(`Invalid or oversized FFmpeg tar.gz: ${error.message}`)
  }
}

function tarString(buffer) {
  const end = buffer.indexOf(0)
  return buffer.subarray(0, end === -1 ? buffer.length : end).toString('utf8')
}

function parseTarOctal(buffer, path) {
  const value = tarString(buffer).trim()
  if (!/^[0-7]+$/.test(value)) throw new Error(`Invalid tar number for ${path}`)
  const parsed = Number.parseInt(value, 8)
  if (!Number.isSafeInteger(parsed) || parsed < 0) throw new Error(`Invalid tar number for ${path}`)
  return parsed
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', env: { ...process.env, MACOSX_DEPLOYMENT_TARGET: '14.0' } })
  if (result.error) throw result.error
  if (result.status !== 0) {
    throw new Error(`${command} failed with exit ${result.status}: ${(result.stderr ?? '').slice(-4000)}`)
  }
  return result
}

function ffmpegSourceDirectory(catalog, sourceRoot) {
  return join(sourceRoot, catalog.id, catalog.version)
}

function safeDestination(root, relativePath) {
  assertSafeRelativePath(relativePath)
  const destination = resolve(root, ...relativePath.split('/'))
  if (!destination.startsWith(`${resolve(root)}${sep}`)) throw new Error('Unsafe destination path')
  return destination
}

async function listFiles(directory, root = directory) {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    const info = await lstat(path)
    if (info.isSymbolicLink()) throw new Error(`Symbolic link is forbidden: ${path}`)
    if (info.isDirectory()) files.push(...(await listFiles(path, root)))
    else if (info.isFile()) files.push(path.slice(root.length + 1).split(sep).join('/'))
    else throw new Error(`Special file is forbidden: ${path}`)
  }
  return files.sort()
}

async function assertSafeCacheRoot(cacheRoot) {
  const absolute = resolve(cacheRoot)
  for (const candidate of [dirname(absolute), absolute]) {
    let current = candidate
    while (true) {
      try {
        const info = await lstat(current)
        if (info.isSymbolicLink()) throw new Error(`Symbolic link is forbidden in cache path: ${current}`)
        if (!info.isDirectory()) throw new Error(`Cache path component is not a directory: ${current}`)
        break
      } catch (error) {
        if (error?.code !== 'ENOENT') throw error
        const parent = dirname(current)
        if (parent === current) throw error
        current = parent
      }
    }
  }
}

async function exists(path) {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (error?.code === 'ENOENT') return false
    throw error
  }
}
