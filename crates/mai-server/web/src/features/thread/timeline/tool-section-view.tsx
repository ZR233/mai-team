/**
 * 工具分区的渲染：事实列表、代码/文本/Markdown 内容、文件列表与搜索匹配。
 *
 * 只做哑渲染，按 ToolSection.kind 穷尽分发；长内容默认截断预览，
 * 用户点击展开完整内容。
 */

import { FileSearch } from "lucide-react"
import { useState } from "react"

import { Markdown } from "@/components/markdown"
import { Button } from "@/components/ui/button"
import { ScrollArea } from "@/components/ui/scroll-area"
import { cn } from "@/lib/utils"

import type { ToolSection } from "./tool-sections"

const CONTENT_PREVIEW_LENGTH = 1_600

export function ToolSectionView({ section }: { section: ToolSection }) {
  switch (section.kind) {
    case "fields":
      return <FactList title={section.title} items={section.items} />
    case "list":
      return (
        <section className="space-y-1.5">
          <h5 className="text-xs font-medium text-muted-foreground">{section.title}</h5>
          <ul className="space-y-1 rounded-md border bg-muted/30 p-2.5 font-mono text-xs break-all">
            {section.items.map((item, index) => <li key={index}>{item}</li>)}
          </ul>
        </section>
      )
    case "matches":
      return (
        <section className="space-y-1.5">
          <h5 className="text-xs font-medium text-muted-foreground">{section.title}</h5>
          <div className="divide-y rounded-md border">
            {section.items.map((match, index) => (
              <div key={index} className="space-y-0.5 p-2">
                <code className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <FileSearch className="size-3 shrink-0" aria-hidden="true" />
                  {matchLocation(match)}
                </code>
                <p className="font-mono text-xs break-all">{match.text}</p>
              </div>
            ))}
          </div>
        </section>
      )
    case "markdown":
      return <ExpandableContent title={section.title} text={section.text} render={(value) => <Markdown>{value}</Markdown>} />
    case "code":
      return <ExpandableContent title={section.title} text={section.text} render={(value) => <pre className="font-mono text-xs whitespace-pre-wrap">{value}</pre>} />
    case "text":
      return <ExpandableContent title={section.title} text={section.text} render={(value) => <p className="text-xs">{value}</p>} />
  }
}

function matchLocation(match: { path?: string; line?: number; column?: number }): string {
  return [match.path, match.line, match.column].filter((part) => part !== undefined).join(":") || "Match"
}

function FactList({ title, items }: { title: string; items: { label: string; value: string }[] }) {
  return (
    <section className="space-y-1.5">
      <h5 className="text-xs font-medium text-muted-foreground">{title}</h5>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 rounded-md border bg-muted/45 p-2.5 text-xs">
        {items.map((item, index) => (
          <div key={index} className="col-span-2 grid grid-cols-subgrid">
            <dt className="text-muted-foreground">{item.label}</dt>
            <dd className="font-mono break-all">{item.value}</dd>
          </div>
        ))}
      </dl>
    </section>
  )
}

function ExpandableContent({ title, text, render }: { title: string; text: string; render: (value: string) => React.ReactNode }) {
  const [expanded, setExpanded] = useState(false)
  const preview = text.length > CONTENT_PREVIEW_LENGTH ? `${text.slice(0, CONTENT_PREVIEW_LENGTH)}\n…` : text
  const value = expanded ? text : preview

  return (
    <section className="space-y-1.5">
      <h5 className="text-xs font-medium text-muted-foreground">{title}</h5>
      <ScrollArea className={cn("rounded-md border", expanded ? "max-h-[32rem]" : "max-h-64")}>
        <div className="p-2.5">{render(value)}</div>
      </ScrollArea>
      {text.length > CONTENT_PREVIEW_LENGTH && (
        <Button variant="ghost" size="xs" className="h-6 px-2 text-xs" onClick={() => setExpanded(!expanded)}>
          {expanded ? "Show less" : "Show full content"}
        </Button>
      )}
    </section>
  )
}
