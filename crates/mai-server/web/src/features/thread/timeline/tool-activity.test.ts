import { describe, expect, it } from "vitest"

import type { ThreadToolInvocation, ThreadToolOutput, ThreadToolState } from "@/events/thread-events.generated"

import { buildToolActivity, type ToolCallItem } from "./tool-activity"

function toolCallItem(state: ThreadToolState = succeeded(), invocation: Partial<ThreadToolInvocation> = {}): ToolCallItem {
  return {
    id: "item-1",
    threadId: "thread-1",
    turnId: "turn-1",
    ordinal: 1,
    revision: 1,
    createdAt: 0,
    updatedAt: 1,
    state: { kind: "tool", data: { invocation: { toolCallId: "call", name: "exec", ...invocation }, state } },
  }
}

function succeeded(output: Partial<ThreadToolOutput> = {}): ThreadToolState {
  return { kind: "succeeded", data: { completedAt: 1, output: { result: "", exitCode: 0, ...output } } }
}

describe("buildToolActivity 原生状态映射", () => {
  it("穷尽映射进行态与终态", () => {
    const active: ThreadToolState[] = [
      { kind: "started", data: null },
      { kind: "streaming", data: null },
      { kind: "awaitingApproval", data: null },
      { kind: "approved", data: null },
      { kind: "running", data: {} },
    ]
    for (const state of active) expect(buildToolActivity(toolCallItem(state)).outcome).toBe("active")

    expect(buildToolActivity(toolCallItem(succeeded())).outcome).toBe("completed")
    expect(buildToolActivity(toolCallItem({ kind: "failed", data: { failedAt: 1, failure: { kind: "execution", message: "boom" } } })).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem({ kind: "failed", data: { failedAt: 1, failure: { kind: "timedOut", message: "timeout" } } })).outcome).toBe("timedOut")
    expect(buildToolActivity(toolCallItem({ kind: "denied", data: { deniedAt: 1, reason: "not allowed" } })).outcome).toBe("denied")
    expect(buildToolActivity(toolCallItem({ kind: "cancelled", data: { cancelledAt: 1, reason: "stop" } })).outcome).toBe("interrupted")
  })

  it("非零退出码判定为失败", () => {
    expect(buildToolActivity(toolCallItem(succeeded({ exitCode: 2 }))).outcome).toBe("failed")
    expect(buildToolActivity(toolCallItem(succeeded({ result: JSON.stringify({ exitCode: 1 }), exitCode: undefined }))).outcome).toBe("failed")
  })

  it("已知工具使用专属标题与参数摘要", () => {
    const activity = buildToolActivity(toolCallItem(succeeded(), { name: "exec", arguments: JSON.stringify({ command: "cargo test" }) }))

    expect(activity.title).toBe("Run command")
    expect(activity.summary).toBe("cargo test")
  })

  it("结构化参数和结果构建分区与原始数据", () => {
    const result = JSON.stringify({ status: "exit", exitCode: 0, stdout: "test passed", stderr: "" })
    const activity = buildToolActivity(toolCallItem(
      succeeded({ result }),
      { arguments: JSON.stringify({ command: "cargo test" }) },
    ))

    expect(activity.sections).toEqual([
      { kind: "code", title: "Command", text: "cargo test" },
      { kind: "fields", title: "Result", items: [{ label: "Status", value: "exit" }, { label: "Exit code", value: "0" }] },
      { kind: "code", title: "Standard output", text: "test passed" },
    ])
    expect(activity.rawResult).toBe(result)
  })

  it("失败、拒绝与输出产物来自 typed Tool state", () => {
    const failed = buildToolActivity(toolCallItem({ kind: "failed", data: { failedAt: 1, failure: { kind: "execution", message: "connection lost" } } }))
    const denied = buildToolActivity(toolCallItem({ kind: "denied", data: { deniedAt: 1, reason: "outside workspace" } }))
    const output = buildToolActivity(toolCallItem(succeeded({ outputArtifacts: [{ id: "artifact-1", filename: "report.md" }] })))

    expect(failed.sections.at(-1)).toEqual({ kind: "text", title: "Error", text: "connection lost" })
    expect(denied.sections.at(-1)).toEqual({ kind: "text", title: "Denied", text: "outside workspace" })
    expect(output.artifacts).toEqual([{ id: "artifact-1", label: "report.md" }])
  })
})
