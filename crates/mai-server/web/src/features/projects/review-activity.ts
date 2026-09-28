import type { ReviewRunDetail } from "@/api/product-types"
import type { ThreadItem, Turn } from "@/events/thread-events.generated"

export interface ReviewConclusionView {
  outcome?: string
  reviewEvent?: string
  summary?: string
  error?: string
}

/** 评审尝试的活动归档：时间线条目 + 固定在末尾的结论。 */
export interface ReviewActivity {
  items: ThreadItem[]
  turn?: Turn
  status: ReviewRunDetail["status"]
  conclusion: ReviewConclusionView
}

export function buildReviewActivity(detail: ReviewRunDetail): ReviewActivity {
  return {
    items: detail.history?.items ?? [],
    turn: detail.history?.turn,
    status: detail.status,
    conclusion: {
      outcome: detail.outcome ?? undefined,
      reviewEvent: detail.review_event ?? undefined,
      summary: detail.summary ?? undefined,
      error: detail.error ?? undefined,
    },
  }
}
