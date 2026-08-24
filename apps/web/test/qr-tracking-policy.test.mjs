import assert from "node:assert/strict"
import test from "node:test"

import { shrinkStreakAfterTrackedResult } from "../src/lib/qr-tracking-policy.ts"

test("complete ROI hits cancel partial full-scan aging", () => {
  let streak = 0
  streak += 1 // periodic full scan saw only 3/4
  streak = shrinkStreakAfterTrackedResult(streak, 4, 4)
  assert.equal(streak, 0)

  streak += 1
  streak = shrinkStreakAfterTrackedResult(streak, 3, 4)
  assert.equal(streak, 1)
})
