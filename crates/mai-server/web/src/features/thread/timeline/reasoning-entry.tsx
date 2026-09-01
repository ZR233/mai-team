/**
 * 思考（reasoning）条目的折叠行：流式生成时默认展开，条目完成后收起为
 * 一行摘要（"Thought for 12s"），点击可随时展开或收起。
 *
 * 折叠是纯 UI 状态，归本组件所有；时长与完成态从条目时间戳推导。
 */

import { Brain, ChevronDown } from "lucide-react"
import { useEffect, useState } from "react"

import { Markdown } from "@/components/markdown"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { cn } from "@/lib/utils"
import { TimelineActivityRail, timelineActivityTriggerClass } from "./timeline-activity"
import type { ReasoningActivityGroup } from "./timeline-entries"

export function ReasoningEntry({ group }: { group: ReasoningActivityGroup }) {
  const [open, setOpen] = useState(group.active)

  // 仅在状态跳变为完成时收起一次；用户随后的手动展开不受影响。
  useEffect(() => {
    if (!group.active) setOpen(false)
  }, [group.active])

  const fallback = group.active ? "Thinking" : group.durationLabel ? `Thought for ${group.durationLabel}` : "Thought"
  const label = group.latestSummary
    ? `${group.latestSummary}${group.durationLabel ? ` · ${group.durationLabel}` : ""}`
    : fallback
  const text = group.items
    .flatMap((item) => [...(item.state.data.summary ?? []), ...(item.state.data.content ?? [])])
    .map((part) => part.trim())
    .filter(Boolean)
    .join("\n\n")

  return (
    <TimelineActivityRail>
      <Collapsible open={open} onOpenChange={setOpen}>
        <CollapsibleTrigger
          className={timelineActivityTriggerClass}
          aria-label={`${open ? "Collapse" : "Expand"} reasoning`}
        >
          <Brain className="size-3.5 shrink-0" data-icon="inline-start" aria-hidden="true" />
          <span className="shrink-0 font-medium text-foreground/80">Reasoning</span>
          <span className="min-w-0 flex-1 truncate text-xs">{label}</span>
          {group.active && <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-muted-foreground/70 motion-reduce:animate-none" aria-hidden="true" />}
          <ChevronDown
            className={cn("size-3.5 shrink-0 transition-transform motion-reduce:transition-none", open && "rotate-180")}
            data-icon="inline-end"
            aria-hidden="true"
          />
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div className="px-1.5 pt-1 pb-2">
            <Markdown variant="auxiliary">{text}</Markdown>
          </div>
        </CollapsibleContent>
      </Collapsible>
    </TimelineActivityRail>
  )
}
