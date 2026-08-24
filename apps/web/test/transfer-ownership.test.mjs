import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import test from "node:test"

import initWasm, { encode_chunk_balanced } from "../wasm-pkg/transfer_engine.js"
import {
  consumeEncodedChunk,
  retainBytes,
  uniqueTransferBuffers,
} from "../src/lib/transfer-ownership.ts"

const wasmBytes = readFileSync(
  new URL("../wasm-pkg/transfer_engine_bg.wasm", import.meta.url)
)
await initWasm(wasmBytes)

test("consumeEncodedChunk consumes a real wasm handle exactly once", () => {
  const encoded = encode_chunk_balanced(
    new Uint8Array(1024).fill(7),
    100_000n,
    false
  )
  const result = consumeEncodedChunk(encoded)
  assert.equal(result.codec, 1)
  assert.ok(result.data.byteLength > 0)
})

test("retained hash survives transfer of the original hash buffer", () => {
  const original = new Uint8Array(32).fill(0xa5)
  const retained = retainBytes(original)
  structuredClone({ original }, { transfer: [original.buffer] })
  assert.equal(original.byteLength, 0)
  assert.equal(retained.byteLength, 32)
  assert.deepEqual([...retained], new Array(32).fill(0xa5))
})

test("transfer lists contain each ArrayBuffer at most once", () => {
  const shared = new Uint8Array(32)
  const transfers = uniqueTransferBuffers([shared, shared])
  assert.equal(transfers.length, 1)
  assert.doesNotThrow(() =>
    structuredClone({ first: shared, second: shared }, { transfer: transfers })
  )
})
