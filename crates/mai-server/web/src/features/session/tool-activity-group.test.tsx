import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import type { SessionPart } from "@/events/session-events.generated"

import { buildToolActivityPresentation, ToolActivityGroup } from "./tool-activity-group"
import type { ToolSessionPart } from "./timeline-presentation"

describe("tool activity group", () => {
  it("aggregates completed, active, and failed group status with final duration", () => {
    const completed = [
      toolPart("tool-1", "completed", 1_000, 7_000),
      toolPart("tool-2", "completed", 7_000, 19_000),
    ]
    const active = [
      toolPart("tool-1", "completed", 1_000, 7_000),
      toolPart("tool-2", "running", 7_000, 9_000),
    ]
    const failed = [
      toolPart("tool-1", "completed", 1_000, 7_000),
      toolPart("tool-2", "failed", 7_000, 19_000),
    ]
    const partiallyFailed = [
      toolPart("tool-1", "failed", 1_000, 7_000),
      toolPart("tool-2", "running", 7_000, 9_000),
    ]

    expect(buildToolActivityPresentation(completed)).toEqual({
      title: "Used 2 tools",
      duration: "18s",
      failedCount: 0,
      active: false,
    })
    expect(buildToolActivityPresentation(active)).toEqual({
      title: "Using 2 tools",
      duration: null,
      failedCount: 0,
      active: true,
    })
    expect(buildToolActivityPresentation(failed)).toEqual({
      title: "Used 2 tools · 1 failed",
      duration: "18s",
      failedCount: 1,
      active: false,
    })
    expect(buildToolActivityPresentation(partiallyFailed)).toEqual({
      title: "Using 2 tools · 1 failed",
      duration: null,
      failedCount: 1,
      active: true,
    })
  })

  it("starts collapsed, expands from the keyboard, and keeps child details independently accessible", async () => {
    const user = userEvent.setup()
    render(<ToolActivityGroup parts={[
      toolPart("tool-1", "completed", 1_000, 7_000, {
        name: "exec",
        arguments: '{"command":"npm run test","cwd":"/workspace"}',
        result: '{"status":"completed","exitCode":0,"stdout":"12 passed","stderr":""}',
      }),
      toolPart("tool-2", "completed", 7_000, 19_000, {
        name: "read_file",
        arguments: '{"path":"src/features/session/session-timeline.tsx"}',
        result: '{"path":"src/features/session/session-timeline.tsx","text":"export function SessionTimeline() {}"}',
      }),
    ]} />)

    const groupTrigger = screen.getByRole("button", { name: "Expand 2 tool calls" })
    expect(groupTrigger).toHaveAttribute("aria-expanded", "false")
    expect(screen.queryByText("Run command")).not.toBeInTheDocument()
    groupTrigger.focus()
    await user.keyboard("{Enter}")
    expect(screen.getByRole("button", { name: "Collapse 2 tool calls" })).toHaveAttribute("aria-expanded", "true")
    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("Read file")).toBeVisible()
    expect(screen.queryByText("Completed")).not.toBeInTheDocument()

    await user.click(screen.getByRole("button", { name: "Expand Run command" }))
    expect(screen.getByText("Command")).toBeVisible()
    expect(screen.getByText("12 passed")).toBeVisible()
    expect(screen.queryByText(/"command"/)).not.toBeInTheDocument()

    await user.click(screen.getByRole("button", { name: "Show original tool data" }))
    expect(screen.getByText(/"command": "npm run test"/)).toBeVisible()
  })

  it("surfaces terminal failures in the group summary", () => {
    render(<ToolActivityGroup parts={[
      toolPart("tool-1", "completed", 1_000, 7_000),
      toolPart("tool-2", "interrupted", 7_000, 19_000),
    ]} />)

    expect(screen.getByText("Used 2 tools · 1 failed")).toBeVisible()
    expect(screen.getByRole("button", { name: "Expand 2 tool calls" })).toHaveAttribute("aria-expanded", "false")
  })
})

function toolPart(
  partId: string,
  status: SessionPart["status"],
  createdAt: number,
  completedAt: number,
  tool: Partial<Extract<SessionPart["content"], { type: "tool" }>["tool"]> = {},
): ToolSessionPart {
  return {
    partId,
    messageId: "message",
    sessionId: "session",
    turnId: "turn",
    order: createdAt,
    revision: 0,
    status,
    createdAt,
    updatedAt: completedAt,
    completedAt: status === "running" ? undefined : completedAt,
    content: {
      type: "tool",
      tool: {
        toolCallId: `${partId}-call`,
        name: "read_file",
        activityGroupId: "turn:tools",
        ...tool,
      },
    },
  }
}
