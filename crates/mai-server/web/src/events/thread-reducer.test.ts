import { describe, expect, it } from "vitest"

import type {
  InteractionRequest,
  ThreadActivity,
  ThreadNotification,
  ThreadNotificationEnvelope,
  ThreadSnapshot,
  ThreadStorageState,
  Turn,
} from "@/events/thread-events.generated"
import { applyThreadNotification, ThreadProjectionError, validateThreadSnapshot } from "@/events/thread-reducer"

describe("Thread reducer 权威首帧与 typed 通知", () => {
  it("拒绝跨 Thread 通知、水位缺口与非逐帧 +1", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), envelope("thread-b", { type: "turnStarted", turn: turn("thread-b") }))).toThrow(ThreadProjectionError)
    expect(() => applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }, { baseRevision: 3, revision: 4 }))).toThrow(/revision gap/)
    expect(() => applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }, { revision: 2 }))).toThrow(/advance by one/)
  })

  it("只用 active Turn 生命周期维护当前 Turn，不伪造历史", () => {
    const started = applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }))
    expect(started.activeTurn?.id).toBe("thread-a:turn")
    expect("items" in started).toBe(false)

    const advanced = applyThreadNotification(started, envelope("thread-a", {
      type: "turnUpdated",
      turn: { ...turn("thread-a"), revision: 1, state: { kind: "running", data: { startedAt: 2, phase: "thinking" } }, updatedAt: 2 },
    }, { baseRevision: 1, revision: 2 }))
    expect(advanced.activeTurn?.state).toEqual({ kind: "running", data: { startedAt: 2, phase: "thinking" } })

    const completed = applyThreadNotification(advanced, envelope("thread-a", {
      type: "turnCompleted",
      turn: { ...turn("thread-a"), state: { kind: "completed", data: { startedAt: 2, completedAt: 3, completion: "normal" } } },
    }, { baseRevision: 2, revision: 3 }))
    expect(completed.activeTurn).toBeUndefined()
  })

  it("终态 Turn 与当前 active Turn 不一致时保留当前 Turn", () => {
    const started = applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "turnStarted", turn: turn("thread-a") }))
    const other = applyThreadNotification(started, envelope("thread-a", {
      type: "turnCompleted",
      turn: { ...turn("thread-a"), id: "thread-a:other" },
    }, { baseRevision: 1, revision: 2 }))
    expect(other.activeTurn?.id).toBe("thread-a:turn")
  })

  it("拒绝跨 Thread 的 Turn 载荷", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "turnStarted", turn: turn("thread-b") }))).toThrow(/Turn belongs to Thread/)
  })

  it("interaction 按身份 upsert 并拒绝 revision 回退", () => {
    const pending = interaction("thread-a", "interaction-1", 0)
    const added = applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "interactionChanged", interaction: pending }))
    expect(added.interactions.map((entry) => entry.interactionId)).toEqual(["interaction-1"])

    const resolved = applyThreadNotification(added, envelope("thread-a", {
      type: "interactionChanged",
      interaction: interaction("thread-a", "interaction-1", 1),
    }, { baseRevision: 1, revision: 2 }))
    expect(resolved.interactions).toHaveLength(1)
    expect(resolved.interactions[0]?.revision).toBe(1)

    expect(() => applyThreadNotification(resolved, envelope("thread-a", {
      type: "interactionChanged",
      interaction: interaction("thread-a", "interaction-1", 0),
    }, { baseRevision: 2, revision: 3 }))).toThrow(/revision regressed/)
  })

  it("runtime、activity 与 storage 都是 typed 状态帧", () => {
    const withRuntime = applyThreadNotification(snapshot("thread-a"), envelope("thread-a", {
      type: "threadRuntimeUpdated",
      runtime: runtime("thread-a"),
    }))
    expect(withRuntime.runtime?.usage.model).toBe("test-model")

    const withActivity = applyThreadNotification(withRuntime, envelope("thread-a", {
      type: "activityChanged",
      activity: activity("thread-a"),
    }, { baseRevision: 1, revision: 2 }))
    expect(withActivity.activity?.kind).toBe("runningTool")

    const clearedActivity = applyThreadNotification(withActivity, envelope("thread-a", {
      type: "activityChanged",
      activity: null,
    }, { baseRevision: 2, revision: 3 }))
    expect(clearedActivity.activity).toBeUndefined()

    const withStorage = applyThreadNotification(clearedActivity, envelope("thread-a", {
      type: "storageChanged",
      storage: storageState(),
    }, { baseRevision: 3, revision: 4 }))
    expect(withStorage.storage?.fault).toBe("writeFailed")
  })

  it("Lagged 使投影失效，首帧必须属于目标 Thread", () => {
    expect(() => applyThreadNotification(snapshot("thread-a"), envelope("thread-a", { type: "lagged", dropped: 4 }))).toThrow(/lagged by 4/)
    expect(validateThreadSnapshot("thread-a", snapshot("thread-a")).thread.id).toBe("thread-a")
    expect(() => validateThreadSnapshot("thread-a", snapshot("thread-b"))).toThrow(/snapshot mismatch/)
  })

  it("首帧校验 runtime 与 interaction 的所有权", () => {
    const wrongRuntime = { ...snapshot("thread-a"), runtime: runtime("thread-b") }
    expect(() => validateThreadSnapshot("thread-a", wrongRuntime)).toThrow(/Runtime snapshot belongs to Thread/)

    const wrongInteraction = { ...snapshot("thread-a"), interactions: [interaction("thread-b", "interaction-1", 0)] }
    expect(() => validateThreadSnapshot("thread-a", wrongInteraction)).toThrow(/Interaction interaction-1 belongs to Thread/)
  })
})

function snapshot(threadId: string): ThreadSnapshot {
  return {
    schemaVersion: 14,
    revision: 0,
    thread: {
      id: threadId,
      projectId: "",
      title: threadId,
      mode: "mode.simple",
      workspaceMode: "local",
      workspacePath: "",
      rootThreadId: threadId,
      role: "planner",
      agentPath: "root",
      status: "idle",
      createdAt: 1,
      updatedAt: 1,
      archived: false,
    },
    interactions: [],
  }
}

function turn(threadId: string): Turn {
  return { id: `${threadId}:turn`, threadId, revision: 0, state: { kind: "queued", data: { queuedAt: 1 } }, updatedAt: 1 }
}

function interaction(threadId: string, interactionId: string, revision: number): InteractionRequest {
  return {
    interactionId,
    scope: { threadId, turnId: `${threadId}:turn`, purpose: { kind: "general" } },
    revision,
    content: { kind: "userInput", data: { questions: [], state: { kind: "pending", data: { operationId: interactionId } } } },
    createdAt: 1,
    updatedAt: 1,
  }
}

function runtime(threadId: string) {
  return {
    threadId,
    usage: {
      hasIncompleteUsage: false,
      model: "test-model",
      latestContextTokens: 0,
      promptTokens: 0,
      completionTokens: 0,
      cachedPromptTokens: 0,
      cacheWriteTokens: 0,
      reasoningTokens: 0,
      inferenceCount: 0,
      totalTokens: 0,
      cacheUsage: { inputTokens: 0, cacheReadTokens: 0, hasIncompleteUsage: false },
      hasUnpricedUsage: false,
      updatedAt: 1,
    },
    turnCompletionTokens: 0,
    turnDecodeMillis: 0,
    activeSkills: [],
    activeMcpServers: [],
    activeLspServers: [],
    updatedAt: 1,
  }
}

function activity(threadId: string): ThreadActivity {
  return {
    threadId,
    identity: `activity:${threadId}:turn:runningTool`,
    revision: 0,
    turnId: `${threadId}:turn`,
    kind: "runningTool",
    summary: "cargo test",
    summaryTruncated: false,
    tools: { count: 1, active: [] },
  }
}

function storageState(): ThreadStorageState {
  return {
    fault: "writeFailed",
    faultGeneration: 2,
    execution: "paused",
    pressurePaused: false,
    resumeRequired: true,
    canResume: false,
    lastError: "disk full",
  }
}

function envelope(
  threadId: string,
  notification: ThreadNotification,
  overrides: { epoch?: number; baseRevision?: number; revision?: number } = {},
): ThreadNotificationEnvelope {
  const revision = overrides.revision ?? 1
  return {
    threadId,
    epoch: overrides.epoch ?? 1,
    baseRevision: overrides.baseRevision ?? 0,
    revision,
    emittedAt: revision,
    notification,
  }
}
