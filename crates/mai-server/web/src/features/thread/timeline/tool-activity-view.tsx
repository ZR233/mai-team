/**
 * 工具调用的视图：单工具紧凑行（ToolActivityRow）与连续工具分组行
 * （ToolActivityGroupView）。默认收起，分组头部展示数量、时长与失败数。
 *
 * 组件只消费投影层产出的 ToolActivity / ToolActivityGroup 模型。
 */

import { CheckCircle2, ChevronDown, CircleDot, Download, FileOutput, FileSearch, FileText, GitPullRequest, NotebookPen, OctagonAlert, SquareTerminal, Wrench } from "lucide-react"
import { useState } from "react"

import { Badge } from "@/components/ui/badge"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Separator } from "@/components/ui/separator"
import { cn } from "@/lib/utils"

import { isFailedOutcome, type ToolActivity, type ToolOutcome } from "./tool-activity"
import { formatToolJson } from "./tool-json"
import type { ToolActivityGroup } from "./timeline-entries"
import { ToolSectionView } from "./tool-section-view"

export type ToolActivityRowVariant = "standalone" | "grouped"

export function ToolActivityRow({ activity, variant = "standalone" }: { activity: ToolActivity; variant?: ToolActivityRowVariant }) {
  const [open, setOpen] = useState(false)
  const failed = isFailedOutcome(activity.outcome)

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="w-full min-w-0 max-w-full overflow-hidden">
      <CollapsibleTrigger
        className="group flex min-h-11 w-full max-w-full items-center gap-2.5 overflow-hidden rounded-md px-1.5 text-left outline-none transition-colors hover:bg-muted/45 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
        aria-label={`${open ? "Collapse" : "Expand"} ${activity.title}`}
      >
        <span className={cn(
          "grid size-8 shrink-0 place-items-center rounded-md border bg-background text-muted-foreground",
          failed && "border-destructive/30 text-destructive",
        )}>
          <ToolIcon name={activity.title} className="size-3.5" aria-hidden="true" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-medium">{activity.title}</span>
          {activity.summary && <span className="block truncate text-xs text-muted-foreground">{activity.summary}</span>}
        </span>
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
        <div className={cn("space-y-3 border-t py-3", variant === "grouped" && "bg-muted/20 px-3")}>
          {activity.sections.map((section, index) => <ToolSectionView key={index} section={section} />)}
          {activity.artifacts.length > 0 && <ArtifactList artifacts={activity.artifacts} />}
          {(activity.rawArguments || activity.rawResult) && <RawToolData rawArguments={activity.rawArguments} rawResult={activity.rawResult} />}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

export function ToolActivityGroupView({ group }: { group: ToolActivityGroup }) {
  const [open, setOpen] = useState(false)

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="w-full min-w-0 max-w-full overflow-hidden" data-tool-activity-group>
      <CollapsibleTrigger
        className="group flex min-h-11 w-full max-w-full items-center gap-2.5 overflow-hidden rounded-md px-1.5 text-left outline-none transition-colors hover:bg-muted/45 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
        aria-label={`${open ? "Collapse" : "Expand"} ${group.countLabel}`}
      >
        <span className={cn(
          "grid size-8 shrink-0 place-items-center rounded-md border bg-background text-muted-foreground",
          group.failedCount > 0 && "border-destructive/30 text-destructive",
        )}>
          <Wrench className="size-3.5" aria-hidden="true" />
        </span>
        <span className="min-w-0 flex-1 truncate text-sm font-semibold">{group.countLabel}</span>
        {group.durationLabel && <span className="shrink-0 text-xs tabular-nums text-muted-foreground">{group.durationLabel}</span>}
        {group.failedCount > 0
          ? <OctagonAlert className="size-3.5 shrink-0 text-destructive" aria-hidden="true" />
          : group.active
            ? <CircleDot className="size-3.5 shrink-0 animate-pulse text-muted-foreground motion-reduce:animate-none" aria-hidden="true" />
            : null}
        <ChevronDown
          className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform motion-reduce:transition-none", open && "rotate-180")}
          aria-hidden="true"
        />
      </CollapsibleTrigger>
      <CollapsibleContent className="max-w-full overflow-hidden">
        <div className="relative ml-3 max-w-[calc(100%-0.75rem)] pl-3 sm:ml-5 sm:max-w-[calc(100%-1.25rem)] sm:pl-5">
          <Separator orientation="vertical" className="absolute top-0 left-0 h-full" />
          <div className="flex min-w-0 max-w-full flex-col overflow-hidden">
            {group.activities.map((activity, index) => (
              <div key={activity.id} className="min-w-0 max-w-full overflow-hidden">
                {index > 0 && <Separator />}
                <ToolActivityRow activity={activity} variant="grouped" />
              </div>
            ))}
          </div>
        </div>
      </CollapsibleContent>
    </Collapsible>
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
  if (outcome === "completed") return <CheckCircle2 className="size-3.5 shrink-0 text-muted-foreground" aria-hidden="true" />
  return null
}

function ArtifactList({ artifacts }: { artifacts: { id?: string; label: string }[] }) {
  return (
    <section className="space-y-1.5">
      <h5 className="text-xs font-medium text-muted-foreground">Artifacts</h5>
      <ul className="space-y-1">
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
        <div className="space-y-2 border-t p-2">
          {rawArguments && <RawDataBlock label="Arguments" value={rawArguments} />}
          {rawResult && <RawDataBlock label="Result" value={rawResult} />}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

function RawDataBlock({ label, value }: { label: string; value: string }) {
  return (
    <div className="space-y-1">
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
