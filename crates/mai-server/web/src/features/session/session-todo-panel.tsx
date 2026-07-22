import {
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Circle,
  CircleDot,
  ListTodo,
  X,
} from "lucide-react"
import { useId, useState } from "react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import {
  Drawer,
  DrawerClose,
  DrawerContent,
  DrawerDescription,
  DrawerHeader,
  DrawerTitle,
  DrawerTrigger,
} from "@/components/ui/drawer"
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty"
import { Progress } from "@/components/ui/progress"
import { ScrollArea } from "@/components/ui/scroll-area"
import type { SessionTimelineEvent, TodoListSnapshot } from "@/events/session-events.generated"
import { cn } from "@/lib/utils"

export function latestTodoSnapshot(events: SessionTimelineEvent[]): TodoListSnapshot | null {
  let latestSequence = Number.NEGATIVE_INFINITY
  let latest: TodoListSnapshot | null = null

  for (const event of events) {
    if (event.kind.type === "todoListChanged" && event.sequence >= latestSequence) {
      latestSequence = event.sequence
      latest = event.kind.snapshot
    }
  }

  return latest
}

export function SessionTodoRail({
  snapshot,
  expanded,
  onExpandedChange,
}: {
  snapshot: TodoListSnapshot
  expanded: boolean
  onExpandedChange(expanded: boolean): void
}) {
  const contentId = useId()
  const progress = todoProgress(snapshot)

  return (
    <aside
      aria-label="Working list"
      className="sticky top-6 hidden self-start @min-[52rem]/session-workspace:block"
      data-session-todo-rail
    >
      <Collapsible open={expanded} onOpenChange={onExpandedChange}>
        {expanded ? (
          <Card size="sm" className="gap-0 py-0 shadow-sm">
            <CardHeader className="border-b py-3">
              <CardTitle className="flex items-center gap-2">
                <ListTodo className="size-4 text-muted-foreground" aria-hidden="true" />
                Working list
              </CardTitle>
              <CardDescription>{snapshot.explanation || "Current session tasks"}</CardDescription>
              <CardAction>
                <CollapsibleTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    aria-controls={contentId}
                    aria-expanded={expanded}
                    aria-label="Collapse working list"
                  >
                    <ChevronRight data-icon="inline-end" />
                  </Button>
                </CollapsibleTrigger>
              </CardAction>
              <TodoProgress progress={progress} className="col-span-full mt-2" />
            </CardHeader>
            <CollapsibleContent id={contentId}>
              <ScrollArea className="max-h-[calc(100svh-18rem)]">
                <CardContent className="py-3">
                  <TodoItems snapshot={snapshot} />
                </CardContent>
              </ScrollArea>
            </CollapsibleContent>
          </Card>
        ) : (
          <CollapsibleTrigger asChild>
            <Button
              variant="outline"
              className="h-auto w-12 flex-col gap-2 bg-background py-3 shadow-sm"
              aria-controls={contentId}
              aria-expanded={expanded}
              aria-label="Expand working list"
            >
              <ListTodo data-icon="inline-start" />
              <span className="text-xs tabular-nums">{progress.completed}/{progress.total}</span>
              <ChevronLeft data-icon="inline-end" />
            </Button>
          </CollapsibleTrigger>
        )}
      </Collapsible>
    </aside>
  )
}

export function SessionTodoDrawer({ snapshot }: { snapshot: TodoListSnapshot }) {
  const [open, setOpen] = useState(false)
  const progress = todoProgress(snapshot)

  return (
    <Drawer open={open} onOpenChange={setOpen}>
      <DrawerTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          className="absolute -top-12 right-3 rounded-full bg-background shadow-sm @min-[52rem]/session-workspace:hidden"
          aria-label="Open working list"
          aria-expanded={open}
        >
          <ListTodo data-icon="inline-start" />
          <span className="tabular-nums">{progress.completed}/{progress.total}</span>
        </Button>
      </DrawerTrigger>
      <DrawerContent className="max-h-[80svh] overflow-hidden">
        <DrawerHeader className="grid grid-cols-[minmax(0,1fr)_auto] text-left">
          <DrawerTitle>Working list</DrawerTitle>
          <DrawerClose asChild>
            <Button variant="ghost" size="icon-sm" className="row-span-2" aria-label="Close working list">
              <X data-icon="inline-end" />
            </Button>
          </DrawerClose>
          <DrawerDescription>{snapshot.explanation || "Current session tasks"}</DrawerDescription>
          <TodoProgress progress={progress} className="col-span-full mt-2" />
        </DrawerHeader>
        <ScrollArea className="min-h-0 flex-1 border-t px-4">
          <div className="py-4">
            <TodoItems snapshot={snapshot} />
          </div>
        </ScrollArea>
      </DrawerContent>
    </Drawer>
  )
}

interface TodoProgressValue {
  completed: number
  total: number
  percent: number
}

function todoProgress(snapshot: TodoListSnapshot): TodoProgressValue {
  const completed = snapshot.items.reduce((count, item) => count + Number(item.status === "completed"), 0)
  const total = snapshot.items.length
  return { completed, total, percent: total > 0 ? Math.round((completed / total) * 100) : 0 }
}

function TodoProgress({ progress, className }: { progress: TodoProgressValue; className?: string }) {
  return (
    <div className={cn("flex items-center gap-2", className)}>
      <Badge variant="secondary" className="tabular-nums">{progress.completed} of {progress.total}</Badge>
      <Progress
        value={progress.percent}
        aria-label={`${progress.completed} of ${progress.total} tasks completed`}
      />
    </div>
  )
}

function TodoItems({ snapshot }: { snapshot: TodoListSnapshot }) {
  if (snapshot.items.length === 0) {
    return (
      <Empty className="border-0 py-8">
        <EmptyHeader>
          <EmptyMedia variant="icon"><ListTodo /></EmptyMedia>
          <EmptyTitle>No planned steps</EmptyTitle>
          <EmptyDescription>The current working list is empty.</EmptyDescription>
        </EmptyHeader>
      </Empty>
    )
  }

  return (
    <ol className="flex flex-col gap-1.5">
      {snapshot.items.map((item, index) => {
        const inProgress = item.status === "inProgress"
        const completed = item.status === "completed"
        const Icon = completed ? CheckCircle2 : inProgress ? CircleDot : Circle
        const status = completed ? "Completed" : inProgress ? "In progress" : "Pending"
        return (
          <li
            key={`${item.step}:${index}`}
            className={cn(
              "flex items-start gap-2.5 rounded-lg px-2.5 py-2 text-sm",
              inProgress && "bg-muted",
            )}
          >
            <Icon
              className={cn(
                "mt-0.5 size-4 shrink-0 text-muted-foreground",
                inProgress && "animate-pulse text-foreground motion-reduce:animate-none",
                completed && "text-foreground",
              )}
              aria-hidden="true"
            />
            <span className={cn("min-w-0 flex-1 leading-5", completed && "text-muted-foreground line-through")}>{item.step}</span>
            <span className="sr-only">{status}</span>
          </li>
        )
      })}
    </ol>
  )
}
