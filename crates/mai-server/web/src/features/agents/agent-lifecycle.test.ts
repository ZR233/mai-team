import { describe, expect, it } from "vitest"

import type { AgentSummary } from "@/api/product-types"
import type { ThreadStatus } from "@/events/thread-events.generated"
import { agentCanRunThread, agentPresentationStatus } from "@/features/agents/agent-lifecycle"

describe("PL Thread 与 Agent 产品资源状态", () => {
  it("只允许可继续执行的 Thread 状态接收新消息", () => {
    const accepting: ThreadStatus[] = ["idle", "queued", "running", "waitingTool", "waitingInteraction"]
    const rejecting: ThreadStatus[] = ["cancelling", "closing", "closed", "faulted"]

    expect(accepting.map((status) => agentCanRunThread(agent(status)))).toEqual(
      accepting.map(() => true),
    )
    expect(rejecting.map((status) => agentCanRunThread(agent(status)))).toEqual(
      rejecting.map(() => false),
    )
  })

  it("产品资源状态与 PL Thread 状态保持正交", () => {
    expect(agentPresentationStatus(agent("running", "deleting"))).toBe("deleting")
    expect(agentCanRunThread(agent("idle", "provisioning"))).toBe(false)
    expect(agentPresentationStatus(agent("waitingTool"), "streaming")).toBe("streaming")
    expect(agentPresentationStatus(agent("closed"), "streaming")).toBe("closed")
  })
})

function agent(
  status: ThreadStatus,
  resourceState: AgentSummary["resource"]["state"] = "ready",
): AgentSummary {
  return {
    id: "agent-1",
    name: "Agent",
    resource: { state: resourceState, error: null },
    runtime: {
      schemaVersion: 1,
      revision: 1,
      thread: {
        id: "agent-1",
        projectId: "project-1",
        title: "Agent",
        mode: "mode.simple",
        workspaceMode: "local",
        workspacePath: "/workspace/repo",
        rootThreadId: "agent-1",
        role: "executor",
        agentPath: "agent-1",
        status,
        createdAt: 1,
        updatedAt: 1,
        archived: false,
      },
      interactions: [],
    },
    provider_id: "provider",
    provider_name: "Provider",
    model: "model",
    created_at: "2026-08-26T00:00:00Z",
    updated_at: "2026-08-26T00:00:00Z",
    usage: {
      promptTokens: 0,
      cachedPromptTokens: 0,
      cacheMissTokens: 0,
      cacheWriteTokens: 0,
      hasIncompleteUsage: false,
      completionTokens: 0,
      reasoningTokens: 0,
      totalTokens: 0,
    },
  }
}
