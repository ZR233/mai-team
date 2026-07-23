import { ChevronDown, CircleDot, OctagonAlert, Wrench } from "lucide-react"
import { useState } from "react"

import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Separator } from "@/components/ui/separator"
import { cn } from "@/lib/utils"

import { ToolCallDetails } from "./tool-call-details"
import { buildToolPresentation } from "./tool-presentation"
import type { ToolSessionPart } from "./timeline-presentation"

const ACTIVE_STATUSES = new Set([
  "started",
  "streaming",
  "awaiting approval",
  "approved",
  "running",
])

export interface ToolActivityPresentation {
  title: string
  duration: string | null
  failedCount: number
  active: boolean
}

export function ToolActivityGroup({ parts }: { parts: ToolSessionPart[] }) {
  const [open, setOpen] = useState(false)
  const presentation = buildToolActivityPresentation(parts)

  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      className="w-full min-w-0 max-w-full overflow-hidden"
      data-tool-activity-group
    >
      <CollapsibleTrigger
        className="group flex min-h-11 w-full max-w-full items-center gap-2.5 overflow-hidden rounded-md px-1.5 text-left outline-none transition-colors hover:bg-muted/45 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
        aria-label={`${open ? "Collapse" : "Expand"} ${parts.length} tool calls`}
      >
        <span className={cn(
          "grid size-8 shrink-0 place-items-center rounded-md border bg-background text-muted-foreground",
          presentation.failedCount > 0 && "border-destructive/30 text-destructive",
        )}>
          <Wrench className="size-3.5" aria-hidden="true" />
        </span>
        <span className="min-w-0 flex-1 truncate text-sm font-semibold">{presentation.title}</span>
        {presentation.duration && (
          <span className="shrink-0 text-xs tabular-nums text-muted-foreground">{presentation.duration}</span>
        )}
        {presentation.failedCount > 0
          ? <OctagonAlert className="size-3.5 shrink-0 text-destructive" aria-hidden="true" />
          : presentation.active
            ? <CircleDot className="size-3.5 shrink-0 animate-pulse text-muted-foreground motion-reduce:animate-none" aria-hidden="true" />
            : null}
        <ChevronDown className={cn(
          "size-3.5 shrink-0 text-muted-foreground transition-transform motion-reduce:transition-none",
          open && "rotate-180",
        )} aria-hidden="true" />
      </CollapsibleTrigger>
      <CollapsibleContent className="max-w-full overflow-hidden">
        <div className="relative ml-3 max-w-[calc(100%-0.75rem)] pl-3 sm:ml-5 sm:max-w-[calc(100%-1.25rem)] sm:pl-5">
          <Separator orientation="vertical" className="absolute left-0 top-0 h-full" />
          <div className="flex min-w-0 max-w-full flex-col overflow-hidden">
            {parts.map((part, index) => (
              <div key={part.partId} className="min-w-0 max-w-full overflow-hidden">
                {index > 0 && <Separator />}
                <ToolCallDetails
                  variant="grouped"
                  tool={{ ...part.content.tool, status: part.status, error: part.error }}
                />
              </div>
            ))}
          </div>
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

export function buildToolActivityPresentation(parts: ToolSessionPart[]): ToolActivityPresentation {
  const models = parts.map((part) => buildToolPresentation({
    ...part.content.tool,
    status: part.status,
    error: part.error,
  }))
  const failedCount = models.filter((model) => model.failed).length
  const active = models.some((model) => ACTIVE_STATUSES.has(model.status))
  const countLabel = `${parts.length} tool${parts.length === 1 ? "" : "s"}`
  const title = `${active ? "Using" : "Used"} ${countLabel}${failedCount > 0 ? ` · ${failedCount} failed` : ""}`

  return {
    title,
    duration: active ? null : activityDuration(parts),
    failedCount,
    active,
  }
}

function activityDuration(parts: ToolSessionPart[]): string | null {
  if (parts.length === 0) return null
  const startedAt = Math.min(...parts.map((part) => part.createdAt))
  const completedAt = Math.max(...parts.map((part) => part.completedAt ?? part.updatedAt))
  const durationMs = completedAt - startedAt
  if (!Number.isFinite(durationMs) || durationMs < 0) return null
  if (durationMs < 1_000) return "<1s"

  const totalSeconds = Math.floor(durationMs / 1_000)
  if (totalSeconds < 60) return `${totalSeconds}s`
  const minutes = Math.floor(totalSeconds / 60)
  const seconds = totalSeconds % 60
  return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`
}
