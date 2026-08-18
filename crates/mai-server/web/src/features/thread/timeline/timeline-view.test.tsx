import { render, screen } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { describe, expect, it } from "vitest"

import type { Thread, ThreadItem, ThreadItemContent, ThreadSnapshot, ThreadToolCall } from "@/events/thread-events.generated"

import { ThreadTimeline } from "./timeline-view"

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

function toolCall(name: string, overrides: Partial<ThreadItem> = {}, tool: Partial<ThreadToolCall> = {}): ThreadItem {
  return item({ type: "toolCall", tool: { toolCallId: "call", name, timedOut: false, ...tool } }, overrides)
}

function snapshot(items: ThreadItem[]): ThreadSnapshot {
  return {
    schemaVersion: 1,
    revision: 1,
    thread: thread(),
    items,
    interactions: [],
  }
}

function thread(): Thread {
  return { id: "thread-1", projectId: "", title: "thread", mode: "simple", rootThreadId: "thread-1", role: "root", agentPath: "root", status: "idle", createdAt: 1, updatedAt: 1, archived: false }
}

describe("ThreadTimeline 工具分组", () => {
  it("连续工具调用折叠为一个分组行，展开后显示各工具", async () => {
    render(<ThreadTimeline snapshot={snapshot([
      toolCall("exec", {}, { arguments: JSON.stringify({ command: "cargo test" }) }),
      toolCall("read_file", {}, { arguments: JSON.stringify({ path: "README.md" }) }),
    ])} />)

    expect(screen.getByText("Used 2 tools")).toBeVisible()
    expect(screen.getByText("18s")).toBeVisible()
    expect(screen.queryByText("Run command")).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole("button", { name: "Expand Used 2 tools" }))

    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("Read file")).toBeVisible()
  })

  it("运行中的工具展示 running 徽标与标题", () => {
    render(<ThreadTimeline snapshot={snapshot([
      toolCall("exec", { status: "running", completedAt: undefined }),
    ])} />)

    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("running")).toBeVisible()
  })

  it("单个工具调用渲染为紧凑行", () => {
    render(<ThreadTimeline snapshot={snapshot([
      toolCall("exec", {}, { arguments: JSON.stringify({ command: "cargo build" }) }),
    ])} />)

    expect(screen.getByText("Run command")).toBeVisible()
    expect(screen.getByText("cargo build")).toBeVisible()
    expect(screen.queryByText(/tools/)).not.toBeInTheDocument()
  })
})

describe("ThreadTimeline 思考条目", () => {
  it("完成的思考默认折叠为一行摘要，点击展开", async () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ type: "reasoning", summary: ["Checking configuration"], content: ["let me check the config"] }),
    ])} />)

    expect(screen.getByText("Checking configuration · 18s")).toBeVisible()
    expect(screen.queryByText(/check the config/)).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole("button", { name: "Expand reasoning" }))

    expect(screen.getByText(/check the config/)).toBeVisible()
  })

  it("流式中的思考默认展开", () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ type: "reasoning", content: ["still thinking"] }, { status: "streaming", completedAt: undefined }),
    ])} />)

    expect(screen.getByText("Thinking")).toBeVisible()
    expect(screen.getByText(/still thinking/)).toBeVisible()
  })

  it("连续思考显示最新摘要并在展开后保留全部内容", async () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ type: "reasoning", summary: ["Inspecting repository"] }),
      item({ type: "reasoning", summary: ["Checking cleanup ownership"] }),
    ])} />)

    expect(screen.getByText("Checking cleanup ownership · 18s")).toBeVisible()
    await userEvent.click(screen.getByRole("button", { name: "Expand reasoning" }))
    expect(screen.getByText("Inspecting repository")).toBeVisible()
    expect(screen.getAllByText("Checking cleanup ownership").length).toBeGreaterThan(0)
  })
})

describe("ThreadTimeline 基础条目", () => {
  it("没有快照时展示空状态", () => {
    render(<ThreadTimeline snapshot={null} />)

    expect(screen.getByText("No Thread activity yet.")).toBeVisible()
  })

  it("文字消息保持卡片渲染", () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ type: "userMessage", text: "please review" }),
      item({ type: "agentMessage", channel: "final", text: "here is the result" }),
    ])} />)

    expect(screen.getByText("please review")).toBeVisible()
    expect(screen.getByText("here is the result")).toBeVisible()
  })

  it("显示 commentary 与 plan，并隐藏内部条目", () => {
    render(<ThreadTimeline snapshot={snapshot([
      item({ type: "agentMessage", channel: "commentary", text: "正在检查生命周期。" }),
      item({ type: "plan", content: "- 核对清理边界" }),
      item({ type: "file", path: "secret.txt" }),
      item({ type: "contextCompaction", beforeTokens: 10, afterTokens: 5, compactedAt: 1 }),
    ])} />)

    expect(screen.getByText("正在检查生命周期。")).toBeVisible()
    expect(screen.getByText("核对清理边界")).toBeVisible()
    expect(screen.queryByText("secret.txt")).not.toBeInTheDocument()
    expect(screen.queryByText(/Context compacted/)).not.toBeInTheDocument()
  })

  it("仅在 turn 运行时显示尾部活动行", () => {
    const active = snapshot([])
    active.activeTurn = {
      id: "turn-1",
      threadId: "thread-1",
      state: { status: "inProgress", phase: "runningTool" },
      startedAt: 0,
      updatedAt: 1,
      completedAt: null,
    }
    active.runtime = {
      threadId: "thread-1",
      usage: { model: "test", latestContextTokens: 0, promptTokens: 0, completionTokens: 0, cachedPromptTokens: 0, cacheWriteTokens: 0, cacheMissTokens: 0, reasoningTokens: 0, inferenceCount: 0, totalTokens: 0, hasUnpricedUsage: false, updatedAt: 1 },
      activeSkills: [],
      activeMcpServers: [],
      activeLspServers: [],
      progress: "正在执行测试",
      updatedAt: 1,
    }

    const { rerender } = render(<ThreadTimeline snapshot={active} />)
    expect(screen.getByText("Running tools")).toBeVisible()
    expect(screen.getByText(/正在执行测试/)).toBeVisible()

    rerender(<ThreadTimeline snapshot={snapshot([])} />)
    expect(screen.queryByText("Running tools")).not.toBeInTheDocument()
  })
})
