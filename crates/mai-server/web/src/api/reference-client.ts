/**
 * 参考源码管理 API 的请求助手。
 *
 * 与通用 `api()` 的差别只有一处：失败响应按 `ReferenceErrorResponse` 契约解析，保留
 * `code`/`field_errors`/`retryable`，使 UI 能区分 CAS 冲突（409）与校验失败（422），
 * 而不是只拿到一段文本。成功路径与 `api()` 保持相同的 JSON 解析语义。
 */

import type { ReferenceErrorCode, ReferenceErrorResponse } from "@/api/reference-types"

/** 带 typed 错误体的参考源码 API 异常。 */
export class ReferenceApiError extends Error {
  constructor(
    readonly status: number,
    readonly response: ReferenceErrorResponse | null,
    fallbackMessage: string,
  ) {
    super(response?.message || fallbackMessage)
    this.name = "ReferenceApiError"
  }

  /** 稳定机器码；响应体不可解析时为 `null`。 */
  get code(): ReferenceErrorCode | null {
    return this.response?.code ?? null
  }

  /** 该请求在条件不变时是否可能成功；响应体不可解析时按 5xx 判断。 */
  get retryable(): boolean {
    return this.response?.retryable ?? this.status >= 500
  }

  /** 是否与配置版本冲突（服务端已有更新的 durable 配置）。 */
  get isRevisionConflict(): boolean {
    return this.status === 409 && this.code === "reference_revision_conflict"
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

function parseErrorResponse(text: string): ReferenceErrorResponse | null {
  if (!text.trim()) return null
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch {
    return null
  }
  if (!isRecord(parsed) || typeof parsed.code !== "string" || typeof parsed.message !== "string") {
    return null
  }
  return {
    code: parsed.code as ReferenceErrorCode,
    message: parsed.message,
    field_errors: Array.isArray(parsed.field_errors)
      ? parsed.field_errors.flatMap((entry) => {
        if (!isRecord(entry) || typeof entry.field !== "string" || typeof entry.message !== "string") return []
        return [{ field: entry.field, message: entry.message }]
      })
      : [],
    request_id: typeof parsed.request_id === "string" ? parsed.request_id : null,
    retryable: parsed.retryable === true,
  }
}

/**
 * 发送参考源码管理请求。
 *
 * `path` 是项目既有的无 `/api` 前缀形态；`202` 的响应体若是空文本则返回 `null`。
 */
export async function referenceApi<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: {
      accept: "application/json",
      ...(init.body ? { "content-type": "application/json" } : {}),
      ...init.headers,
    },
  })
  const text = await response.text()
  if (!response.ok) {
    throw new ReferenceApiError(response.status, parseErrorResponse(text), response.statusText || `HTTP ${response.status}`)
  }
  if (response.status === 204 || response.status === 205) return null as T
  return text.trim() ? (JSON.parse(text) as T) : (null as T)
}

/** 把 JSON 请求体编码成 `RequestInit` 片段。 */
export function referenceJsonBody(value: unknown): Pick<RequestInit, "body"> {
  return { body: JSON.stringify(value) }
}
