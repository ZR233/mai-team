import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { beforeEach, describe, expect, it, vi } from "vitest"

import type { AgentDetail } from "@/api/product-types"
import { emptySession } from "@/events/session-reducer"
import { useSessionStore } from "@/events/session-store"
import { SessionWorkspace, activeSessionTurnId } from "@/features/session/session-workspace"

vi.mock("@/events/use-session-events", () => ({ useSessionEvents: vi.fn() }))
vi.mock("@/features/session/skill-mention-picker", () => ({ SkillMentionPicker: () => null }))

describe("session workspace canonical turn controls", () => {
  beforeEach(() => {
    useSessionStore.setState({
      generation: 1,
      connection: "live",
      connectionMessage: null,
      view: {
        ...emptySession(),
        sessionId: "session-1",
        turn: {
          turnId: "turn-from-session-stream",
          sessionId: "session-1",
          status: "runningTool",
          updatedAt: 7,
        },
      },
    })
  })

  it("cancels the canonical turn even when AgentDetail contains no active turn", async () => {
    const onStop = vi.fn(async () => undefined)
    render(
      <SessionWorkspace
        agent={agentWithoutActiveTurn()}
        sessionId="session-1"
        onSelectSession={() => undefined}
        onCreateSession={async () => undefined}
        onSend={async () => undefined}
        onStop={onStop}
      />,
    )

    await userEvent.click(screen.getByRole("button", { name: /stop/i }))

    expect(onStop).toHaveBeenCalledWith("turn-from-session-stream")
  })

  it("does not treat terminal canonical phases as running", () => {
    expect(activeSessionTurnId({
      turnId: "turn",
      sessionId: "session-1",
      status: "completed",
      updatedAt: 8,
    })).toBeNull()
  })
})

function agentWithoutActiveTurn(): AgentDetail {
  return {
    id: "agent-1",
    name: "Reviewer",
    role: "reviewer",
    state: {
      resource: "ready",
      runtime: {
        lifecycle: "active",
        activity: "idle",
        pending_inputs: 0,
      },
    },
    provider_id: "provider",
    provider_name: "Provider",
    model: "model",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    token_usage: {
      input_tokens: 0,
      cached_input_tokens: 0,
      output_tokens: 0,
      reasoning_output_tokens: 0,
      total_tokens: 0,
    },
    sessions: [{
      id: "session-1",
      title: "Chat 1",
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
      message_count: 0,
      token_usage: {
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        total_tokens: 0,
      },
    }],
    selected_session_id: "session-1",
  }
}
