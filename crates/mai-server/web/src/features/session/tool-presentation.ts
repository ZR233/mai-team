import { buildInputSections, buildResultSections, type ToolJsonRecord } from "./tool-presentation-sections"

export interface ToolPresentationInput {
  name?: string
  arguments?: string
  result?: string
  status?: string
  exitCode?: number
  timedOut?: boolean
  workingDirectory?: string
  denialReason?: string
  error?: string
  outputArtifacts?: Record<string, unknown>[]
}

export interface ToolFact {
  label: string
  value: string
}

export interface ToolMatch {
  path?: string
  line?: number
  column?: number
  text: string
}

export type ToolSection =
  | { kind: "fields"; title: string; items: ToolFact[] }
  | { kind: "code"; title: string; text: string }
  | { kind: "markdown"; title: string; text: string }
  | { kind: "list"; title: string; items: string[] }
  | { kind: "matches"; title: string; items: ToolMatch[] }
  | { kind: "text"; title: string; text: string }

export interface ToolPresentationModel {
  name: string
  title: string
  summary: string
  status: string
  failed: boolean
  facts: ToolFact[]
  sections: ToolSection[]
  rawArguments?: string
  rawResult?: string
  outputArtifacts: Record<string, unknown>[]
}

type JsonRecord = ToolJsonRecord

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

export function buildToolPresentation(input: ToolPresentationInput): ToolPresentationModel {
  const parsedArguments = parseToolText(input.arguments)
  const parsedResult = parseToolText(input.result)
  const argumentsRecord = asRecord(parsedArguments.value)
  const resultRecord = asRecord(parsedResult.value)
  const name = inferToolName(input.name, argumentsRecord, resultRecord)
  const status = presentationStatus(input, resultRecord)
  const facts = commonFacts(input, resultRecord)
  const sections = [
    ...buildInputSections(name, parsedArguments, argumentsRecord),
    ...buildResultSections(name, parsedResult, resultRecord),
  ]
  if (input.error) sections.push({ kind: "text", title: "Error", text: input.error })
  if (input.denialReason) sections.push({ kind: "text", title: "Denied", text: input.denialReason })

  return {
    name,
    title: TOOL_TITLES[name] ?? humanize(name || "tool"),
    summary: toolSummary(name, argumentsRecord, resultRecord, parsedArguments.text, parsedResult.text),
    status,
    failed: isFailure(status, input, resultRecord),
    facts,
    sections,
    rawArguments: parsedArguments.structured ? input.arguments : undefined,
    rawResult: parsedResult.structured ? input.result : undefined,
    outputArtifacts: input.outputArtifacts ?? [],
  }
}

export function parseToolText(value?: string): { value: unknown; text: string; structured: boolean } {
  const text = value?.trim() ?? ""
  if (!text) return { value: null, text: "", structured: false }
  let parsed: unknown = text
  let structured = false
  for (let depth = 0; depth < 2 && typeof parsed === "string"; depth += 1) {
    try {
      parsed = JSON.parse(parsed)
      structured = true
    } catch {
      break
    }
  }
  return { value: parsed, text, structured }
}

export function formatRawToolData(value: string): string {
  const parsed = parseToolText(value)
  if (!parsed.structured) return value
  try {
    return JSON.stringify(parsed.value, null, 2)
  } catch {
    return value
  }
}


function toolSummary(
  name: string,
  args: JsonRecord | null,
  result: JsonRecord | null,
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

function inferToolName(name: string | undefined, args: JsonRecord | null, result: JsonRecord | null): string {
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

function presentationStatus(input: ToolPresentationInput, result: JsonRecord | null): string {
  if (input.error) return "failed"
  if (input.denialReason) return "denied"
  if (input.timedOut || result?.timedOut === true) return "timed out"
  if (input.exitCode !== undefined && input.exitCode !== 0) return "failed"
  if (typeof result?.exitCode === "number" && result.exitCode !== 0) return "failed"
  const partStatus = normalizedStatus(input.status)
  if (["failed", "denied", "interrupted", "budget limited"].includes(partStatus)) return partStatus
  return normalizedStatus(stringValue(result?.status) || input.status) || "completed"
}

function commonFacts(input: ToolPresentationInput, result: JsonRecord | null): ToolFact[] {
  const facts: ToolFact[] = []
  const exitCode = input.exitCode ?? numberValue(result?.exitCode)
  if (exitCode !== undefined) facts.push({ label: "Exit code", value: String(exitCode) })
  const cwd = input.workingDirectory || stringValue(result?.workingDirectory)
  if (cwd) facts.push({ label: "Working directory", value: cwd })
  const process = stringValue(result?.processId)
  if (process) facts.push({ label: "Process", value: process })
  return facts
}

function isFailure(status: string, input: ToolPresentationInput, result: JsonRecord | null): boolean {
  const normalized = status.toLowerCase().replaceAll(" ", "")
  return Boolean(
    input.error
    || input.denialReason
    || input.timedOut
    || result?.timedOut === true
    || (input.exitCode !== undefined && input.exitCode !== 0)
    || (typeof result?.exitCode === "number" && result.exitCode !== 0)
    || ["failed", "error", "denied", "timedout", "interrupted", "budgetlimited"].includes(normalized),
  )
}

function genericSummary(args: JsonRecord | null, result: JsonRecord | null, argumentText: string, resultText: string): string {
  for (const record of [args, result]) {
    if (!record) continue
    for (const key of ["summary", "message", "path", "query", "status"]) {
      const value = stringValue(record[key])
      if (value) return inlinePreview(value)
    }
  }
  return inlinePreview(argumentText || resultText || "Tool activity")
}

function pathWithRange(record: JsonRecord | null): string {
  if (!record) return "File"
  const path = stringValue(record.path) || "File"
  const range = lineRangeSummary(record)
  return range ? `${path} · ${range}` : path
}

function lineRangeSummary(record: JsonRecord | null): string {
  if (!record) return ""
  const start = numberValue(record.startLine)
  const end = numberValue(record.endLine)
  if (start === undefined) return ""
  return end === undefined || end === start ? `line ${start}` : `lines ${start}–${end}`
}

function revisionSummary(record: JsonRecord | null): string {
  if (!record) return "Session note"
  const revision = numberValue(record.revision ?? record.expectedRevision)
  const status = stringValue(record.status)
  return joinSummary(status, revision === undefined ? "Session note" : `revision ${revision}`)
}

function countSummary(record: JsonRecord | null, noun: string): string {
  const count = numberValue(record?.count)
  return count === undefined ? "" : `${count} ${noun}${count === 1 ? "" : "es"}`
}

function patchPaths(record: JsonRecord | null): string {
  const patch = stringValue(record?.input ?? record?.patch)
  if (!patch) return ""
  const paths = [...patch.matchAll(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/gm)].map(match => match[1]).filter(Boolean)
  return paths.length > 0 ? inlinePreview(paths.join(", ")) : "Workspace changes"
}

function changedFilesSummary(record: JsonRecord | null): string {
  const files = arrayValues(record?.changedFiles)
  return files.length > 0 ? inlinePreview(files.join(", ")) : ""
}

function githubResultLabel(record: JsonRecord | null): string {
  return stringValue(record?.html_url) || (record?.id === undefined ? "GitHub" : `Review ${conciseValue(record.id)}`)
}

function joinSummary(first: string, second: string): string {
  return inlinePreview([first, second].filter(Boolean).join(" · ") || "Tool activity")
}

function inlinePreview(value: string, max = 140): string {
  const singleLine = value.replace(/\s+/g, " ").trim()
  return singleLine.length <= max ? singleLine : `${singleLine.slice(0, max - 1)}…`
}

function normalizedStatus(value?: string): string {
  return value?.replace(/([a-z])([A-Z])/g, "$1 $2").replaceAll("_", " ").toLowerCase() ?? ""
}

function conciseValue(value: unknown): string {
  if (Array.isArray(value)) return `${value.length} item${value.length === 1 ? "" : "s"}`
  if (isRecord(value)) return `${Object.keys(value).length} field${Object.keys(value).length === 1 ? "" : "s"}`
  return inlinePreview(valueText(value))
}

function arrayValues(value: unknown): string[] {
  return Array.isArray(value) ? value.map(item => conciseValue(item)) : []
}

function stringValue(value: unknown): string {
  return typeof value === "string" ? value : ""
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined
}

function valueText(value: unknown): string {
  if (value === null || value === undefined) return ""
  if (typeof value === "string") return value
  if (typeof value === "number" || typeof value === "boolean") return String(value)
  return conciseValue(value)
}

function asRecord(value: unknown): JsonRecord | null {
  return isRecord(value) ? value : null
}

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function humanize(value: string): string {
  const text = value.replace(/([a-z0-9])([A-Z])/g, "$1 $2").replaceAll("_", " ").replaceAll("-", " ").trim()
  return text ? text[0]!.toUpperCase() + text.slice(1) : "Tool"
}
