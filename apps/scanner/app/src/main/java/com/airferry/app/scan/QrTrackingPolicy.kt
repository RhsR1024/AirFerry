package com.airferry.app.scan

/** Pure tracking-state rule, shared with JVM regression tests. */
internal object QrTrackingPolicy {
    fun shrinkStreakAfterTrackedResult(
        current: Int,
        resultCount: Int,
        lockedCount: Int,
    ): Int = if (lockedCount > 0 && resultCount >= lockedCount) 0 else current
}
