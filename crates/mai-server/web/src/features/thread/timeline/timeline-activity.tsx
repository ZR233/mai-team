/**
 * 时间线次要活动的统一视觉边界。
 *
 * 推理、工具、技能、计划与过程说明都挂在同一条执行轨上，避免它们与
 * 用户消息和最终回复争夺视觉层级。组件只负责布局与交互外观，不拥有
 * 任何协议状态或折叠状态。
 */

import type { ComponentProps } from "react"

import { cn } from "@/lib/utils"

export function TimelineActivityRail({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-timeline-entry="activity"
      className={cn("relative ml-2 min-w-0 border-l border-border/80 py-0.5 pl-4", className)}
      {...props}
    />
  )
}

export const timelineActivityTriggerClass =
  "group flex min-h-9 w-full min-w-0 items-center gap-2 rounded-md px-1.5 text-left text-sm text-muted-foreground outline-none transition-colors hover:bg-muted/45 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
