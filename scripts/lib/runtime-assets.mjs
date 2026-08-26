import { createHash, randomUUID } from 'node:crypto'
import { chmod, lstat, mkdir, readFile, readdir, rename, rm, stat, writeFile } from 'node:fs/promises'
import { dirname, join, posix, relative, resolve, sep } from 'node:path'

import yauzl from 'yauzl'

const SHA256_PATTERN = /^[a-f0-9]{64}$/
const FAT_MAGIC = 0xcafebabe
const CPU_TYPE_X86_64 = 0x01000007
const CPU_TYPE_ARM64 = 0x0100000c
const MH_MAGIC_64 = 0xfeedfacf
const MAX_FAT_ARCHITECTURES = 32

export function validateCatalog(catalog) {
  if (catalog.schema_version !== 1) throw new Error('Unsupported asset catalog version')
  if (catalog.approved_for_distribution !== false || catalog.bundled_in_release !== false) {
    throw new Error('Development catalog must not be approved or bundled for distribution')
  }
  if (!catalog.source?.url?.startsWith('https://github.com/xinntao/Real-ESRGAN/')) {
    throw new Error('Asset source must be the approved official Real-ESRGAN repository')
  }
  if (!SHA256_PATTERN.test(catalog.source.sha256)) throw new Error('Invalid archive SHA-256')
  if (!Number.isSafeInteger(catalog.source.archive_size) || catalog.source.archive_size <= 0) {
    throw new Error('Invalid archive size')
  }
  if (!Array.isArray(catalog.files) || catalog.files.length !== 5) {
    throw new Error('Real-ESRGAN development catalog must allow exactly five files')
  }

  const archivePaths = new Set()
  const destinations = new Set()
  for (const file of catalog.files) {
    assertSafeRelativePath(file.archive_path)
    assertSafeRelativePath(file.destination)
    if (archivePaths.has(file.archive_path) || destinations.has(file.destination)) {
      throw new Error('Catalog contains duplicate paths')
    }
    archivePaths.add(file.archive_path)
    destinations.add(file.destination)
    if (!SHA256_PATTERN.test(file.sha256)) throw new Error(`Invalid SHA-256 for ${file.archive_path}`)
    if (!Number.isSafeInteger(file.size) || file.size <= 0) {
      throw new Error(`Invalid size for ${file.archive_path}`)
    }
  }

  const engines = catalog.files.filter((file) => file.kind === 'engine' && file.executable)
  if (engines.length !== 1) throw new Error('Catalog must contain one executable engine')
  return catalog
}

export function assertSafeRelativePath(value) {
  if (typeof value !== 'string' || value.length === 0) throw new Error('Archive path is empty')
  if (value.includes('\\') || value.startsWith('/') || /^[A-Za-z]:/.test(value)) {
    throw new Error(`Unsafe archive path: ${value}`)
  }
  const parts = value.split('/')
  if (parts.some((part) => part === '' || part === '.' || part === '..')) {
    throw new Error(`Unsafe archive path: ${value}`)
  }
  if (posix.normalize(value) !== value) throw new Error(`Unsafe archive path: ${value}`)
}

export function isSymbolicLink(entry) {
  const unixMode = entry.externalFileAttributes >>> 16
  return (unixMode & 0o170000) === 0o120000
}

export function validateUniversalMachO(buffer) {
  if (buffer.length < 48 || buffer.readUInt32BE(0) !== FAT_MAGIC) {
    throw new Error('Engine is not a universal Mach-O binary')
  }
  const architectureCount = buffer.readUInt32BE(4)
  if (
    architectureCount < 2 ||
    architectureCount > MAX_FAT_ARCHITECTURES ||
    buffer.length < 8 + architectureCount * 20
  ) {
    throw new Error('Universal Mach-O architecture table is incomplete')
  }
  const cpuTypes = new Set()
  const slices = []
  for (let index = 0; index < architectureCount; index += 1) {
    const architectureOffset = 8 + index * 20
    const cpuType = buffer.readUInt32BE(architectureOffset)
    const sliceOffset = buffer.readUInt32BE(architectureOffset + 8)
    const sliceSize = buffer.readUInt32BE(architectureOffset + 12)
    const sliceEnd = sliceOffset + sliceSize
    if (
      sliceSize < 12 ||
      sliceOffset < 8 + architectureCount * 20 ||
      sliceEnd > buffer.length ||
      sliceEnd < sliceOffset
    ) {
      throw new Error('Universal Mach-O contains an invalid architecture slice')
    }
    if (
      buffer.readUInt32LE(sliceOffset) !== MH_MAGIC_64 ||
      buffer.readUInt32LE(sliceOffset + 4) !== cpuType
    ) {
      throw new Error('Universal Mach-O slice header does not match its architecture')
    }
    cpuTypes.add(cpuType)
    slices.push([sliceOffset, sliceEnd])
  }
  slices.sort((left, right) => left[0] - right[0])
  for (let index = 1; index < slices.length; index += 1) {
    if (slices[index][0] < slices[index - 1][1]) {
      throw new Error('Universal Mach-O architecture slices overlap')
    }
  }
  if (!cpuTypes.has(CPU_TYPE_X86_64) || !cpuTypes.has(CPU_TYPE_ARM64)) {
    throw new Error('Engine must contain both arm64 and x86_64 Mach-O slices')
  }
}

export async function installArchiveBuffer(catalogValue, archiveBuffer, cacheRoot) {
  const catalog = validateCatalog(catalogValue)
  verifyBuffer(archiveBuffer, catalog.source.sha256, catalog.source.archive_size, 'archive')
  const destination = join(cacheRoot, catalog.id, catalog.version, 'macos-universal')
  if (await pathExists(destination)) {
    if (await verifyInstalledCatalog(catalog, destination)) return destination
    throw new Error(`Existing runtime asset cache is incomplete: ${destination}`)
  }

  const staging = `${destination}.staging-${randomUUID()}`
  await mkdir(staging, { recursive: true })
  try {
    await extractAllowedFiles(catalog, archiveBuffer, staging)
    await writeFile(join(staging, 'catalog.json'), `${JSON.stringify(catalog, null, 2)}\n`)
    await mkdir(dirname(destination), { recursive: true })
    await rename(staging, destination)
    return destination
  } catch (error) {
    await rm(staging, { recursive: true, force: true })
    throw error
  }
}

export async function fetchAndInstall(catalog, cacheRoot, fetchImplementation = globalThis.fetch) {
  const validated = validateCatalog(catalog)
  const destination = cacheDestination(validated, cacheRoot)
  if (await pathExists(destination)) {
    if (await verifyInstalledCatalog(validated, destination)) return destination
    throw new Error(`Existing runtime asset cache is incomplete: ${destination}`)
  }
  const response = await fetchImplementation(validated.source.url, { redirect: 'follow' })
  if (!response.ok) throw new Error(`Asset download failed with HTTP ${response.status}`)
  const declaredLength = response.headers?.get?.('content-length')
  if (declaredLength !== null && declaredLength !== undefined) {
    const parsedLength = Number(declaredLength)
    if (!Number.isSafeInteger(parsedLength) || parsedLength !== validated.source.archive_size) {
      throw new Error('Asset download Content-Length does not match the pinned archive size')
    }
  }
  const archive = await readResponseBody(response, validated.source.archive_size)
  return installArchiveBuffer(validated, archive, cacheRoot)
}

function cacheDestination(catalog, cacheRoot) {
  return join(cacheRoot, catalog.id, catalog.version, 'macos-universal')
}

async function extractAllowedFiles(catalog, archiveBuffer, destination) {
  const allowed = new Map(catalog.files.map((file) => [file.archive_path, file]))
  const extracted = new Set()
  await visitZipEntries(archiveBuffer, async (zip, entry) => {
    assertSafeRelativePath(entry.fileName.replace(/\/$/, ''))
    if (isSymbolicLink(entry)) throw new Error(`Symbolic link is forbidden: ${entry.fileName}`)
    if (entry.fileName.endsWith('/')) return
    if (extracted.has(entry.fileName)) throw new Error(`Duplicate archive entry: ${entry.fileName}`)
    extracted.add(entry.fileName)

    const expected = allowed.get(entry.fileName)
    if (!expected) return
    if (entry.uncompressedSize !== expected.size) {
      throw new Error(`Unexpected size for ${entry.fileName}`)
    }
    const contents = await readZipEntry(zip, entry)
    verifyBuffer(contents, expected.sha256, expected.size, entry.fileName)
    if (expected.kind === 'engine') validateUniversalMachO(contents)
    const output = resolve(destination, expected.destination.split('/').join(sep))
    if (!output.startsWith(`${resolve(destination)}${sep}`)) throw new Error('Unsafe destination path')
    await mkdir(dirname(output), { recursive: true })
    await writeFile(output, contents, { mode: expected.executable ? 0o755 : 0o644 })
    if (expected.executable) await chmod(output, 0o755)
  })

  const missing = catalog.files.filter((file) => !extracted.has(file.archive_path))
  if (missing.length > 0) throw new Error(`Archive is missing: ${missing.map((file) => file.archive_path).join(', ')}`)
}

async function verifyInstalledCatalog(catalog, destination) {
  try {
    const allowedFiles = new Set([
      'catalog.json',
      ...catalog.files.map((file) => file.destination),
    ])
    const installedFiles = await listInstalledFiles(destination)
    if (
      installedFiles.length !== allowedFiles.size ||
      installedFiles.some((file) => !allowedFiles.has(file))
    ) {
      return false
    }
    const installedCatalog = JSON.parse(await readFile(join(destination, 'catalog.json'), 'utf8'))
    if (JSON.stringify(installedCatalog) !== JSON.stringify(catalog)) return false

    for (const file of catalog.files) {
      const path = join(destination, ...file.destination.split('/'))
      const info = await lstat(path)
      if (!info.isFile() || info.size !== file.size) return false
      const contents = await readFile(path)
      verifyBuffer(contents, file.sha256, file.size, file.destination)
      if (file.kind === 'engine') validateUniversalMachO(contents)
    }
    for (const file of catalog.files.filter((file) => file.executable)) {
      await chmod(join(destination, ...file.destination.split('/')), 0o755)
    }
    return true
  } catch (error) {
    if (error?.code === 'ENOENT') return false
    throw error
  }
}

async function listInstalledFiles(root, directory = root) {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    const info = await lstat(path)
    if (info.isSymbolicLink()) throw new Error(`Symbolic link is forbidden in cache: ${path}`)
    if (info.isDirectory()) {
      files.push(...(await listInstalledFiles(root, path)))
    } else if (info.isFile()) {
      files.push(relative(root, path).split(sep).join('/'))
    } else {
      throw new Error(`Unsupported cache entry: ${path}`)
    }
  }
  return files.sort()
}

async function readResponseBody(response, expectedSize) {
  if (!response.body?.getReader) {
    const fallback = Buffer.from(await response.arrayBuffer())
    if (fallback.length > expectedSize) throw new Error('Asset download exceeded pinned size')
    return fallback
  }

  const reader = response.body.getReader()
  const chunks = []
  let length = 0
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      length += value.byteLength
      if (length > expectedSize) throw new Error('Asset download exceeded pinned size')
      chunks.push(Buffer.from(value))
    }
  } catch (error) {
    await reader.cancel(error).catch(() => {})
    throw error
  }
  return Buffer.concat(chunks, length)
}

async function pathExists(path) {
  try {
    await stat(path)
    return true
  } catch (error) {
    if (error?.code === 'ENOENT') return false
    throw error
  }
}

function verifyBuffer(buffer, expectedHash, expectedSize, label) {
  if (buffer.length !== expectedSize) throw new Error(`Unexpected size for ${label}`)
  const actualHash = createHash('sha256').update(buffer).digest('hex')
  if (actualHash !== expectedHash) throw new Error(`SHA-256 mismatch for ${label}`)
}

function visitZipEntries(buffer, visitor) {
  return new Promise((resolvePromise, reject) => {
    yauzl.fromBuffer(buffer, { lazyEntries: true, decodeStrings: true, strictFileNames: true, validateEntrySizes: true }, (openError, zip) => {
      if (openError) return reject(openError)
      let chain = Promise.resolve()
      let failed = false
      zip.on('entry', (entry) => {
        chain = chain.then(() => visitor(zip, entry)).then(() => zip.readEntry())
        chain.catch((error) => {
          if (!failed) {
            failed = true
            zip.close()
            reject(error)
          }
        })
      })
      zip.on('end', () => {
        if (!failed) chain.then(resolvePromise, reject)
      })
      zip.on('error', (error) => {
        if (!failed) reject(error)
      })
      zip.readEntry()
    })
  })
}

function readZipEntry(zip, entry) {
  return new Promise((resolvePromise, reject) => {
    zip.openReadStream(entry, (error, stream) => {
      if (error) return reject(error)
      const chunks = []
      stream.on('data', (chunk) => chunks.push(chunk))
      stream.on('error', reject)
      stream.on('end', () => resolvePromise(Buffer.concat(chunks)))
    })
  })
}
