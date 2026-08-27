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
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger
        className="group flex min-h-9 w-full items-center gap-2 rounded-md px-1.5 text-left text-sm text-muted-foreground outline-none transition-colors hover:bg-muted/45 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
        aria-label={`${open ? "Collapse" : "Expand"} reasoning`}
      >
        <Brain className="size-4 shrink-0" aria-hidden="true" />
        <span className="min-w-0 flex-1 truncate font-medium">{label}</span>
        {group.active && <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-muted-foreground/70 motion-reduce:animate-none" aria-hidden="true" />}
        <ChevronDown
          className={cn("size-3.5 shrink-0 transition-transform motion-reduce:transition-none", open && "rotate-180")}
          aria-hidden="true"
        />
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="border-l-2 py-1 pr-2 pl-3 text-muted-foreground">
          <Markdown>{text}</Markdown>
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}
