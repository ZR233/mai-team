import { describe, expect, it } from "vitest"

import type { AgentRuntimeState, AgentSummary } from "@/api/product-types"
import { agentCanRunThread, agentPresentationStatus } from "@/features/agents/agent-lifecycle"

const turn = { turnId: "turn-1" }

describe("PL v2 Agent tagged state", () => {
  it("只允许 canonical accepting-work 状态接收新消息", () => {
    const accepting: AgentRuntimeState[] = [
      { kind: "idle", data: null },
      { kind: "queued", data: turn },
      { kind: "running", data: turn },
      { kind: "waitingTool", data: turn },
      { kind: "waitingInteraction", data: { ...turn, interactionId: "interaction-1" } },
    ]
    const rejecting: AgentRuntimeState[] = [
      { kind: "cancelling", data: turn },
      { kind: "closing", data: null },
      { kind: "closed", data: null },
      {
        kind: "faulted",
        data: {
          error: { code: "runtimeFault", message: "failed", retryable: false },
          turnId: "turn-1",
          classification: "recoverableRuntime",
        },
      },
    ]

    expect(accepting.map((state) => agentCanRunThread(agent(state)))).toEqual([
      true,
      true,
      true,
      true,
      true,
    ])
    expect(rejecting.map((state) => agentCanRunThread(agent(state)))).toEqual([
      false,
      false,
      false,
      false,
    ])
  })

  it("产品资源状态与 PL 执行状态保持正交", () => {
    expect(agentPresentationStatus(agent({ kind: "running", data: turn }, "deleting"))).toBe("deleting")
    expect(agentCanRunThread(agent({ kind: "idle", data: null }, "provisioning"))).toBe(false)
    expect(agentPresentationStatus(agent({ kind: "waitingTool", data: turn }), "streaming")).toBe("streaming")
    expect(agentPresentationStatus(agent({ kind: "closed", data: null }), "streaming")).toBe("closed")
  })
})

function agent(
  state: AgentRuntimeState,
  resourceState: AgentSummary["resource"]["state"] = "ready",
): AgentSummary {
  return {
    id: "agent-1",
    name: "Agent",
    resource: { state: resourceState, error: null },
    runtime: {
      identity: { id: "agent-1", parentId: null, role: "executor", depth: 0 },
      state,
      pendingInputs: 0,
      progress: null,
      lastTurn: null,
      revision: 1,
      eventSequence: 1,
      updatedAt: 1,
    },
    provider_id: "provider",
    provider_name: "Provider",
    model: "model",
    created_at: "2026-08-26T00:00:00Z",
    updated_at: "2026-08-26T00:00:00Z",
    token_usage: {
      input_tokens: 0,
      cached_input_tokens: 0,
      output_tokens: 0,
      reasoning_output_tokens: 0,
      total_tokens: 0,
    },
  }
}
