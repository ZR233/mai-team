export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
    this.name = "ApiError"
  }
}

export async function api<T>(path: string, init: RequestInit & { timeoutMs?: number } = {}): Promise<T> {
  const { timeoutMs, ...requestInit } = init
  const controller = timeoutMs ? new AbortController() : undefined
  const timer = timeoutMs
    ? window.setTimeout(() => controller?.abort(), timeoutMs)
    : undefined
  try {
    const response = await fetch(path, {
      ...requestInit,
      signal: controller?.signal ?? requestInit.signal,
      headers: {
        accept: "application/json",
        ...(requestInit.body ? { "content-type": "application/json" } : {}),
        ...requestInit.headers,
      },
    })
    if (!response.ok) {
      const text = await response.text()
      let message = text || response.statusText
      try {
        const parsed = JSON.parse(text) as { error?: string }
        message = parsed.error ?? message
      } catch {
        // Plain-text API errors are already suitable for display.
      }
      throw new ApiError(message, response.status)
    }
    if (response.status === 204 || response.status === 205) return null as T
    const text = await response.text()
    return text.trim() ? (JSON.parse(text) as T) : (null as T)
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") {
      throw new Error("Request timed out.")
    }
    throw error
  } finally {
    if (timer !== undefined) window.clearTimeout(timer)
  }
}

export function jsonBody(value: unknown): Pick<RequestInit, "body"> {
  return { body: JSON.stringify(value) }
}
