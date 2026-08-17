/**
 * Zero-dependency streaming/buffer ZIP archiver (PKZIP 2.0 Store mode with UTF-8).
 *
 * Packs file entries into a valid .zip archive Blob for one-click download
 * of multi-file bundles in the browser.
 */

// CRC-32 table (polynomial 0xEDB88320)
const CRC32_TABLE = new Uint32Array(256)
for (let i = 0; i < 256; i++) {
  let c = i
  for (let k = 0; k < 8; k++) {
    c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
  }
  CRC32_TABLE[i] = c >>> 0
}

export function crc32(bytes: Uint8Array): number {
  let crc = 0xffffffff
  for (let i = 0; i < bytes.length; i++) {
    crc = CRC32_TABLE[(crc ^ bytes[i]) & 0xff] ^ (crc >>> 8)
  }
  return (crc ^ 0xffffffff) >>> 0
}

export interface ZipEntryInput {
  name: string
  data: Uint8Array
  mtime?: Date
}

function dateToDosTimeDate(d: Date): { dosTime: number; dosDate: number } {
  const year = d.getFullYear()
  const month = d.getMonth() + 1
  const day = d.getDate()
  const hours = d.getHours()
  const minutes = d.getMinutes()
  const seconds = Math.floor(d.getSeconds() / 2)

  const dosTime = (hours << 11) | (minutes << 5) | seconds
  const dosDate = ((Math.max(1980, year) - 1980) << 9) | (month << 5) | day
  return { dosTime, dosDate }
}

/**
 * Creates a standard .zip Blob from an array of file entries.
 * Normalizes backslashes to forward slashes for cross-platform archive compatibility.
 */
export function createZipBlob(entries: ZipEntryInput[]): Blob {
  const encoder = new TextEncoder()
  const localHeaders: Uint8Array[] = []
  const centralDirHeaders: Uint8Array[] = []
  let currentOffset = 0

  const now = new Date()
  const { dosTime, dosDate } = dateToDosTimeDate(now)

  for (const entry of entries) {
    // Normalize path separators to forward slash (ZIP standard)
    const cleanPath = entry.name.replace(/\\/g, "/")
    const encodedName = encoder.encode(cleanPath)
    const data = entry.data
    const entryCrc = crc32(data)
    const dataLen = data.length
    const nameLen = encodedName.length
    const localOffset = currentOffset

    // 1. Local File Header (30 bytes + name + data)
    const localHeader = new Uint8Array(30 + nameLen)
    const lView = new DataView(localHeader.buffer)
    lView.setUint32(0, 0x04034b50, true) // Local file header signature
    lView.setUint16(4, 20, true) // Version needed to extract (2.0)
    lView.setUint16(6, 0x0800, true) // General purpose bit flag (Bit 11: UTF-8)
    lView.setUint16(8, 0, true) // Compression method: 0 (Store)
    lView.setUint16(10, dosTime, true)
    lView.setUint16(12, dosDate, true)
    lView.setUint32(14, entryCrc, true)
    lView.setUint32(18, dataLen, true) // Compressed size
    lView.setUint32(22, dataLen, true) // Uncompressed size
    lView.setUint16(26, nameLen, true)
    lView.setUint16(28, 0, true) // Extra field length
    localHeader.set(encodedName, 30)

    localHeaders.push(localHeader)
    localHeaders.push(data)
    currentOffset += localHeader.length + dataLen

    // 2. Central Directory Header (46 bytes + name)
    const cdHeader = new Uint8Array(46 + nameLen)
    const cdView = new DataView(cdHeader.buffer)
    cdView.setUint32(0, 0x02014b50, true) // Central directory file header signature
    cdView.setUint16(4, 20, true) // Version made by (2.0)
    cdView.setUint16(6, 20, true) // Version needed (2.0)
    cdView.setUint16(8, 0x0800, true) // UTF-8 flag
    cdView.setUint16(10, 0, true) // Compression: Store
    cdView.setUint16(12, dosTime, true)
    cdView.setUint16(14, dosDate, true)
    cdView.setUint32(16, entryCrc, true)
    cdView.setUint32(20, dataLen, true)
    cdView.setUint32(24, dataLen, true)
    cdView.setUint16(28, nameLen, true)
    cdView.setUint16(30, 0, true) // Extra field length
    cdView.setUint16(32, 0, true) // File comment length
    cdView.setUint16(34, 0, true) // Disk number start
    cdView.setUint16(36, 0, true) // Internal file attributes
    cdView.setUint32(38, 0, true) // External file attributes
    cdView.setUint32(42, localOffset, true) // Relative offset of local header
    cdHeader.set(encodedName, 46)

    centralDirHeaders.push(cdHeader)
  }

  const cdOffset = currentOffset
  let cdSize = 0
  for (const h of centralDirHeaders) {
    cdSize += h.length
  }

  // 3. End of Central Directory Record (22 bytes)
  const eocd = new Uint8Array(22)
  const eocdView = new DataView(eocd.buffer)
  eocdView.setUint32(0, 0x06054b50, true) // End of central dir signature
  eocdView.setUint16(4, 0, true) // Disk number
  eocdView.setUint16(6, 0, true) // Disk with CD
  eocdView.setUint16(8, entries.length, true) // Total entries on disk
  eocdView.setUint16(10, entries.length, true) // Total entries
  eocdView.setUint32(12, cdSize, true) // Size of central directory
  eocdView.setUint32(16, cdOffset, true) // Offset of start of central directory
  eocdView.setUint16(20, 0, true) // ZIP comment length

  const parts: BlobPart[] = [
    ...localHeaders.map((h) => h.slice().buffer as ArrayBuffer),
    ...centralDirHeaders.map((h) => h.slice().buffer as ArrayBuffer),
    eocd.slice().buffer as ArrayBuffer,
  ]
  return new Blob(parts, { type: "application/zip" })
}
