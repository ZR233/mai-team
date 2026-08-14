/**
 * 工具调用载荷 → 展示分区的投影。
 *
 * 按工具名称（exec、search_files、apply_patch、session note、GitHub 等）
 * 把已解析的参数/结果组织成结构化分区；未知工具走通用规则。
 * 分区模型与构建逻辑都在本模块，tool-activity 只负责组装。
 */

import { asRecord, humanize, inlinePreview, numberValue, stringValue, valueText, type ParsedToolText, type ToolJsonRecord } from "./tool-json"

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

/** 大体积键不进入通用事实列表，由各自的专属分区呈现。 */
const GENERIC_OMITTED_KEYS = new Set(["body", "command", "content", "input", "matches", "files", "patch", "stderr", "stdout", "text"])

export function buildToolInputSections(name: string, parsed: ParsedToolText): ToolSection[] {
  const record = asRecord(parsed.value)
  if (!parsed.text) return []
  if (!record) return [{ kind: "text", title: "Input", text: valueText(parsed.value) }]

  switch (name) {
    case "exec":
      return compactSections([
        codeSection("Command", stringValue(record.command)),
        fieldsSection("Options", factsFrom(record, [
          ["cwd", "Working directory"],
          ["timeoutSeconds", "Timeout"],
          ["yieldTimeMs", "Yield time"],
          ["maxOutputChars", "Output limit"],
        ])),
      ])
    case "write_stdin":
      return compactSections([
        fieldsSection("Process", factsFrom(record, [
          ["processId", "Process"],
          ["yieldTimeMs", "Yield time"],
          ["maxOutputChars", "Output limit"],
        ])),
        codeSection("Input", stringValue(record.chars)),
      ])
    case "read_file":
    case "list_files":
    case "search_files":
      return compactSections([fieldsSection("Input", fileInputFacts(record))])
    case "apply_patch":
      return compactSections([
        fieldsSection("Options", factsFrom(record, [["cwd", "Working directory"]])),
        codeSection("Patch", stringValue(record.input ?? record.patch)),
      ])
    case "read_session_note":
    case "search_session_note":
      return compactSections([fieldsSection("Input", noteInputFacts(record))])
    case "write_session_note":
      return compactSections([
        fieldsSection("Revision", factsFrom(record, [["expectedRevision", "Expected revision"]])),
        markdownSection("Note", stringValue(record.content)),
      ])
    case "apply_session_note_patch":
      return compactSections([
        fieldsSection("Revision", factsFrom(record, [["expectedRevision", "Expected revision"]])),
        codeSection("Patch", stringValue(record.patch)),
      ])
    case "github_api_request":
      return githubInputSections(record)
    default:
      return compactSections([fieldsSection("Input", genericFacts(record))])
  }
}

export function buildToolResultSections(name: string, parsed: ParsedToolText): ToolSection[] {
  const record = asRecord(parsed.value)
  if (!parsed.text) return []
  if (!record) {
    if (Array.isArray(parsed.value)) {
      const items = parsed.value.map(valueText)
      return items.length > 0
        ? [{ kind: "list", title: "Result", items }]
        : [{ kind: "text", title: "Result", text: "No items returned." }]
    }
    return [{ kind: "text", title: "Result", text: valueText(parsed.value) }]
  }

  switch (name) {
    case "exec":
    case "write_stdin":
      return commandResultSections(record)
    case "read_file":
      return compactSections([
        fieldsSection("File", factsFrom(record, [
          ["path", "Path"],
          ["startLine", "Start line"],
          ["endLine", "End line"],
          ["nextStartLine", "Next line"],
        ])),
        codeSection("Content", stringValue(record.text)),
      ])
    case "list_files":
      return compactSections([
        fieldsSection("Result", factsFrom(record, [
          ["path", "Path"],
          ["glob", "Pattern"],
          ["count", "Items"],
        ])),
        listSection("Files", fileValues(record.files)),
      ])
    case "search_files":
      return compactSections([
        fieldsSection("Result", factsFrom(record, [
          ["query", "Query"],
          ["path", "Path"],
          ["count", "Matches"],
        ])),
        matchSection("Matches", fileMatches(record.files)),
      ])
    case "apply_patch":
      return compactSections([
        fieldsSection("Changes", patchFacts(record)),
        listSection("Changed files", fileValues(record.changedFiles)),
        codeSection("Output", stringValue(record.stdout)),
        codeSection("Errors", stringValue(record.stderr)),
      ])
    case "read_session_note":
      return compactSections([
        fieldsSection("Note", noteResultFacts(record)),
        markdownSection("Content", stringValue(record.text)),
      ])
    case "search_session_note":
      return compactSections([
        fieldsSection("Result", noteResultFacts(record)),
        matchSection("Matches", noteMatches(record.matches)),
      ])
    case "write_session_note":
    case "apply_session_note_patch":
      return compactSections([fieldsSection("Note", noteResultFacts(record))])
    case "github_api_request":
      return githubResultSections(record)
    default:
      return genericResultSections(record)
  }
}

function commandResultSections(record: ToolJsonRecord): ToolSection[] {
  return compactSections([
    fieldsSection("Result", factsFrom(record, [
      ["status", "Status"],
      ["exitCode", "Exit code"],
      ["processId", "Process"],
      ["outputFile", "Full output"],
    ])),
    codeSection("Standard output", stringValue(record.stdout)),
    codeSection("Standard error", stringValue(record.stderr)),
    !record.stdout && !record.stderr ? textSection("Message", stringValue(record.message)) : null,
  ])
}

function githubInputSections(record: ToolJsonRecord): ToolSection[] {
  const body = asRecord(record.body)
  const fields = factsFrom(record, [["method", "Method"], ["path", "Endpoint"]])
  if (body) {
    fields.push(...factsFrom(body, [
      ["event", "Review decision"],
      ["commit_id", "Commit"],
      ["comments", "Inline comments"],
    ]))
  }
  return compactSections([
    fieldsSection("Request", fields),
    markdownSection("Review body", stringValue(body?.body)),
  ])
}

function githubResultSections(record: ToolJsonRecord): ToolSection[] {
  const fields = factsFrom(record, [
    ["id", "Review ID"],
    ["state", "State"],
    ["submitted_at", "Submitted"],
    ["html_url", "Link"],
  ])
  const user = asRecord(record.user)
  if (user) fields.push(...factsFrom(user, [["login", "Author"]]))
  return compactSections([
    fieldsSection("GitHub response", fields),
    markdownSection("Review body", stringValue(record.body)),
  ])
}

function genericResultSections(record: ToolJsonRecord): ToolSection[] {
  const sections: Array<ToolSection | null> = [fieldsSection("Result", genericFacts(record))]
  for (const key of ["text", "content", "stdout", "stderr"] as const) {
    const text = stringValue(record[key])
    if (text) sections.push(codeSection(humanize(key), text))
  }
  for (const key of ["files", "items", "matches"] as const) {
    const items = fileValues(record[key])
    if (items.length > 0) sections.push(listSection(humanize(key), items))
  }
  return compactSections(sections)
}

function fileInputFacts(record: ToolJsonRecord): ToolFact[] {
  return factsFrom(record, [
    ["path", "Path"], ["query", "Query"], ["glob", "Pattern"],
    ["cwd", "Working directory"], ["startLine", "Start line"],
    ["maxLines", "Maximum lines"], ["contextLines", "Context lines"], ["limit", "Limit"],
  ])
}

function noteInputFacts(record: ToolJsonRecord): ToolFact[] {
  return factsFrom(record, [
    ["query", "Query"], ["expectedRevision", "Expected revision"], ["startLine", "Start line"],
    ["maxLines", "Maximum lines"], ["contextLines", "Context lines"], ["limit", "Limit"],
  ])
}

function noteResultFacts(record: ToolJsonRecord): ToolFact[] {
  return factsFrom(record, [
    ["status", "Status"], ["revision", "Revision"], ["startLine", "Start line"],
    ["endLine", "End line"], ["nextStartLine", "Next line"], ["totalLines", "Total lines"],
    ["totalBytes", "Bytes"], ["count", "Matches"],
  ])
}

function patchFacts(record: ToolJsonRecord): ToolFact[] {
  const facts = factsFrom(record, [["cwd", "Working directory"], ["summary", "Summary"]])
  for (const [key, label] of [["added", "Added"], ["updated", "Updated"], ["deleted", "Deleted"], ["moved", "Moved"]] as const) {
    if (Array.isArray(record[key])) facts.push({ label, value: String(record[key].length) })
  }
  return facts
}

function genericFacts(record: ToolJsonRecord): ToolFact[] {
  return Object.entries(record)
    .filter(([key]) => !GENERIC_OMITTED_KEYS.has(key))
    .slice(0, 12)
    .map(([key, value]) => ({ label: humanize(key), value: conciseValue(value) }))
}

function factsFrom(record: ToolJsonRecord, fields: Array<readonly [string, string]>): ToolFact[] {
  return fields.flatMap(([key, label]) => {
    const value = record[key]
    return value === null || value === undefined || value === "" ? [] : [{ label, value: conciseValue(value) }]
  })
}

function fileMatches(value: unknown): ToolMatch[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((file) => {
    const fileRecord = asRecord(file)
    if (!fileRecord || !Array.isArray(fileRecord.matches)) return []
    return fileRecord.matches.flatMap((match) => {
      const item = asRecord(match)
      if (!item) return []
      return [{
        path: stringValue(fileRecord.path) || undefined,
        line: numberValue(item.line),
        column: numberValue(item.column),
        text: stringValue(item.text) || conciseValue(item),
      }]
    })
  })
}

function noteMatches(value: unknown): ToolMatch[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((match) => {
    const item = asRecord(match)
    return item ? [{
      line: numberValue(item.line),
      column: numberValue(item.column),
      text: stringValue(item.text) || conciseValue(item),
    }] : []
  })
}

function fileValues(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  return value.map((item) => {
    const record = asRecord(item)
    return stringValue(record?.path) || stringValue(record?.name) || conciseValue(item)
  })
}

function conciseValue(value: unknown): string {
  if (Array.isArray(value)) return `${value.length} item${value.length === 1 ? "" : "s"}`
  const record = asRecord(value)
  if (record) return `${Object.keys(record).length} field${Object.keys(record).length === 1 ? "" : "s"}`
  return inlinePreview(valueText(value))
}

function fieldsSection(title: string, items: ToolFact[]): ToolSection {
  return { kind: "fields", title, items }
}

function codeSection(title: string, text: string): ToolSection | null {
  return text ? { kind: "code", title, text } : null
}

function markdownSection(title: string, text: string): ToolSection | null {
  return text ? { kind: "markdown", title, text } : null
}

function listSection(title: string, items: string[]): ToolSection {
  return { kind: "list", title, items }
}

function matchSection(title: string, items: ToolMatch[]): ToolSection {
  return { kind: "matches", title, items }
}

function textSection(title: string, text: string): ToolSection | null {
  return text ? { kind: "text", title, text } : null
}

/** 过滤空分区，保证每个分区都有可见内容。 */
function compactSections(sections: Array<ToolSection | null>): ToolSection[] {
  return sections.filter((section): section is ToolSection => Boolean(section && sectionHasContent(section)))
}

function sectionHasContent(section: ToolSection): boolean {
  if (section.kind === "fields" || section.kind === "list" || section.kind === "matches") return section.items.length > 0
  return Boolean(section.text)
}
