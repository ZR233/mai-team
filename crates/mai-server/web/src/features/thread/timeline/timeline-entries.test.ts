import { describe, expect, it } from "vitest"

import type { ThreadItem, ThreadItemContent, ThreadToolCall } from "@/events/thread-events.generated"

import { formatDuration } from "./duration"
import { buildTimelineEntries } from "./timeline-entries"

let sequence = 0

function item(content: ThreadItemContent, overrides: Partial<ThreadItem> = {}): ThreadItem {
  sequence += 1
  return {
    id: `item-${sequence}`,
    threadId: "thread-1",
    turnId: "turn-1",
    ordinal: sequence,
    revision: 1,
    status: "completed",
    createdAt: 0,
    updatedAt: 18_000,
    completedAt: 18_000,
    content,
    ...overrides,
  }
}

function toolCall(overrides: Partial<ThreadItem> = {}, tool: Partial<ThreadToolCall> = {}): ThreadItem {
  return item({ type: "toolCall", tool: { toolCallId: "call", name: "exec", timedOut: false, ...tool } }, overrides)
}

const message = (text: string) => item({ type: "agentMessage", channel: "final", text })

describe("buildTimelineEntries", () => {
  it("返回空数组当没有条目", () => {
    expect(buildTimelineEntries([])).toEqual([])
  })

  it("合并同 turn 内连续的工具调用为一个分组", () => {
    const items = [toolCall(), toolCall(), toolCall()]

    const entries = buildTimelineEntries(items)

    expect(entries).toHaveLength(1)
    const entry = entries[0]!
    if (entry.kind !== "toolGroup") throw new Error(`expected toolGroup, got ${entry.kind}`)
    expect(entry.key).toBe(`tool-group:${items[0]!.id}`)
    expect(entry.group).toEqual({
      activities: entry.group.activities,
      countLabel: "Used 3 tools",
      durationLabel: "18s",
      active: false,
      failedCount: 0,
    })
    expect(entry.group.activities).toHaveLength(3)
  })

  it("文字输出打断分组", () => {
    const entries = buildTimelineEntries([toolCall(), toolCall(), message("working on it"), toolCall()])

    expect(entries.map((entry) => entry.kind)).toEqual(["toolGroup", "item", "tool"])
  })

  it("思考输出打断分组", () => {
    const entries = buildTimelineEntries([toolCall(), item({ type: "reasoning", summary: ["thinking"] }), toolCall()])

    expect(entries.map((entry) => entry.kind)).toEqual(["tool", "reasoningGroup", "tool"])
  })

  it("按 ordinal 与 id 稳定排序", () => {
    const later = message("later")
    const sameOrdinalB = message("same-b")
    const sameOrdinalA = message("same-a")
    later.ordinal = 2
    sameOrdinalB.ordinal = 1
    sameOrdinalB.id = "b"
    sameOrdinalA.ordinal = 1
    sameOrdinalA.id = "a"

    expect(buildTimelineEntries([later, sameOrdinalB, sameOrdinalA]).map((entry) => entry.key)).toEqual(["a", "b", later.id])
  })

  it("合并同 turn 内连续 reasoning 并保留最新摘要", () => {
    const first = item({ type: "reasoning", summary: ["Inspecting files"] })
    const second = item({ type: "reasoning", summary: ["Checking lifecycle"] })

    const entries = buildTimelineEntries([first, second])

    expect(entries).toHaveLength(1)
    expect(entries[0]).toMatchObject({
      kind: "reasoningGroup",
      key: `reasoning-group:${first.id}`,
      group: { latestSummary: "Checking lifecycle", durationLabel: "18s", active: false },
    })
  })

  it("turn 边界打断 reasoning 分组", () => {
    const entries = buildTimelineEntries([
      item({ type: "reasoning", summary: ["one"] }, { turnId: "turn-1" }),
      item({ type: "reasoning", summary: ["two"] }, { turnId: "turn-2" }),
    ])

    expect(entries.map((entry) => entry.kind)).toEqual(["reasoningGroup", "reasoningGroup"])
  })

  it("隐藏空 reasoning、file 与 contextCompaction", () => {
    expect(buildTimelineEntries([
      item({ type: "reasoning", summary: ["  "] }),
      item({ type: "file", path: "README.md" }),
      item({ type: "contextCompaction", beforeTokens: 10, afterTokens: 5, compactedAt: 1 }),
    ])).toEqual([])
  })

  it("隐藏的内部条目不打断可见 Timeline 的连续分组", () => {
    const first = toolCall()
    const internal = item({ type: "file", path: "README.md" })
    const second = toolCall()

    expect(buildTimelineEntries([first, internal, second])).toEqual([
      expect.objectContaining({ kind: "toolGroup", key: `tool-group:${first.id}` }),
    ])
  })

  it("turn 边界打断分组", () => {
    const entries = buildTimelineEntries([toolCall({ turnId: "turn-1" }), toolCall({ turnId: "turn-2" })])

    expect(entries.map((entry) => entry.kind)).toEqual(["tool", "tool"])
  })

  it("单个工具调用退化为紧凑条目", () => {
    const entries = buildTimelineEntries([toolCall({ id: "solo" })])

    expect(entries).toEqual([
      expect.objectContaining({ kind: "tool", key: "solo", activity: expect.objectContaining({ id: "solo" }) }),
    ])
  })

  it("非工具条目原样透传", () => {
    const user = item({ type: "userMessage", text: "hello" })

    expect(buildTimelineEntries([user])).toEqual([{ kind: "item", key: user.id, item: user }])
  })

  it("运行中的分组展示 Using 且不带时长", () => {
    const entries = buildTimelineEntries([
      toolCall({ status: "completed" }),
      toolCall({ status: "running", completedAt: undefined, updatedAt: 5_000 }),
    ])

    expect(entries[0]).toMatchObject({
      kind: "toolGroup",
      group: { countLabel: "Using 2 tools", durationLabel: null, active: true },
    })
  })

  it("统计失败的工具调用", () => {
    const entries = buildTimelineEntries([
      toolCall(),
      toolCall({}, { exitCode: 1, result: JSON.stringify({ status: "error", exitCode: 1 }) }),
    ])

    expect(entries[0]).toMatchObject({ kind: "toolGroup", group: { failedCount: 1 } })
  })

  it("分组 key 基于首条 id，流式更新期间保持稳定", () => {
    const growing = [toolCall(), toolCall()]
    const before = buildTimelineEntries(growing)
    growing.push(toolCall(), toolCall())
    const after = buildTimelineEntries(growing)

    expect(after[0]!.key).toBe(before[0]!.key)
    const entry = after[0]!
    if (entry.kind !== "toolGroup") throw new Error(`expected toolGroup, got ${entry.kind}`)
    expect(entry.group.activities).toHaveLength(4)
  })
})

describe("formatDuration", () => {
  it("格式化各档时长", () => {
    expect(formatDuration(0, 500)).toBe("<1s")
    expect(formatDuration(0, 18_000)).toBe("18s")
    expect(formatDuration(0, 125_000)).toBe("2m 5s")
    expect(formatDuration(0, 180_000)).toBe("3m")
  })

  it("缺失或非法时间返回 null", () => {
    expect(formatDuration(undefined, 1_000)).toBeNull()
    expect(formatDuration(0, undefined)).toBeNull()
    expect(formatDuration(2_000, 1_000)).toBeNull()
  })
})
