import { test } from "node:test"
import assert from "node:assert/strict"
import { createZipBlob, crc32 } from "../src/lib/zip.ts"

test("crc32 calculates correct IEEE standard checksum", () => {
  const encoder = new TextEncoder()
  const data = encoder.encode("123456789")
  assert.equal(crc32(data), 0xcbf43926)
})

test("createZipBlob creates valid PKZIP buffer with local and central directory headers", async () => {
  const encoder = new TextEncoder()
  const file1 = { name: "hello.txt", data: encoder.encode("Hello World!") }
  const file2 = { name: "sub/test.json", data: encoder.encode('{"key": "value"}') }

  const blob = createZipBlob([file1, file2])
  assert.equal(blob.type, "application/zip")
  assert.ok(blob.size > file1.data.length + file2.data.length)

  const buf = new Uint8Array(await blob.arrayBuffer())
  const view = new DataView(buf.buffer)

  // First local file header signature 0x04034b50
  assert.equal(view.getUint32(0, true), 0x04034b50)

  // End of Central Directory signature 0x06054b50 exists near the end
  const eocdSig = view.getUint32(buf.length - 22, true)
  assert.equal(eocdSig, 0x06054b50)
  // Total entries = 2
  assert.equal(view.getUint16(buf.length - 22 + 8, true), 2)
  assert.equal(view.getUint16(buf.length - 22 + 10, true), 2)
})
