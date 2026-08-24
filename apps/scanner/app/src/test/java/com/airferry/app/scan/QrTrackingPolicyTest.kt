package com.airferry.app.scan

import org.junit.Assert.assertEquals
import org.junit.Test

class QrTrackingPolicyTest {
    @Test
    fun completeRoiHitCancelsPartialFullScanAging() {
        var streak = 1 // periodic full scan saw only 3/4
        streak = QrTrackingPolicy.shrinkStreakAfterTrackedResult(streak, 4, 4)
        assertEquals(0, streak)

        streak += 1
        streak = QrTrackingPolicy.shrinkStreakAfterTrackedResult(streak, 3, 4)
        assertEquals(1, streak)
    }
}
