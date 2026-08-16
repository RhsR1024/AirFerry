#!/usr/bin/env node
/**
 * Release artifact verification gate.
 *
 * Verifies all published artifacts match project invariants:
 *   - version across all files matches Cargo.toml (delegates to version.mjs)
 *   - release upload list does NOT contain private keys (*.pem, *.keystore)
 *   - standalone sender HTML is built and self-contained
 *
 * Any failure exits non-zero. This gate must never swallow errors: a failed
 * sub-check is a failed gate, not a "skipped safely".
 *
 * Usage:
 *   node scripts/verify-dist.mjs
 */
import { execFileSync } from "node:child_process"
import { readFileSync, existsSync } from "node:fs"
import path from "node:path"
import { fileURLToPath } from "node:url"

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")

console.log("▶ 1. Version gate check")
// Delegate to the real gate (root Cargo.toml [workspace.package].version is
// the single source of truth; version.mjs checks all 6 declared sites).
try {
  execFileSync(process.execPath, ["scripts/version.mjs", "check"], {
    cwd: root,
    encoding: "utf8",
    stdio: "inherit",
  })
} catch {
  console.error("✗ version gate failed (see output above)")
  process.exit(1)
}

console.log("▶ 2. Security gate check (dist-upload-list)")
// A non-zero exit from build-all.sh is a gate failure, not a pass.
let uploadList
try {
  const isWin = process.platform === "win32"
  const bin = isWin ? "bash" : "./scripts/build-all.sh"
  const args = isWin ? ["./scripts/build-all.sh", "dist-upload-list"] : ["dist-upload-list"]
  uploadList = execFileSync(bin, args, {
    cwd: root,
    encoding: "utf8",
  })
} catch (e) {
  console.error(`✗ dist-upload-list failed to run: ${e.shortMessage ?? e}`)
  process.exit(1)
}
for (const line of uploadList.split(/\s+/)) {
  if (line.endsWith(".pem") || line.endsWith(".keystore")) {
    console.error(`✗ CRITICAL: Secret/key file ${line} found in release upload list!`)
    process.exit(1)
  }
}
console.log("   release upload list is safe (keys excluded)")

console.log("▶ 3. Standalone HTML check")
const standaloneHtml = path.join(root, "apps/web/dist-standalone/index.html")
if (!existsSync(standaloneHtml)) {
  console.error(`✗ standalone HTML missing: ${standaloneHtml} (build it before verifying)`)
  process.exit(1)
}
const html = readFileSync(standaloneHtml, "utf8")
// Two independent conditions: the payload is present ("AirFerry"), and the
// page is self-contained (no external <script src=...> — everything inline).
// Note: `__AIRFERRY_STANDALONE__` legitimately survives as a runtime global
// flag (`globalThis.__AIRFERRY_STANDALONE__ = true`), so its presence is NOT
// a substitution failure.
if (!html.includes("AirFerry") || html.includes("<script src=")) {
  console.error("✗ Standalone HTML content invalid (payload missing or not self-contained)")
  process.exit(1)
}
console.log("   standalone HTML ok")

console.log("✅ All dist verification checks passed.")
