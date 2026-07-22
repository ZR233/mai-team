import { fireEvent, render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { useState } from "react"
import { describe, expect, it } from "vitest"

import { emptySession } from "@/events/session-reducer"
import type { SessionTimelineEvent, TodoListSnapshot } from "@/events/session-events.generated"
import { SessionTimeline } from "@/features/session/session-timeline"
import {
  SessionTodoDrawer,
  SessionTodoRail,
  latestTodoSnapshot,
} from "@/features/session/session-todo-panel"

describe("session todo panel", () => {
  it("selects the highest-sequence todo snapshot among mixed events", () => {
    const events = [
      todoEvent(7, snapshot("Newest step", "completed")),
      activityEvent(9),
      todoEvent(2, snapshot("Old step", "pending")),
    ]

    expect(latestTodoSnapshot(events)).toEqual(snapshot("Newest step", "completed"))
  })

  it("renders only the selected latest snapshot", () => {
    const latest = latestTodoSnapshot([
      todoEvent(2, snapshot("Old step", "pending")),
      todoEvent(5, snapshot("Current step", "inProgress")),
    ])

    render(<RailHarness snapshot={latest!} />)

    expect(screen.getByText("Current step")).toBeInTheDocument()
    expect(screen.queryByText("Old step")).not.toBeInTheDocument()
  })

  it("collapses to a progress entry and can expand again", async () => {
    render(<RailHarness snapshot={snapshot("Inspect changes", "inProgress")} />)

    await userEvent.click(screen.getByRole("button", { name: "Collapse working list" }))
    expect(screen.queryByText("Inspect changes")).not.toBeInTheDocument()
    expect(screen.getByRole("button", { name: "Expand working list" })).toHaveAttribute("aria-expanded", "false")

    await userEvent.click(screen.getByRole("button", { name: "Expand working list" }))
    expect(screen.getByText("Inspect changes")).toBeInTheDocument()
  })

  it("shows empty and completed progress states", () => {
    const { rerender } = render(<RailHarness snapshot={{ ...snapshot("unused", "pending"), items: [] }} />)
    expect(screen.getByText("No planned steps")).toBeInTheDocument()
    expect(screen.getByLabelText("0 of 0 tasks completed")).toBeInTheDocument()

    rerender(<RailHarness snapshot={{
      ...snapshot("First", "completed"),
      items: [
        { step: "First", status: "completed" },
        { step: "Second", status: "completed" },
      ],
    }} />)
    expect(screen.getByLabelText("2 of 2 tasks completed")).toBeInTheDocument()
  })

  it("opens and closes the narrow-screen drawer", async () => {
    render(<SessionTodoDrawer snapshot={snapshot("Inspect changes", "inProgress")} />)

    await userEvent.click(screen.getByRole("button", { name: "Open working list" }))
    expect(screen.getByRole("heading", { name: "Working list" })).toBeInTheDocument()
    expect(screen.getByText("Inspect changes")).toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: "Close working list" }))
    expect(screen.queryByRole("heading", { name: "Working list" })).not.toBeInTheDocument()
  })

  it("keeps todo-only events out of the message timeline", () => {
    render(<SessionTimeline view={{
      ...emptySession(),
      sessionId: "session-1",
      timelineEvents: [todoEvent(1, snapshot("Detached step", "pending"))],
    }} />)

    expect(screen.getByText("No messages yet")).toBeInTheDocument()
    expect(screen.queryByText("Detached step")).not.toBeInTheDocument()
  })
})

function RailHarness({ snapshot }: { snapshot: TodoListSnapshot }) {
  const [expanded, setExpanded] = useState(true)
  return <SessionTodoRail snapshot={snapshot} expanded={expanded} onExpandedChange={setExpanded} />
}

function snapshot(step: string, status: TodoListSnapshot["items"][number]["status"]): TodoListSnapshot {
  return {
    callId: "todo-call",
    explanation: "Review flow",
    items: [{ step, status }],
  }
}

function todoEvent(sequence: number, value: TodoListSnapshot): SessionTimelineEvent {
  return {
    eventId: `todo-${sequence}`,
    sessionId: "session-1",
    sequence,
    createdAt: sequence,
    kind: { type: "todoListChanged", snapshot: value },
  }
}

function activityEvent(sequence: number): SessionTimelineEvent {
  return {
    eventId: `activity-${sequence}`,
    sessionId: "session-1",
    sequence,
    createdAt: sequence,
    kind: {
      type: "subAgentActivity",
      callId: "agent-call",
      kind: "spawned",
    },
  }
}
