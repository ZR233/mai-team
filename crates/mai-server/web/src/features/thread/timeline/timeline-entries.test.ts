import { describe, expect, it } from "vitest"

import type { ThreadItem, ThreadItemState, ThreadToolState } from "@/events/thread-events.generated"

import { formatDuration } from "./duration"
import { buildTimelineEntries } from "./timeline-entries"

let sequence = 0

function item(state: ThreadItemState, overrides: Partial<ThreadItem> = {}): ThreadItem {
  sequence += 1
  return { id: `item-${sequence}`, threadId: "thread-1", turnId: "turn-1", ordinal: sequence, revision: 1, createdAt: 0, updatedAt: 18, state, ...overrides }
}

function toolCall(state: ThreadToolState = succeeded(), overrides: Partial<ThreadItem> = {}, result = ""): ThreadItem {
  return item({ kind: "tool", data: { invocation: { toolCallId: "call", name: "exec" }, state: result ? succeeded(result) : state } }, overrides)
}

function succeeded(result = ""): ThreadToolState {
  return { kind: "succeeded", data: { completedAt: 18, output: { result, exitCode: 0 } } }
}

const completed = { kind: "completed", data: { completedAt: 18 } } as const
const message = (text: string, turnId = "turn-1") => item({ kind: "text", data: { channel: "final", text, lifecycle: completed } }, { turnId })

describe("buildTimelineEntries PL v2 投影", () => {
  it("合并同 turn 连续工具，并由文字打断", () => {
    const first = toolCall()
    const entries = buildTimelineEntries([first, toolCall(), message("working"), toolCall()])

    expect(entries.map((entry) => entry.kind)).toEqual(["toolGroup", "item", "tool"])
    expect(entries[0]).toMatchObject({ kind: "toolGroup", key: `tool-group:${first.id}`, group: { countLabel: "Used 2 tools", active: false } })
  })

  it("turn 边界打断工具和思考分组", () => {
    const tools = buildTimelineEntries([toolCall(succeeded(), { turnId: "turn-1" }), toolCall(succeeded(), { turnId: "turn-2" })])
    const thoughts = buildTimelineEntries([
      item({ kind: "thinking", data: { summary: ["one"], lifecycle: completed } }, { turnId: "turn-1" }),
      item({ kind: "thinking", data: { summary: ["two"], lifecycle: completed } }, { turnId: "turn-2" }),
    ])

    expect(tools.map((entry) => entry.kind)).toEqual(["tool", "tool"])
    expect(thoughts.map((entry) => entry.kind)).toEqual(["reasoningGroup", "reasoningGroup"])
  })

  it("隐藏空思考与内部协议条目且不打断工具分组", () => {
    const first = toolCall()
    const entries = buildTimelineEntries([
      first,
      item({ kind: "skill", data: { activation: { name: "review", source: "system", providerId: "local", resourceBase: { kind: "directory", path: "/skills/review" }, turnId: "turn-1", cause: { kind: "tool", toolCallId: "call" }, activatedAt: 1 } } }),
      item({ kind: "thinking", data: { summary: ["  "], lifecycle: completed } }),
      item({ kind: "file", data: { path: "README.md", completedAt: 1 } }),
      toolCall(),
    ])

    expect(entries).toEqual([expect.objectContaining({ kind: "toolGroup", key: `tool-group:${first.id}` })])
  })

  it("运行中的工具分组标记为 active，失败输出计数", () => {
    const active = buildTimelineEntries([toolCall(), toolCall({ kind: "running", data: {} })])
    const failed = buildTimelineEntries([
      toolCall(),
      toolCall({ kind: "failed", data: { failedAt: 18, failure: { kind: "execution", message: "exit" }, output: { result: JSON.stringify({ exitCode: 1 }), exitCode: 1 } } }),
    ])

    expect(active[0]).toMatchObject({ kind: "toolGroup", group: { countLabel: "Using 2 tools", durationLabel: null, active: true } })
    expect(failed[0]).toMatchObject({ kind: "toolGroup", group: { failedCount: 1 } })
  })

  it("按 ordinal 与 id 稳定排序", () => {
    const later = message("later")
    const sameB = message("same-b")
    const sameA = message("same-a")
    later.ordinal = 2
    sameB.ordinal = 1
    sameB.id = "b"
    sameA.ordinal = 1
    sameA.id = "a"

    expect(buildTimelineEntries([later, sameB, sameA]).map((entry) => entry.key)).toEqual(["a", "b", later.id])
  })
})

describe("formatDuration", () => {
  it("按 PL 秒级时间戳格式化时长", () => {
    expect(formatDuration(0, 0.5)).toBe("<1s")
    expect(formatDuration(0, 18)).toBe("18s")
    expect(formatDuration(0, 125)).toBe("2m 5s")
    expect(formatDuration(0, 180)).toBe("3m")
  })

  it("缺失或非法时间返回 null", () => {
    expect(formatDuration(undefined, 1)).toBeNull()
    expect(formatDuration(2, undefined)).toBeNull()
    expect(formatDuration(2, 1)).toBeNull()
  })
})
