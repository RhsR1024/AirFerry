#!/usr/bin/env node
import { createHash } from "node:crypto"
import { existsSync, readFileSync, writeFileSync } from "node:fs"
import path from "node:path"
import { fileURLToPath } from "node:url"

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const inputs = [
  "core/zxing-decoder/CMakeLists.txt",
  "core/zxing-decoder/zxing-source.cmake",
  "core/zxing-decoder/airferry_zxing_core.h",
  "core/zxing-decoder/airferry_zxing_core.cpp",
  "core/zxing-decoder/zxing_wasm.cpp",
  "core/zxing-decoder/link-wasm.sh",
]

function fingerprint() {
  const hash = createHash("sha256")
  for (const relative of inputs) {
    const bytes = readFileSync(path.join(root, relative))
    hash.update(`${relative}\0${bytes.length}\0`)
    hash.update(bytes)
  }
  return hash.digest("hex")
}

const command = process.argv[2] || "print"
const outputDir = process.argv[3] ? path.resolve(process.argv[3]) : null
const expected = fingerprint()
if (command === "print") {
  console.log(expected)
} else if (command === "write" && outputDir) {
  writeFileSync(path.join(outputDir, "SOURCE.sha256"), `${expected}\n`, { mode: 0o644 })
} else if (command === "check" && outputDir) {
  const stamp = path.join(outputDir, "SOURCE.sha256")
  const actual = existsSync(stamp) ? readFileSync(stamp, "utf8").trim() : ""
  if (actual !== expected) {
    console.error("FAST ZXing artifact is stale or unstamped; rebuild with scripts/build-fastzxing.sh")
    process.exit(1)
  }
} else {
  console.error("usage: fastzxing-fingerprint.mjs [print|write DIR|check DIR]")
  process.exit(2)
}
