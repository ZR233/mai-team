import type { ToolFact, ToolMatch, ToolSection } from "./tool-presentation"

export type ToolJsonRecord = Record<string, unknown>

interface ParsedToolText {
  value: unknown
  text: string
}

const GENERIC_OMITTED_KEYS = new Set([
  "body",
  "command",
  "content",
  "input",
  "matches",
  "files",
  "patch",
  "stderr",
  "stdout",
  "text",
])

export function buildInputSections(
  name: string,
  parsed: ParsedToolText,
  record: ToolJsonRecord | null,
): ToolSection[] {
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

export function buildResultSections(
  name: string,
  parsed: ParsedToolText,
  record: ToolJsonRecord | null,
): ToolSection[] {
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
        path: stringValue(fileRecord.path),
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
  if (isRecord(value)) return `${Object.keys(value).length} field${Object.keys(value).length === 1 ? "" : "s"}`
  return inlinePreview(valueText(value))
}

function valueText(value: unknown): string {
  if (value === null || value === undefined) return ""
  if (typeof value === "string") return value
  if (typeof value === "number" || typeof value === "boolean") return String(value)
  return conciseValue(value)
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

function compactSections(sections: Array<ToolSection | null>): ToolSection[] {
  return sections.filter((section): section is ToolSection => Boolean(section && sectionHasContent(section)))
}

function sectionHasContent(section: ToolSection): boolean {
  if (section.kind === "fields" || section.kind === "list" || section.kind === "matches") return section.items.length > 0
  return Boolean(section.text)
}

function asRecord(value: unknown): ToolJsonRecord | null {
  return isRecord(value) ? value : null
}

function isRecord(value: unknown): value is ToolJsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function stringValue(value: unknown): string {
  return typeof value === "string" ? value : ""
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined
}

function inlinePreview(value: string, max = 140): string {
  const singleLine = value.replace(/\s+/g, " ").trim()
  return singleLine.length <= max ? singleLine : `${singleLine.slice(0, max - 1)}…`
}

function humanize(value: string): string {
  const text = value.replace(/([a-z0-9])([A-Z])/g, "$1 $2").replaceAll("_", " ").replaceAll("-", " ").trim()
  return text ? text[0]!.toUpperCase() + text.slice(1) : "Tool"
}
