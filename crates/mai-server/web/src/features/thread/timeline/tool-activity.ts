/**
 * 单个工具调用条目 → ToolActivity 展示模型。
 *
 * 聚合三件事：结局判定（ToolOutcome 穷尽映射，替代字符串散落比较）、
 * 标题与一行摘要、以及由 tool-sections 构建的结构化分区。
 * 视图组件只消费本模块产出的模型，不做业务推导。
 */

import type { ThreadItem, ThreadItemContent } from "@/events/thread-events.generated"

import { asRecord, humanize, inlinePreview, numberValue, parseToolText, stringValue, type ToolJsonRecord } from "./tool-json"
import { buildToolInputSections, buildToolResultSections, type ToolSection } from "./tool-sections"

/** 工具调用的终局/进行状态；分组行据此统计失败数与活跃态。 */
export type ToolOutcome = "active" | "completed" | "failed" | "denied" | "timedOut" | "interrupted"

export interface ToolArtifactView {
  id?: string
  label: string
}

export interface ToolActivity {
  id: string
  title: string
  summary: string
  outcome: ToolOutcome
  sections: ToolSection[]
  artifacts: ToolArtifactView[]
  /** 原始 JSON 折叠区；仅在载荷是结构化 JSON 时给出。 */
  rawArguments: string | null
  rawResult: string | null
}

/** content.type === "toolCall" 的 ThreadItem。 */
export type ToolCallItem = ThreadItem & { content: Extract<ThreadItemContent, { type: "toolCall" }> }

const TOOL_TITLES: Record<string, string> = {
  exec: "Run command",
  write_stdin: "Continue process",
  read_file: "Read file",
  list_files: "List files",
  search_files: "Search files",
  apply_patch: "Apply patch",
  read_session_note: "Read session note",
  search_session_note: "Search session note",
  write_session_note: "Write session note",
  apply_session_note_patch: "Update session note",
  github_api_request: "GitHub API request",
  update_todo_list: "Update working list",
}

export function buildToolActivity(item: ToolCallItem): ToolActivity {
  const tool = item.content.tool
  const parsedArguments = parseToolText(tool.arguments)
  const parsedResult = parseToolText(tool.result)
  const argumentsRecord = asRecord(parsedArguments.value)
  const resultRecord = asRecord(parsedResult.value)
  const name = inferToolName(tool.name, argumentsRecord, resultRecord)

  const sections = [
    ...buildToolInputSections(name, parsedArguments),
    ...buildToolResultSections(name, parsedResult),
  ]
  if (item.error) sections.push({ kind: "text", title: "Error", text: item.error })
  if (tool.denialReason) sections.push({ kind: "text", title: "Denied", text: tool.denialReason })

  return {
    id: item.id,
    title: TOOL_TITLES[name] ?? humanize(name || "tool"),
    summary: toolSummary(name, argumentsRecord, resultRecord, parsedArguments.text, parsedResult.text),
    outcome: toolOutcome(item, resultRecord),
    sections,
    artifacts: artifactViews(tool.outputArtifacts),
    rawArguments: parsedArguments.structured ? tool.arguments ?? null : null,
    rawResult: parsedResult.structured ? tool.result ?? null : null,
  }
}

/** 失败性结局：用于分组行的失败计数与警示图标。 */
export function isFailedOutcome(outcome: ToolOutcome): boolean {
  return outcome === "failed" || outcome === "denied" || outcome === "timedOut" || outcome === "interrupted"
}

function toolOutcome(item: ToolCallItem, result: ToolJsonRecord | null): ToolOutcome {
  const tool = item.content.tool
  if (item.error) return "failed"
  if (tool.denialReason || item.status === "denied") return "denied"
  if (tool.timedOut || result?.timedOut === true) return "timedOut"
  const exitCode = tool.exitCode ?? numberValue(result?.exitCode)
  if (exitCode !== undefined && exitCode !== 0) return "failed"

  switch (item.status) {
    case "started":
    case "streaming":
    case "awaitingApproval":
    case "approved":
    case "running":
      return "active"
    case "failed":
    case "budgetLimited":
      return "failed"
    case "interrupted":
      return "interrupted"
    case "completed":
      return "completed"
  }
}

function artifactViews(artifacts: unknown[] | undefined): ToolArtifactView[] {
  if (!Array.isArray(artifacts)) return []
  return artifacts.flatMap((artifact, index) => {
    const record = asRecord(artifact)
    if (!record) return []
    const label = stringValue(record.filename) || stringValue(record.path) || stringValue(record.name) || stringValue(record.label) || `Artifact ${index + 1}`
    const id = stringValue(record.id) || undefined
    return [{ id, label }]
  })
}

/** 名称缺失时按载荷形状推断工具类型，保证未知工具也有可读展示。 */
function inferToolName(name: string | undefined, args: ToolJsonRecord | null, result: ToolJsonRecord | null): string {
  if (name?.trim()) return name.trim()
  if (result?.stdout !== undefined && result.stderr !== undefined && result.status !== undefined) return "exec"
  if (result?.contentHash !== undefined && result.revision !== undefined) {
    if (result.matches !== undefined) return "search_session_note"
    if (result.text !== undefined) return "read_session_note"
    return "apply_session_note_patch"
  }
  if (result?.files !== undefined && result.query !== undefined) return "search_files"
  if (result?.path !== undefined && result.text !== undefined) return "read_file"
  if (result?.changedFiles !== undefined || result?.added !== undefined) return "apply_patch"
  if (result?.html_url !== undefined || (result?.id !== undefined && result?.node_id !== undefined)) return "github_api_request"
  if (args?.method !== undefined && args.path !== undefined) return "github_api_request"
  return "tool"
}

function toolSummary(
  name: string,
  args: ToolJsonRecord | null,
  result: ToolJsonRecord | null,
  argumentText: string,
  resultText: string,
): string {
  switch (name) {
    case "exec":
      return inlinePreview(stringValue(args?.command) || stringValue(result?.message) || "Shell command")
    case "write_stdin":
      return inlinePreview(`Process ${stringValue(args?.processId) || stringValue(result?.processId) || "update"}`)
    case "read_file":
      return pathWithRange(args ?? result)
    case "list_files":
      return inlinePreview(stringValue(args?.path) || stringValue(result?.path) || "Workspace")
    case "search_files":
      return joinSummary(stringValue(args?.query) || stringValue(result?.query), stringValue(args?.path) || stringValue(result?.path))
    case "apply_patch":
      return patchPaths(args) || changedFilesSummary(result) || "Workspace changes"
    case "read_session_note":
      return lineRangeSummary(result ?? args) || "Session note"
    case "search_session_note":
      return joinSummary(stringValue(args?.query) || stringValue(result?.query), countSummary(result, "match"))
    case "write_session_note":
    case "apply_session_note_patch":
      return revisionSummary(result ?? args)
    case "github_api_request":
      return joinSummary(stringValue(args?.method) || "GET", stringValue(args?.path) || githubResultLabel(result))
    default:
      return genericSummary(args, result, argumentText, resultText)
  }
}

function genericSummary(args: ToolJsonRecord | null, result: ToolJsonRecord | null, argumentText: string, resultText: string): string {
  for (const record of [args, result]) {
    if (!record) continue
    for (const key of ["summary", "message", "path", "query", "status"]) {
      const value = stringValue(record[key])
      if (value) return inlinePreview(value)
    }
  }
  return inlinePreview(argumentText || resultText || "Tool activity")
}

function pathWithRange(record: ToolJsonRecord | null): string {
  if (!record) return "File"
  const path = stringValue(record.path) || "File"
  const range = lineRangeSummary(record)
  return range ? `${path} · ${range}` : path
}

function lineRangeSummary(record: ToolJsonRecord | null): string {
  if (!record) return ""
  const start = numberValue(record.startLine)
  const end = numberValue(record.endLine)
  if (start === undefined) return ""
  return end === undefined || end === start ? `line ${start}` : `lines ${start}–${end}`
}

function revisionSummary(record: ToolJsonRecord | null): string {
  if (!record) return "Session note"
  const revision = numberValue(record.revision ?? record.expectedRevision)
  const status = stringValue(record.status)
  return joinSummary(status, revision === undefined ? "Session note" : `revision ${revision}`)
}

function countSummary(record: ToolJsonRecord | null, noun: string): string {
  const count = numberValue(record?.count)
  return count === undefined ? "" : `${count} ${noun}${count === 1 ? "" : "es"}`
}

function patchPaths(record: ToolJsonRecord | null): string {
  const patch = stringValue(record?.input ?? record?.patch)
  if (!patch) return ""
  const paths = [...patch.matchAll(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/gm)].map((match) => match[1]).filter(Boolean)
  return paths.length > 0 ? inlinePreview(paths.join(", ")) : "Workspace changes"
}

function changedFilesSummary(record: ToolJsonRecord | null): string {
  const files = Array.isArray(record?.changedFiles) ? record.changedFiles.map((file) => stringValue(file) || "file") : []
  return files.length > 0 ? inlinePreview(files.join(", ")) : ""
}

function githubResultLabel(record: ToolJsonRecord | null): string {
  return stringValue(record?.html_url) || (record?.id === undefined ? "GitHub" : `Review ${inlinePreview(stringValue(record.id))}`)
}

function joinSummary(first: string, second: string): string {
  return inlinePreview([first, second].filter(Boolean).join(" · ") || "Tool activity")
}
