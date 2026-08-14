/**
 * 工具调用 JSON 载荷的解析原语。
 *
 * 工具的 `arguments` 与 `result` 在协议中是字符串：可能是纯文本，也可能是
 * （偶发双重编码后的）JSON。本模块统一负责"尽力解析 + 安全取值"，供
 * tool-sections 与 tool-activity 投影使用，避免解析细节散落各处。
 */

export type ToolJsonRecord = Record<string, unknown>

export interface ParsedToolText {
  /** 解析结果；无法解析时为原始文本，结构化解析后为 JSON 值。 */
  value: unknown
  /** 原始（去除首尾空白后的）文本；空输入为空字符串。 */
  text: string
  /** 是否成功解析出结构化 JSON。 */
  structured: boolean
}

/** 解析工具载荷文本：最多剥两层 JSON 字符串包装，失败则按纯文本处理。 */
export function parseToolText(value?: string): ParsedToolText {
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

/** 将原始载荷格式化为可读 JSON；非结构化文本原样返回。 */
export function formatToolJson(value: string): string {
  const parsed = parseToolText(value)
  if (!parsed.structured) return value
  try {
    return JSON.stringify(parsed.value, null, 2)
  } catch {
    return value
  }
}

export function asRecord(value: unknown): ToolJsonRecord | null {
  return isRecord(value) ? value : null
}

export function isRecord(value: unknown): value is ToolJsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export function stringValue(value: unknown): string {
  return typeof value === "string" ? value : ""
}

export function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined
}

/** 把任意 JSON 值压缩成一行短预览，用于事实列表与兜底摘要。 */
export function conciseValue(value: unknown): string {
  if (Array.isArray(value)) return `${value.length} item${value.length === 1 ? "" : "s"}`
  if (isRecord(value)) return `${Object.keys(value).length} field${Object.keys(value).length === 1 ? "" : "s"}`
  return inlinePreview(valueText(value))
}

export function valueText(value: unknown): string {
  if (value === null || value === undefined) return ""
  if (typeof value === "string") return value
  if (typeof value === "number" || typeof value === "boolean") return String(value)
  return conciseValue(value)
}

/** 压缩空白并截断到单行预览。 */
export function inlinePreview(value: string, max = 140): string {
  const singleLine = value.replace(/\s+/g, " ").trim()
  return singleLine.length <= max ? singleLine : `${singleLine.slice(0, max - 1)}…`
}

/** 把标识符转为人类可读标题："apply_patch" → "Apply patch"。 */
export function humanize(value: string): string {
  const text = value.replace(/([a-z0-9])([A-Z])/g, "$1 $2").replaceAll("_", " ").replaceAll("-", " ").trim()
  return text ? text[0]!.toUpperCase() + text.slice(1) : "Tool"
}
