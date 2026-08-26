import { createHash, randomUUID } from 'node:crypto'
import { gunzipSync } from 'node:zlib'
import { lstat, mkdir, readFile, readdir, rename, rm, writeFile } from 'node:fs/promises'
import { dirname, join, posix, relative, resolve, sep } from 'node:path'

const SHA256_PATTERN = /^[a-f0-9]{64}$/
const TAR_BLOCK_SIZE = 512
const MAX_TAR_SIZE = 256 * 1024 * 1024
const ARM64_MACHO_MAGIC = 0xfeedfacf
const CPU_TYPE_ARM64 = 0x0100000c

export function validateOrtCatalog(catalog) {
  validateDevelopmentOnly(catalog)
  if (catalog.id !== 'onnxruntime-macos-arm64' || catalog.version !== '1.29.0') {
    throw new Error('Unexpected ONNX Runtime catalog identity')
  }
  if (!catalog.source?.url?.startsWith('https://github.com/microsoft/onnxruntime/releases/download/v1.29.0/')) {
    throw new Error('ONNX Runtime source must be the pinned official release')
  }
  validateSource(catalog.source)
  if (!Array.isArray(catalog.files) || catalog.files.length !== 1) {
    throw new Error('ONNX Runtime catalog must allow exactly one file')
  }
  const file = catalog.files[0]
  assertSafeArchivePath(file.archive_path)
  assertSafeRelativePath(file.destination)
  validateFile(file)
  if (file.destination !== 'lib/libonnxruntime.1.29.0.dylib' || file.architecture !== 'arm64') {
    throw new Error('ONNX Runtime catalog must pin the versioned arm64 dylib')
  }
  return catalog
}

export function validateWeightCatalog(catalog) {
  validateDevelopmentOnly(catalog)
  if (catalog.id !== 'realesrgan-pytorch-weights' || catalog.source?.repository !== 'https://github.com/xinntao/Real-ESRGAN') {
    throw new Error('Unexpected Real-ESRGAN weight catalog identity')
  }
  if (!Array.isArray(catalog.files) || catalog.files.length !== 2) {
    throw new Error('Weight catalog must contain photo and anime weights')
  }
  const ids = new Set()
  for (const file of catalog.files) {
    if (!['photo', 'anime'].includes(file.id) || ids.has(file.id)) throw new Error('Weight ids must be unique photo and anime')
    ids.add(file.id)
    if (!file.url?.startsWith(`https://github.com/xinntao/Real-ESRGAN/releases/download/${file.release}/`)) {
      throw new Error('Weight source must be an official pinned release')
    }
    assertSafeRelativePath(file.destination)
    validateFile(file)
  }
  return catalog
}

function validateDevelopmentOnly(catalog) {
  if (catalog.schema_version !== 1) throw new Error('Unsupported asset catalog version')
  if (catalog.approved_for_distribution !== false || catalog.bundled_in_release !== false) {
    throw new Error('Goal 1B assets must remain development-only')
  }
}

function validateSource(source) {
  if (!SHA256_PATTERN.test(source.sha256)) throw new Error('Invalid archive SHA-256')
  if (!Number.isSafeInteger(source.archive_size) || source.archive_size <= 0) throw new Error('Invalid archive size')
}

function validateFile(file) {
  if (!SHA256_PATTERN.test(file.sha256)) throw new Error('Invalid file SHA-256')
  if (!Number.isSafeInteger(file.size) || file.size <= 0) throw new Error('Invalid file size')
}

export function assertSafeRelativePath(value) {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\\') || value.startsWith('/') || /^[A-Za-z]:/.test(value)) {
    throw new Error(`Unsafe asset path: ${value}`)
  }
  const parts = value.split('/')
  if (parts.some((part) => part === '' || part === '.' || part === '..') || posix.normalize(value) !== value) {
    throw new Error(`Unsafe asset path: ${value}`)
  }
}

export function assertSafeArchivePath(value) {
  const normalized = value?.startsWith('./') ? value.slice(2) : value
  assertSafeRelativePath(normalized)
  return normalized
}

export function validateArm64MachO(buffer) {
  if (buffer.length < 12 || buffer.readUInt32LE(0) !== ARM64_MACHO_MAGIC || buffer.readUInt32LE(4) !== CPU_TYPE_ARM64) {
    throw new Error('Runtime library is not an arm64 Mach-O binary')
  }
}

export async function fetchAndInstallOrt(catalogValue, cacheRoot, fetchImplementation = globalThis.fetch) {
  const catalog = validateOrtCatalog(catalogValue)
  const destination = join(cacheRoot, catalog.id, catalog.version)
  if (await verifyInstallation(catalog, destination, validateArm64MachO)) return destination
  if (await exists(destination)) throw new Error(`Existing ONNX Runtime cache is incomplete: ${destination}`)

  const archive = await fetchVerifiedBuffer(catalog.source.url, catalog.source.sha256, catalog.source.archive_size, fetchImplementation)
  const extracted = extractAllowedTarGzip(archive, catalog.files)
  return installFiles(catalog, destination, extracted, validateArm64MachO)
}

export async function fetchAndInstallWeights(catalogValue, cacheRoot, fetchImplementation = globalThis.fetch) {
  const catalog = validateWeightCatalog(catalogValue)
  const destination = join(cacheRoot, catalog.id, catalog.version)
  if (await verifyInstallation(catalog, destination)) return destination
  if (await exists(destination)) throw new Error(`Existing weight cache is incomplete: ${destination}`)

  const files = new Map()
  for (const file of catalog.files) {
    files.set(file.destination, await fetchVerifiedBuffer(file.url, file.sha256, file.size, fetchImplementation))
  }
  return installFiles(catalog, destination, files)
}

export function extractAllowedTarGzip(archive, files) {
  let tar
  try {
    tar = gunzipSync(archive, { maxOutputLength: MAX_TAR_SIZE })
  } catch (error) {
    throw new Error(`Invalid or oversized gzip archive: ${error.message}`)
  }
  const allowed = new Map(files.map((file) => [assertSafeArchivePath(file.archive_path), file]))
  const extracted = new Map()
  let offset = 0
  while (offset + TAR_BLOCK_SIZE <= tar.length) {
    const header = tar.subarray(offset, offset + TAR_BLOCK_SIZE)
    if (header.every((byte) => byte === 0)) break
    const name = tarString(header.subarray(0, 100))
    const prefix = tarString(header.subarray(345, 500))
    const type = header[156]
    const rawPath = prefix ? `${prefix}/${name}` : name
    const archivePath = assertSafeArchivePath(type === 0x35 && rawPath.endsWith('/') ? rawPath.slice(0, -1) : rawPath)
    const size = parseTarOctal(header.subarray(124, 136), archivePath)
    const dataStart = offset + TAR_BLOCK_SIZE
    const dataEnd = dataStart + size
    if (dataEnd > tar.length) throw new Error(`Truncated tar entry: ${archivePath}`)
    if ((type === 0x32 || type === 0x31) && allowed.has(archivePath)) {
      throw new Error(`Link is forbidden for allowlisted archive entry: ${archivePath}`)
    }
    if (type === 0x32 || type === 0x31) {
      offset = dataStart + Math.ceil(size / TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
      continue
    }
    if (type !== 0 && type !== 0x30 && type !== 0x35) throw new Error(`Unsupported tar entry type for ${archivePath}`)
    if (type !== 0x35 && allowed.has(archivePath)) {
      if (extracted.has(archivePath)) throw new Error(`Duplicate tar entry: ${archivePath}`)
      const file = allowed.get(archivePath)
      const contents = Buffer.from(tar.subarray(dataStart, dataEnd))
      verifyBuffer(contents, file.sha256, file.size, archivePath)
      extracted.set(file.destination, contents)
    }
    offset = dataStart + Math.ceil(size / TAR_BLOCK_SIZE) * TAR_BLOCK_SIZE
  }
  const missing = files.filter((file) => !extracted.has(file.destination))
  if (missing.length > 0) throw new Error(`Archive is missing: ${missing.map((file) => file.archive_path).join(', ')}`)
  return extracted
}

async function fetchVerifiedBuffer(url, sha256, size, fetchImplementation) {
  const response = await fetchImplementation(url, { redirect: 'follow' })
  if (!response.ok) throw new Error(`Asset download failed with HTTP ${response.status}`)
  const declared = response.headers?.get?.('content-length')
  if (declared !== null && declared !== undefined && Number(declared) !== size) {
    throw new Error('Asset Content-Length does not match the pinned size')
  }
  const reader = response.body?.getReader?.()
  if (!reader) {
    const buffer = Buffer.from(await response.arrayBuffer())
    verifyBuffer(buffer, sha256, size, url)
    return buffer
  }
  const chunks = []
  let length = 0
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      length += value.byteLength
      if (length > size) throw new Error('Asset download exceeded the pinned size')
      chunks.push(Buffer.from(value))
    }
  } catch (error) {
    await reader.cancel(error).catch(() => {})
    throw error
  }
  const buffer = Buffer.concat(chunks, length)
  verifyBuffer(buffer, sha256, size, url)
  return buffer
}

async function installFiles(catalog, destination, files, binaryValidator) {
  const staging = `${destination}.staging-${randomUUID()}`
  await mkdir(staging, { recursive: true })
  try {
    for (const file of catalog.files) {
      const contents = files.get(file.destination)
      if (!contents) throw new Error(`Missing staged asset: ${file.destination}`)
      if (binaryValidator) binaryValidator(contents)
      const output = safeDestination(staging, file.destination)
      await mkdir(dirname(output), { recursive: true })
      await writeFile(output, contents, { mode: 0o644 })
    }
    await writeFile(join(staging, 'catalog.json'), `${JSON.stringify(catalog, null, 2)}\n`)
    await mkdir(dirname(destination), { recursive: true })
    await rename(staging, destination)
    return destination
  } catch (error) {
    await rm(staging, { recursive: true, force: true })
    throw error
  }
}

async function verifyInstallation(catalog, destination, binaryValidator) {
  if (!(await exists(destination))) return false
  const rootInfo = await lstat(destination)
  if (rootInfo.isSymbolicLink()) throw new Error(`Symbolic link is forbidden in cache: ${destination}`)
  if (!rootInfo.isDirectory()) return false
  const expected = new Set(['catalog.json', ...catalog.files.map((file) => file.destination)])
  const installed = await listFiles(destination)
  if (installed.length !== expected.size || installed.some((file) => !expected.has(file))) return false
  const installedCatalog = JSON.parse(await readFile(join(destination, 'catalog.json'), 'utf8'))
  if (JSON.stringify(installedCatalog) !== JSON.stringify(catalog)) return false
  for (const file of catalog.files) {
    const path = safeDestination(destination, file.destination)
    const info = await lstat(path)
    if (!info.isFile() || info.isSymbolicLink()) return false
    const contents = await readFile(path)
    verifyBuffer(contents, file.sha256, file.size, file.destination)
    if (binaryValidator) binaryValidator(contents)
  }
  return true
}

async function listFiles(root, directory = root) {
  const result = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    const info = await lstat(path)
    if (info.isSymbolicLink()) throw new Error(`Symbolic link is forbidden in cache: ${path}`)
    if (info.isDirectory()) result.push(...(await listFiles(root, path)))
    else if (info.isFile()) result.push(relative(root, path).split(sep).join('/'))
    else throw new Error(`Unsupported cache entry: ${path}`)
  }
  return result.sort()
}

function safeDestination(root, value) {
  assertSafeRelativePath(value)
  const output = resolve(root, ...value.split('/'))
  if (!output.startsWith(`${resolve(root)}${sep}`)) throw new Error(`Unsafe asset path: ${value}`)
  return output
}

function verifyBuffer(buffer, sha256, size, label) {
  if (buffer.length !== size) throw new Error(`Unexpected size for ${label}`)
  if (createHash('sha256').update(buffer).digest('hex') !== sha256) throw new Error(`SHA-256 mismatch for ${label}`)
}

function tarString(buffer) {
  const zero = buffer.indexOf(0)
  return buffer.subarray(0, zero === -1 ? buffer.length : zero).toString('utf8')
}

function parseTarOctal(buffer, path) {
  const value = tarString(buffer).trim()
  if (!/^[0-7]+$/.test(value)) throw new Error(`Invalid tar size for ${path}`)
  const parsed = Number.parseInt(value, 8)
  if (!Number.isSafeInteger(parsed) || parsed < 0) throw new Error(`Invalid tar size for ${path}`)
  return parsed
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
