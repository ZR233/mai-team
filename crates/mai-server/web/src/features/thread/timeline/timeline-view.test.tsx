import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import type { Thread, ThreadItem, ThreadItemState, ThreadSnapshot, ThreadToolState } from "@/events/thread-events.generated"

import { ThreadTimeline, TimelineEntriesView } from "./timeline-view"

let sequence = 0

function item(state: ThreadItemState, overrides: Partial<ThreadItem> = {}): ThreadItem {
  sequence += 1
  return {
    id: `item-${sequence}`,
    threadId: "thread-1",
    turnId: "turn-1",
    ordinal: sequence,
    revision: 1,
    createdAt: 0,
    updatedAt: 18,
    state,
    ...overrides,
  }
}

function toolCall(name: string, state: ThreadToolState = succeeded(), argumentsText = ""): ThreadItem {
  return item({
    kind: "tool",
    data: { invocation: { toolCallId: "call", name, arguments: argumentsText }, state },
  })
}

function succeeded(result = ""): ThreadToolState {
  return { kind: "succeeded", data: { completedAt: 18, output: { result, exitCode: 0 } } }
}

function completed(at = 18) {
  return { kind: "completed", data: { completedAt: at } } as const
}

function snapshot(items: ThreadItem[]): ThreadSnapshot {
  return { schemaVersion: 7, revision: 1, thread: thread(), items, interactions: [] }
}

function thread(): Thread {
  return { id: "thread-1", projectId: "", title: "thread", mode: "simple", rootThreadId: "thread-1", role: "root", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false }
}

describe("ThreadTimeline PL v2 原生时间线", () => {
  it("连续工具调用折叠为分组并可展开", async () => {
    render(<ThreadTimeline snapshot={snapshot([
      toolCall("exec", succeeded(), JSON.stringify({ command: "cargo test" })),
      toolCall("read_file", succeeded(), JSON.stringify({ path: "README.md" })),
    ])} />)

    expect(screen.getByText("Used 2 tools")).toBeVisible()
    expect(screen.getByText("18s")).toBeVisible()
    await userEvent.click(screen.getByRole("button", { name: "Expand Used 2 tools" }))
    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("Read file")).toBeVisible()
  })

  it("运行中的工具和思考使用各自 tagged state", () => {
    render(<ThreadTimeline snapshot={snapshot([
      toolCall("exec", { kind: "running", data: { streamedOutput: "working" } }),
      item({ kind: "thinking", data: { content: ["still thinking"], lifecycle: { kind: "streaming", data: null } } }),
    ])} />)

    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("running")).toBeVisible()
    expect(screen.getByText(/still thinking/)).toBeVisible()
  })

  it("完成的思考默认折叠，展开后显示内容", async () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ kind: "thinking", data: { summary: ["Checking configuration"], content: ["details"], lifecycle: completed() } }),
    ])} />)

    expect(screen.getByText("Checking configuration · 18s")).toBeVisible()
    await userEvent.click(screen.getByRole("button", { name: "Expand reasoning" }))
    expect(screen.getByText(/details/)).toBeVisible()
  })

  it("直接渲染生产 Review 使用的 PL v2 tagged ThreadItem", () => {
    const productionItem = item({
      kind: "text",
      data: { channel: "final", text: "原生 PL v2 Review 结果", lifecycle: completed(2) },
    }, { id: "turn-1:mail:1", revision: 0, createdAt: 1, updatedAt: 2 })

    render(<TimelineEntriesView items={[productionItem]} />)

    expect(screen.getByText("原生 PL v2 Review 结果")).toBeVisible()
  })

  it("渲染文字与计划，并隐藏协议内部条目", () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ kind: "text", data: { channel: "user", text: "please review", lifecycle: completed() } }),
      item({ kind: "text", data: { channel: "commentary", text: "正在检查生命周期。", lifecycle: completed() } }),
      item({ kind: "plan", data: { content: "- 核对清理边界", lifecycle: completed() } }),
      item({ kind: "skill", data: { activation: { name: "review", source: "system", providerId: "local", resourceBase: { kind: "directory", path: "/skills/review" }, turnId: "turn-1", cause: { kind: "tool", toolCallId: "call" }, activatedAt: 1 } } }),
      item({ kind: "file", data: { path: "secret.txt", completedAt: 1 } }),
      item({ kind: "contextCompaction", data: { beforeTokens: 10, afterTokens: 5, compactedAt: 1 } }),
    ])} />)

    expect(screen.getByText("please review")).toBeVisible()
    expect(screen.getByText("正在检查生命周期。")).toBeVisible()
    expect(screen.getByText("核对清理边界")).toBeVisible()
    expect(screen.queryByText("secret.txt")).not.toBeInTheDocument()
  })

  it("仅在 canonical Turn running 状态显示尾部活动行", () => {
    const active = snapshot([])
    active.activeTurn = {
      id: "turn-1",
      threadId: "thread-1",
      revision: 1,
      state: { kind: "running", data: { startedAt: 1, phase: "runningTool" } },
      updatedAt: 1,
    }
    active.runtime = {
      threadId: "thread-1",
      usage: { model: "test", latestContextTokens: 0, promptTokens: 0, completionTokens: 0, cachedPromptTokens: 0, cacheWriteTokens: 0, cacheMissTokens: 0, reasoningTokens: 0, inferenceCount: 0, totalTokens: 0, hasUnpricedUsage: false, updatedAt: 1 },
      activeSkills: [], activeMcpServers: [], activeLspServers: [], progress: "正在执行测试", updatedAt: 1,
    }

    const { rerender } = render(<ThreadTimeline snapshot={active} />)
    expect(screen.getByText("Running tools")).toBeVisible()
    expect(screen.getByText(/正在执行测试/)).toBeVisible()
    rerender(<ThreadTimeline snapshot={snapshot([])} />)
    expect(screen.queryByText("Running tools")).not.toBeInTheDocument()
  })
})
