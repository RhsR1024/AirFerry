/**
 * Preserve the periodic full-scan shrink streak only while tracked ROI results
 * do not prove that every known code is still alive.
 */
export function shrinkStreakAfterTrackedResult(
  current: number,
  resultCount: number,
  lockedCount: number
): number {
  return lockedCount > 0 && resultCount >= lockedCount ? 0 : current
}
