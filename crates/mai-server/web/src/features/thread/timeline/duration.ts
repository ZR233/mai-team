/**
 * 时间轴条目的时长格式化。
 *
 * 供工具分组行（"Used 4 tools · 18s"）与思考行（"Thought for 18s"）
 * 共用；输入为毫秒时间戳，输出短时长文案。
 */

export function formatDuration(startedAt?: number, endedAt?: number): string | null {
  if (startedAt === undefined || endedAt === undefined) return null
  const durationMs = endedAt - startedAt
  if (!Number.isFinite(durationMs) || durationMs < 0) return null
  if (durationMs < 1_000) return "<1s"

  const totalSeconds = Math.floor(durationMs / 1_000)
  if (totalSeconds < 60) return `${totalSeconds}s`
  const minutes = Math.floor(totalSeconds / 60)
  const seconds = totalSeconds % 60
  return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`
}
