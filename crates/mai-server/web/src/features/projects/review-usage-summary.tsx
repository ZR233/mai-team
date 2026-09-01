import { Gauge } from "lucide-react"

import type { TokenUsage } from "@/api/product-types"
import { Badge } from "@/components/ui/badge"
import { Progress } from "@/components/ui/progress"

import { cacheHitRate } from "./review-job-model"

export function ReviewUsageSummary({ usage, active }: { usage: TokenUsage | null; active: boolean }) {
  const hitRate = cacheHitRate(usage)
  if (!usage) {
    return (
      <section aria-label="Token usage" className="flex items-start gap-2 border-y py-3">
        <Gauge className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
        <div className="flex flex-col gap-0.5">
          <h3 className="text-sm font-medium">Token usage</h3>
          <p className="text-xs text-muted-foreground">{active ? "Usage will appear after the active attempt finishes." : "No token usage was recorded for this review."}</p>
        </div>
      </section>
    )
  }

  const hitRateLabel = formatCacheHitRate(hitRate)
  return <section aria-label="Token usage" className="flex flex-col gap-3 border-y py-3">
    <div className="flex items-start gap-2">
      <Gauge className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
      <div className="flex flex-col gap-0.5">
        <h3 className="text-sm font-medium">Token usage</h3>
        <p className="text-xs text-muted-foreground">{active ? "Usage from completed attempts; the active attempt appears after it finishes." : "Combined usage across this logical review and its retries."}</p>
      </div>
    </div>
    <div className="flex flex-col gap-3 pl-6">
      <dl className="grid grid-cols-2 gap-3">
        <UsageMetric label="Total tokens" value={formatCompactTokens(usage.totalTokens)} title={formatExactTokens(usage.totalTokens)} />
        <UsageMetric label="Cache hit" value={hitRateLabel} />
      </dl>
      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between gap-3 text-xs text-muted-foreground">
          <span>Cached input</span>
          <Badge variant="secondary">{hitRateLabel}</Badge>
        </div>
        <Progress
          value={hitRate ?? 0}
          aria-label={`Cache hit rate ${hitRateLabel}`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={hitRate ?? 0}
        />
      </div>
      <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-4">
        <UsageDetail label="Input" value={usage.promptTokens} />
        <UsageDetail label="Cached input" value={usage.cachedPromptTokens} />
        <UsageDetail label="Output" value={usage.completionTokens} />
        <UsageDetail label="Reasoning" value={usage.reasoningTokens} />
      </dl>
    </div>
  </section>
}

export function ReviewAttemptUsage({ usage }: { usage?: TokenUsage }) {
  if (!usage) return null
  return <span className="mt-1 flex flex-wrap items-center gap-x-1 text-xs text-muted-foreground">
    <span title={formatExactTokens(usage.totalTokens)}>{formatCompactTokens(usage.totalTokens)} tokens</span>
    <span aria-hidden>·</span>
    <span>{formatCacheHitRate(cacheHitRate(usage))} cache</span>
  </span>
}

function UsageMetric({ label, value, title }: { label: string; value: string; title?: string }) {
  return <div className="min-w-0">
    <dt className="text-xs text-muted-foreground">{label}</dt>
    <dd className="mt-0.5 truncate text-base font-semibold tabular-nums" title={title}>{value}</dd>
  </div>
}

function UsageDetail({ label, value }: { label: string; value: number }) {
  return <div className="min-w-0">
    <dt className="text-muted-foreground">{label}</dt>
    <dd className="truncate font-medium tabular-nums" title={formatExactTokens(value)}>{formatExactNumber(value)}</dd>
  </div>
}

function formatCompactTokens(value: number) {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 2 }).format(value)
}

function formatExactTokens(value: number) {
  return `${formatExactNumber(value)} tokens`
}

function formatExactNumber(value: number) {
  return new Intl.NumberFormat().format(value)
}

function formatCacheHitRate(value: number | null) {
  return value === null ? "—" : `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(value)}%`
}
