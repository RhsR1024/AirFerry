package com.airferry.app.scan

/**
 * Process-wide mutex serializing the three flows that compete over §12
 * resume state (spill / ledger / entry-stage) and received content:
 *
 *  - startup purge ([com.airferry.app.scan.CacheCleanup.purgeOnAppStart]);
 *  - the file list's manual "清理断点";
 *  - recovery staging inside ScanActivity's §12 recovery task.
 *
 * The earlier `recoveryActive` flag alone was a check-then-act: a recovery
 * starting right after the check would still race the deletions. The flag
 * stays (it drives the UI toast), but the lock is the authority. Never take
 * it on the main thread — every taker runs on a background executor and
 * recovery can hold it for seconds while streaming a large spill.
 */
object TransferMaintenance {
    val lock: Any = Any()
}
