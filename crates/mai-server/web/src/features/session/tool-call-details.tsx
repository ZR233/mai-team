import {
  CheckCircle2,
  ChevronDown,
  CircleDot,
  Download,
  FileOutput,
  FileSearch,
  FileText,
  GitPullRequest,
  NotebookPen,
  OctagonAlert,
  SquareTerminal,
  Wrench,
} from "lucide-react"
import { type ComponentType, useMemo, useState } from "react"

import { Markdown } from "@/components/markdown"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { ScrollArea } from "@/components/ui/scroll-area"
import { cn } from "@/lib/utils"

import {
  buildToolPresentation,
  formatRawToolData,
  type ToolPresentationInput,
  type ToolSection,
} from "./tool-presentation"

const CONTENT_PREVIEW_LENGTH = 1_600

export function ToolCallDetails({ tool, defaultOpen = false }: { tool: ToolPresentationInput; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen)
  const [rawOpen, setRawOpen] = useState(false)
  const model = useMemo(() => buildToolPresentation(tool), [tool])
  const Icon = toolIcon(model.name)
  const running = ["started", "streaming", "running", "awaiting approval"].includes(model.status)

  return (
    <Collapsible open={open} onOpenChange={setOpen} className={cn(
      "overflow-hidden rounded-lg border bg-card",
      model.failed && "border-destructive/30",
    )}>
      <CollapsibleTrigger
        className="group flex w-full items-center gap-2.5 px-3 py-2.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
        aria-label={`${open ? "Collapse" : "Expand"} ${model.title}`}
      >
        <span className={cn(
          "grid size-7 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground",
          model.failed && "bg-destructive/10 text-destructive",
        )}>
          <Icon className="size-3.5" aria-hidden="true" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-xs font-semibold">{model.title}</span>
          <span className="block truncate text-xs text-muted-foreground">{model.summary}</span>
        </span>
        {model.facts.find((fact) => fact.label === "Exit code") && (
          <span className="hidden text-xs tabular-nums text-muted-foreground sm:inline">
            exit {model.facts.find((fact) => fact.label === "Exit code")?.value}
          </span>
        )}
        <StatusBadge status={model.status} failed={model.failed} />
        {model.failed
          ? <OctagonAlert className="size-3.5 text-destructive" aria-hidden="true" />
          : running
            ? <CircleDot className="size-3.5 animate-pulse text-muted-foreground motion-reduce:animate-none" aria-hidden="true" />
            : <CheckCircle2 className="size-3.5 text-muted-foreground" aria-hidden="true" />}
        <ChevronDown className={cn("size-3.5 text-muted-foreground transition-transform motion-reduce:transition-none", open && "rotate-180")} aria-hidden="true" />
      </CollapsibleTrigger>
      <CollapsibleContent className="border-t">
        <div className="space-y-4 p-3 sm:p-4">
          {model.sections.map((section, index) => <ToolSectionView key={`${section.title}:${index}`} section={section} />)}
          {model.outputArtifacts.length > 0 && (
            <section className="space-y-2">
              <SectionTitle>Attachments</SectionTitle>
              <div className="space-y-1.5">{model.outputArtifacts.map((artifact, index) => (
                <ToolArtifact key={`${artifact.id || artifact.path || "artifact"}:${index}`} artifact={artifact} />
              ))}</div>
            </section>
          )}
          {(model.rawArguments || model.rawResult) && (
            <Collapsible open={rawOpen} onOpenChange={setRawOpen} className="rounded-md border border-dashed">
              <CollapsibleTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="w-full justify-between rounded-md px-2.5 text-xs text-muted-foreground"
                  aria-label={`${rawOpen ? "Hide" : "Show"} original tool data`}
                >
                  Original data
                  <ChevronDown className={cn("size-3.5 transition-transform motion-reduce:transition-none", rawOpen && "rotate-180")} aria-hidden="true" />
                </Button>
              </CollapsibleTrigger>
              <CollapsibleContent>
                {rawOpen && <div className="space-y-3 border-t border-dashed p-3">
                  {model.rawArguments && <RawData title="Arguments" value={model.rawArguments} />}
                  {model.rawResult && <RawData title="Result" value={model.rawResult} />}
                </div>}
              </CollapsibleContent>
            </Collapsible>
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  )
}

function ToolSectionView({ section }: { section: ToolSection }) {
  switch (section.kind) {
    case "fields":
      return <FactList title={section.title} items={section.items} />
    case "list":
      return <section className="space-y-2"><SectionTitle>{section.title}</SectionTitle><ul className="space-y-1 rounded-md bg-muted/45 p-2.5 font-mono text-xs">{section.items.map((item, index) => <li key={`${item}:${index}`} className="break-all">{item}</li>)}</ul></section>
    case "matches":
      return <section className="space-y-2"><SectionTitle>{section.title}</SectionTitle><div className="divide-y rounded-md border">{section.items.map((item, index) => <div key={`${item.path}:${item.line}:${index}`} className="grid gap-1 p-2.5 text-xs sm:grid-cols-[minmax(7rem,auto)_1fr]"><code className="break-all text-muted-foreground">{matchLocation(item.path, item.line, item.column)}</code><span className="break-words font-mono">{item.text}</span></div>)}</div></section>
    case "markdown":
      return <ExpandableContent title={section.title} text={section.text} markdown />
    case "code":
      return <ExpandableContent title={section.title} text={section.text} code />
    case "text":
      return <ExpandableContent title={section.title} text={section.text} />
  }
}

function ExpandableContent({ title, text, code = false, markdown = false }: { title: string; text: string; code?: boolean; markdown?: boolean }) {
  const [expanded, setExpanded] = useState(false)
  const long = text.length > CONTENT_PREVIEW_LENGTH
  const visible = long && !expanded ? `${text.slice(0, CONTENT_PREVIEW_LENGTH)}\n…` : text
  return (
    <section className="space-y-2">
      <div className="flex items-center justify-between gap-3">
        <SectionTitle>{title}</SectionTitle>
        {long && <Button variant="ghost" size="xs" onClick={() => setExpanded((value) => !value)} aria-expanded={expanded}>{expanded ? "Show less" : "Show full content"}</Button>}
      </div>
      <ScrollArea className={cn("rounded-md", expanded ? "max-h-[32rem]" : "max-h-64")}>
        {markdown
          ? <div className="bg-muted/45 p-3"><Markdown>{visible}</Markdown></div>
          : code
            ? <pre className="whitespace-pre-wrap break-words bg-muted/60 p-3 font-mono text-xs leading-5 text-foreground">{visible}</pre>
            : <p className="whitespace-pre-wrap break-words rounded-md bg-muted/45 p-3 text-sm">{visible}</p>}
      </ScrollArea>
    </section>
  )
}

function FactList({ title, items }: { title: string; items: Array<{ label: string; value: string }> }) {
  return (
    <section className="space-y-2">
      <SectionTitle>{title}</SectionTitle>
      <dl className="grid gap-x-5 gap-y-2 rounded-md bg-muted/45 p-2.5 sm:grid-cols-2">
        {items.map((item) => <div key={`${item.label}:${item.value}`} className="min-w-0"><dt className="text-[11px] text-muted-foreground">{item.label}</dt><dd className="break-words font-mono text-xs">{item.value}</dd></div>)}
      </dl>
    </section>
  )
}

function RawData({ title, value }: { title: string; value: string }) {
  return <section className="space-y-1.5"><SectionTitle>{title}</SectionTitle><ScrollArea className="max-h-72 rounded-md"><pre className="whitespace-pre-wrap break-words bg-foreground p-3 font-mono text-xs text-background">{formatRawToolData(value)}</pre></ScrollArea></section>
}

function ToolArtifact({ artifact }: { artifact: Record<string, unknown> }) {
  const id = typeof artifact.id === "string" ? artifact.id : null
  const label = String(artifact.name || artifact.path || artifact.filename || "Tool output")
  const content = <><FileOutput className="size-3.5" /><span className="min-w-0 flex-1 truncate">{label}</span>{id && <Download className="size-3.5" />}</>
  return id
    ? <a href={`/artifacts/${encodeURIComponent(id)}/download`} download className="flex items-center gap-2 rounded-md border px-2.5 py-2 text-xs hover:bg-muted">{content}</a>
    : <div className="flex items-center gap-2 rounded-md border px-2.5 py-2 text-xs text-muted-foreground">{content}</div>
}

function StatusBadge({ status, failed }: { status: string; failed: boolean }) {
  return <Badge variant={failed ? "destructive" : status === "completed" || status === "success" ? "outline" : "secondary"} className="hidden capitalize sm:inline-flex">{status}</Badge>
}

function SectionTitle({ children }: { children: string }) {
  return <h4 className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{children}</h4>
}

function matchLocation(path: string | undefined, line: number | undefined, column: number | undefined) {
  return [path, line, column].filter((value) => value !== undefined && value !== "").join(":") || "Match"
}

function toolIcon(name: string): ComponentType<{ className?: string; "aria-hidden"?: boolean | "true" | "false" }> {
  if (name === "exec" || name === "write_stdin") return SquareTerminal
  if (name.includes("session_note")) return NotebookPen
  if (name.includes("search")) return FileSearch
  if (name.includes("file") || name === "apply_patch") return FileText
  if (name.includes("github")) return GitPullRequest
  return Wrench
}
