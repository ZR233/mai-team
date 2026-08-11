import type { ReviewRunDetail } from "@/api/product-types"
import type { ThreadItem } from "@/events/thread-events.generated"

export type ReviewActivityItem =
  | { kind: "threadItem"; id: string; item: ThreadItem }
  | { kind: "conclusion"; id: string; outcome?: string; reviewEvent?: string; summary?: string; error?: string }

export function buildReviewActivity(detail: ReviewRunDetail): ReviewActivityItem[] {
  const archived = (detail.history?.items ?? []).map((item) => ({
    kind: "threadItem" as const,
    id: item.id,
    item,
  }))
  return [...archived, {
    kind: "conclusion" as const,
    id: `conclusion:${detail.id}`,
    outcome: detail.outcome ?? undefined,
    reviewEvent: detail.review_event ?? undefined,
    summary: detail.summary ?? undefined,
    error: detail.error ?? undefined,
  }]
}
