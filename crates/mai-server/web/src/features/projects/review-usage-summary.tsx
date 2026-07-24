import { Gauge } from "lucide-react"

import type { TokenUsage } from "@/api/product-types"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Progress } from "@/components/ui/progress"

import { cacheHitRate } from "./review-job-model"

export function ReviewUsageSummary({ usage, active }: { usage: TokenUsage | null; active: boolean }) {
  const hitRate = cacheHitRate(usage)
  if (!usage) {
    return <Card size="sm">
      <CardHeader>
        <CardTitle className="flex items-center gap-2"><Gauge className="size-4 text-muted-foreground" aria-hidden />Token usage</CardTitle>
        <CardDescription>{active ? "Usage will appear after the active attempt finishes." : "No token usage was recorded for this review."}</CardDescription>
      </CardHeader>
    </Card>
  }

  const hitRateLabel = formatCacheHitRate(hitRate)
  return <Card size="sm">
    <CardHeader>
      <CardTitle className="flex items-center gap-2"><Gauge className="size-4 text-muted-foreground" aria-hidden />Token usage</CardTitle>
      <CardDescription>{active ? "Usage from completed attempts; the active attempt appears after it finishes." : "Combined usage across this logical review and its retries."}</CardDescription>
    </CardHeader>
    <CardContent className="flex flex-col gap-4">
      <dl className="grid grid-cols-2 gap-3">
        <UsageMetric label="Total tokens" value={formatCompactTokens(usage.total_tokens)} title={formatExactTokens(usage.total_tokens)} />
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
        <UsageDetail label="Input" value={usage.input_tokens} />
        <UsageDetail label="Cached input" value={usage.cached_input_tokens} />
        <UsageDetail label="Output" value={usage.output_tokens} />
        <UsageDetail label="Reasoning" value={usage.reasoning_output_tokens} />
      </dl>
    </CardContent>
  </Card>
}

export function ReviewAttemptUsage({ usage }: { usage?: TokenUsage }) {
  if (!usage) return null
  return <span className="mt-1 flex flex-wrap items-center gap-x-1 text-xs text-muted-foreground">
    <span title={formatExactTokens(usage.total_tokens)}>{formatCompactTokens(usage.total_tokens)} tokens</span>
    <span aria-hidden>·</span>
    <span>{formatCacheHitRate(cacheHitRate(usage))} cache</span>
  </span>
}

function UsageMetric({ label, value, title }: { label: string; value: string; title?: string }) {
  return <div className="min-w-0">
    <dt className="text-xs text-muted-foreground">{label}</dt>
    <dd className="mt-1 truncate text-xl font-semibold tabular-nums" title={title}>{value}</dd>
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
