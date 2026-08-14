import { describe, expect, it } from "vitest"

import type { ThreadItem, ThreadToolCall } from "@/events/thread-events.generated"

import { buildToolActivity, type ToolCallItem } from "./tool-activity"

function toolCallItem(overrides: Partial<ThreadItem> = {}, tool: Partial<ThreadToolCall> = {}): ToolCallItem {
  return {
    id: "item-1",
    threadId: "thread-1",
    turnId: "turn-1",
    ordinal: 1,
    revision: 1,
    status: "completed",
    createdAt: 0,
    updatedAt: 1_000,
    completedAt: 1_000,
    content: { type: "toolCall", tool: { toolCallId: "call", name: "exec", timedOut: false, ...tool } },
    ...overrides,
  } as ToolCallItem
}

describe("buildToolActivity 结局映射", () => {
  it("错误、拒绝与超时优先于其他信号", () => {
    expect(buildToolActivity(toolCallItem({ error: "boom" })).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem({}, { denialReason: "not allowed" })).outcome).toBe("denied")
    expect(buildToolActivity(toolCallItem({ status: "denied" })).outcome).toBe("denied")
    expect(buildToolActivity(toolCallItem({}, { timedOut: true })).outcome).toBe("timedOut")
    expect(buildToolActivity(toolCallItem({}, { result: JSON.stringify({ timedOut: true }) })).outcome).toBe("timedOut")
  })

  it("非零退出码判定为失败", () => {
    expect(buildToolActivity(toolCallItem({}, { exitCode: 2 })).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem({}, { result: JSON.stringify({ exitCode: 1 }) })).outcome).toBe("failed")
  })

  it("条目状态穷尽映射到 ToolOutcome", () => {
    expect(buildToolActivity(toolCallItem({ status: "started" })).outcome).toBe("active")
    expect(buildToolActivity(toolCallItem({ status: "streaming" })).outcome).toBe("active")
    expect(buildToolActivity(toolCallItem({ status: "awaitingApproval" })).outcome).toBe("active")
    expect(buildToolActivity(toolCallItem({ status: "approved" })).outcome).toBe("active")
    expect(buildToolActivity(toolCallItem({ status: "running" })).outcome).toBe("active")
    expect(buildToolActivity(toolCallItem({ status: "failed" })).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem({ status: "budgetLimited" })).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem({ status: "interrupted" })).outcome).toBe("interrupted")
    expect(buildToolActivity(toolCallItem({ status: "completed" })).outcome).toBe("completed")
  })
})

describe("buildToolActivity 标题与摘要", () => {
  it("已知工具使用专属标题，摘要取命令", () => {
    const activity = buildToolActivity(toolCallItem({}, { name: "exec", arguments: JSON.stringify({ command: "cargo test" }) }))

    expect(activity.title).toBe("Run command")
    expect(activity.summary).toBe("cargo test")
  })

  it("apply_patch 摘要列出变更路径", () => {
    const patch = "*** Add File: crates/new.rs\n*** Update File: README.md"
    const activity = buildToolActivity(toolCallItem({}, { name: "apply_patch", arguments: JSON.stringify({ input: patch }) }))

    expect(activity.title).toBe("Apply patch")
    expect(activity.summary).toBe("crates/new.rs, README.md")
  })

  it("未知工具回退到通用标题与摘要", () => {
    const activity = buildToolActivity(toolCallItem({}, { name: "custom_tool", arguments: JSON.stringify({ summary: "did a thing" }) }))

    expect(activity.title).toBe("Custom tool")
    expect(activity.summary).toBe("did a thing")
  })

  it("名称缺失时按载荷形状推断工具", () => {
    const activity = buildToolActivity(toolCallItem({}, {
      name: "",
      result: JSON.stringify({ status: "exit", exitCode: 0, stdout: "ok", stderr: "" }),
    }))

    expect(activity.title).toBe("Run command")
  })
})

describe("buildToolActivity 分区与原始数据", () => {
  it("exec 参数与结果构建命令、标准输出分区", () => {
    const activity = buildToolActivity(toolCallItem({}, {
      arguments: JSON.stringify({ command: "cargo test" }),
      result: JSON.stringify({ status: "exit", exitCode: 0, stdout: "test passed", stderr: "" }),
    }))

    expect(activity.sections).toEqual([
      { kind: "code", title: "Command", text: "cargo test" },
      { kind: "fields", title: "Result", items: [{ label: "Status", value: "exit" }, { label: "Exit code", value: "0" }] },
      { kind: "code", title: "Standard output", text: "test passed" },
    ])
    expect(activity.rawArguments).toBe(JSON.stringify({ command: "cargo test" }))
    expect(activity.rawResult).toBe(JSON.stringify({ status: "exit", exitCode: 0, stdout: "test passed", stderr: "" }))
  })

  it("纯文本载荷不提供原始 JSON 折叠区", () => {
    const activity = buildToolActivity(toolCallItem({}, { arguments: "just text" }))

    expect(activity.rawArguments).toBeNull()
    expect(activity.sections).toEqual([{ kind: "text", title: "Input", text: "just text" }])
  })

  it("条目错误与拒绝原因追加为分区", () => {
    const failed = buildToolActivity(toolCallItem({ error: "connection lost" }))
    const denied = buildToolActivity(toolCallItem({}, { denialReason: "outside workspace" }))

    expect(failed.sections.at(-1)).toEqual({ kind: "text", title: "Error", text: "connection lost" })
    expect(denied.sections.at(-1)).toEqual({ kind: "text", title: "Denied", text: "outside workspace" })
  })

  it("输出产物映射为带 id 的下载条目", () => {
    const activity = buildToolActivity(toolCallItem({}, {
      outputArtifacts: [{ id: "artifact-1", filename: "report.md" }, { label: "anonymous" }],
    }))

    expect(activity.artifacts).toEqual([
      { id: "artifact-1", label: "report.md" },
      { id: undefined, label: "anonymous" },
    ])
  })
})
