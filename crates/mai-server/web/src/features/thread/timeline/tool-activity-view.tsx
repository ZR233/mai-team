/**
 * 工具调用的视图：单工具紧凑行（ToolActivityRow）与连续工具分组行
 * （ToolActivityGroupView）。默认收起，分组头部展示数量、时长与失败数。
 *
 * 组件只消费投影层产出的 ToolActivity / ToolActivityGroup 模型。
 */

import { ChevronDown, CircleDot, Download, FileOutput, FileSearch, FileText, GitPullRequest, NotebookPen, OctagonAlert, SquareTerminal, Wrench } from "lucide-react"
import { useState } from "react"

import { Badge } from "@/components/ui/badge"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Separator } from "@/components/ui/separator"
import { cn } from "@/lib/utils"

import { isFailedOutcome, type ToolActivity, type ToolOutcome } from "./tool-activity"
import { TimelineActivityRail, timelineActivityTriggerClass } from "./timeline-activity"
import { formatToolJson } from "./tool-json"
import type { ToolActivityGroup } from "./timeline-entries"
import { ToolSectionView } from "./tool-section-view"

export type ToolActivityRowVariant = "standalone" | "grouped"

export function ToolActivityRow({ activity, variant = "standalone" }: { activity: ToolActivity; variant?: ToolActivityRowVariant }) {
  const [open, setOpen] = useState(false)
  const failed = isFailedOutcome(activity.outcome)

  const content = (
    <Collapsible open={open} onOpenChange={setOpen} className="w-full min-w-0 max-w-full overflow-hidden">
      <CollapsibleTrigger
        className={timelineActivityTriggerClass}
        aria-label={`${open ? "Collapse" : "Expand"} ${activity.title}`}
      >
        <ToolIcon name={activity.title} className={cn("size-3.5 shrink-0", failed && "text-destructive")} aria-hidden="true" />
        <span className="shrink-0 font-medium text-foreground/80">{activity.title}</span>
        {activity.summary && <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{activity.summary}</span>}
        {!activity.summary && <span className="min-w-0 flex-1" />}
        {variant === "standalone" && <OutcomeBadge outcome={activity.outcome} />}
        <OutcomeIcon outcome={activity.outcome} />
        {variant === "standalone" && (
          <ChevronDown
            className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform motion-reduce:transition-none", open && "rotate-180")}
            aria-hidden="true"
          />
        )}
      </CollapsibleTrigger>
      <CollapsibleContent className="max-w-full overflow-hidden">
        <div className={cn("flex flex-col gap-3 rounded-lg bg-muted/25 p-3", variant === "grouped" && "rounded-none bg-muted/20")}>
          {activity.sections.map((section, index) => <ToolSectionView key={index} section={section} />)}
          {activity.artifacts.length > 0 && <ArtifactList artifacts={activity.artifacts} />}
          {(activity.rawArguments || activity.rawResult) && <RawToolData rawArguments={activity.rawArguments} rawResult={activity.rawResult} />}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )

  return variant === "standalone" ? <TimelineActivityRail>{content}</TimelineActivityRail> : content
}

export function ToolActivityGroupView({ group }: { group: ToolActivityGroup }) {
  const [open, setOpen] = useState(false)

  return (
    <TimelineActivityRail>
      <Collapsible open={open} onOpenChange={setOpen} className="w-full min-w-0 max-w-full overflow-hidden" data-tool-activity-group>
        <CollapsibleTrigger
          className={timelineActivityTriggerClass}
          aria-label={`${open ? "Collapse" : "Expand"} ${group.countLabel}`}
        >
          <Wrench className={cn("size-3.5 shrink-0", group.failedCount > 0 && "text-destructive")} data-icon="inline-start" aria-hidden="true" />
          <span className="shrink-0 font-medium text-foreground/80">Tools</span>
          <span className="min-w-0 flex-1 truncate text-xs">{group.countLabel}</span>
          {group.durationLabel && <span className="shrink-0 text-xs tabular-nums text-muted-foreground">{group.durationLabel}</span>}
          {group.failedCount > 0
            ? <OctagonAlert className="size-3.5 shrink-0 text-destructive" aria-hidden="true" />
            : group.active
              ? <CircleDot className="size-3.5 shrink-0 animate-pulse text-muted-foreground motion-reduce:animate-none" aria-hidden="true" />
              : null}
          <ChevronDown
            className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform motion-reduce:transition-none", open && "rotate-180")}
            data-icon="inline-end"
            aria-hidden="true"
          />
        </CollapsibleTrigger>
        <CollapsibleContent className="max-w-full overflow-hidden">
          <div className="ml-4 flex min-w-0 max-w-[calc(100%-1rem)] flex-col overflow-hidden border-l pl-3">
            {group.activities.map((activity, index) => (
              <div key={activity.id} className="min-w-0 max-w-full overflow-hidden">
                {index > 0 && <Separator />}
                <ToolActivityRow activity={activity} variant="grouped" />
              </div>
            ))}
          </div>
        </CollapsibleContent>
      </Collapsible>
    </TimelineActivityRail>
  )
}

function OutcomeBadge({ outcome }: { outcome: ToolOutcome }) {
  if (outcome === "active") return <Badge variant="secondary" className="shrink-0">running</Badge>
  if (outcome === "failed") return <Badge variant="destructive" className="shrink-0">failed</Badge>
  return null
}

function OutcomeIcon({ outcome }: { outcome: ToolOutcome }) {
  if (isFailedOutcome(outcome)) return <OctagonAlert className="size-3.5 shrink-0 text-destructive" aria-hidden="true" />
  if (outcome === "active") return <CircleDot className="size-3.5 shrink-0 animate-pulse text-muted-foreground motion-reduce:animate-none" aria-hidden="true" />
  if (outcome === "completed") return null
  return null
}

function ArtifactList({ artifacts }: { artifacts: { id?: string; label: string }[] }) {
  return (
    <section className="flex flex-col gap-1.5">
      <h5 className="text-xs font-medium text-muted-foreground">Artifacts</h5>
      <ul className="flex flex-col gap-1">
        {artifacts.map((artifact, index) => (
          <li key={index}>
            {artifact.id
              ? <a className="flex items-center gap-1.5 rounded-md border px-2 py-1.5 text-xs hover:bg-muted/45" href={`/artifacts/${encodeURIComponent(artifact.id)}/download`} download>
                  <FileOutput className="size-3.5 text-muted-foreground" aria-hidden="true" />
                  {artifact.label}
                  <Download className="ml-auto size-3 text-muted-foreground" aria-hidden="true" />
                </a>
              : <div className="flex items-center gap-1.5 rounded-md border px-2 py-1.5 text-xs text-muted-foreground">
                  <FileOutput className="size-3.5" aria-hidden="true" />
                  {artifact.label}
                </div>}
          </li>
        ))}
      </ul>
    </section>
  )
}

function RawToolData({ rawArguments, rawResult }: { rawArguments: string | null; rawResult: string | null }) {
  return (
    <Collapsible className="rounded-md border border-dashed">
      <CollapsibleTrigger className="px-3 py-2 text-xs font-medium text-muted-foreground hover:text-foreground">
        Original data
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="flex flex-col gap-2 border-t p-2">
          {rawArguments && <RawDataBlock label="Arguments" value={rawArguments} />}
          {rawResult && <RawDataBlock label="Result" value={rawResult} />}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

function RawDataBlock({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col gap-1">
      <p className="text-xs text-muted-foreground">{label}</p>
      <ScrollArea className="max-h-48 rounded-md border">
        <pre className="bg-foreground p-2.5 font-mono text-xs break-all whitespace-pre-wrap text-background">{formatToolJson(value)}</pre>
      </ScrollArea>
    </div>
  )
}

/** 按工具标题选择语义图标；标题由投影层根据工具名称生成。 */
function ToolIcon({ name, className }: { name: string; className?: string }) {
  const text = name.toLowerCase()
  if (text.includes("command") || text.includes("process")) return <SquareTerminal className={className} aria-hidden="true" />
  if (text.includes("note")) return <NotebookPen className={className} aria-hidden="true" />
  if (text.includes("search")) return <FileSearch className={className} aria-hidden="true" />
  if (text.includes("file") || text.includes("patch")) return <FileText className={className} aria-hidden="true" />
  if (text.includes("github")) return <GitPullRequest className={className} aria-hidden="true" />
  return <Wrench className={className} aria-hidden="true" />
}
