import { expect, type APIRequestContext } from "@playwright/test"

/**
 * live（真实隔离服务器）验收共用的地址守卫、DTO、夹具发现与 CAS 读写。
 *
 * 这里不碰任何 UI 选择器：只负责“地址必须指向本机隔离服务”、按 API 事实发现夹具，以及
 * 用 revision CAS 读写项目参考源绑定。`playwright.live.config.ts`（基地址守卫）与
 * `reference-sources-live.spec.ts`（旅程）共用本模块，避免两侧判定漂移。
 */

/** live 服务基地址的环境变量名。 */
export const LIVE_URL_ENVIRONMENT = "MAI_REFERENCES_LIVE_URL"

/** 默认基地址：主代理把隔离服务转发到本机 18080。 */
export const LIVE_URL_DEFAULT = "http://127.0.0.1:18080"

/** live 用例唯一允许的端口。 */
export const LIVE_URL_PORT = "18080"

/** 应用收敛的等待上限与轮询间隔。 */
export const APPLICATION_WAIT_TIMEOUT_MS = 600_000
export const APPLICATION_POLL_INTERVAL_MS = 1_000

/** 项目级应用终态：不再是 pending/applying，且没有环境仍在等待。 */
const TERMINAL_STATES = new Set(["applied", "failed", "deferred"])

/** 隔离服务上的参考源登记项（字段与 mai-protocol/src/references.rs 一致）。 */
export interface LiveReferenceSource {
  id: string
  name: string
  revision_label: string
  host_path: string
  state: string
  updated_at: string
  approved_for_agent_access: boolean
  file_count: number
  byte_count: number
  provenance: { kind: string; commit_sha?: string | null }
}

/** 项目参考源绑定条目。 */
export interface LiveReferenceBinding {
  id: string
  source_id: string
  alias: string
  description: string
  enabled: boolean
}

/** `GET/PUT /projects/{id}/references` 的唯一响应形状。 */
export interface LiveReferenceConfiguration {
  revision: number
  bindings: LiveReferenceBinding[]
}

/** `GET /projects/{id}/references/status` 的项目级应用事实。 */
export interface LiveReferenceStatus {
  project_id: string
  configuration: LiveReferenceConfiguration
  target_revision: number
  state: string
  counts: { applied: number; waiting: number; failed: number; deferred: number; no_environment: number }
  agents: Array<{ agent_id: string; applied_revision: number; state: string }>
  page: { offset: number; limit: number; total: number }
  observed_at: string
}

/** 部署侧参考源策略，管理 UI 只能读取。 */
export interface LiveReferenceSourcePolicy {
  allowed_roots: string[]
  reference_container_root: string
  alias_pattern: string
  max_bindings_per_project: number
}

/** 项目摘要里 live 旅程需要的字段。 */
export interface LiveProjectSummary {
  id: string
  name: string
  repository?: string
  repository_full_name?: string
}

/** 动态发现出来的夹具；缺失项由用例显式 skip。 */
export interface LiveFixture {
  projects: LiveProjectSummary[]
  sources: LiveReferenceSource[]
  /** 预期为 Linux 7.2.10 那棵树；隔离库没有匹配项时为 undefined。 */
  linuxSource: LiveReferenceSource | undefined
  /** 预期为 private 那棵树（只按 /private/i 匹配）；没有时为 null。 */
  privateSource: LiveReferenceSource | null
  projectA: LiveProjectSummary | undefined
  projectB: LiveProjectSummary | undefined
}

const fixturePromises = new Map<string, Promise<LiveFixture>>()

/** 读取 live 基地址；未配置时使用默认值。 */
export function resolveLiveBaseUrl(): string {
  const configured = process.env[LIVE_URL_ENVIRONMENT]?.trim()
  return configured && configured.length > 0 ? configured : LIVE_URL_DEFAULT
}

/** 解析并校验 live 基地址：只允许环回地址 + 18080，防止误连生产或远端主机。 */
export function assertLiveBaseUrl(value: string = resolveLiveBaseUrl()): URL {
  let parsed: URL
  try {
    parsed = new URL(value)
  } catch {
    throw new Error(`${LIVE_URL_ENVIRONMENT} 不是合法 URL：${value}`)
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error(`live 服务只支持 http(s)，收到 ${parsed.protocol}（${value}）`)
  }
  if (!isLoopbackHost(parsed.hostname)) {
    throw new Error(`live 用例只允许访问环回地址，收到 ${parsed.hostname}（${value}）`)
  }
  if (parsed.port !== LIVE_URL_PORT) {
    throw new Error(`live 用例只允许端口 ${LIVE_URL_PORT}，收到 ${parsed.port || "默认端口"}（${value}）`)
  }
  if (parsed.pathname !== "/" || parsed.search.length > 0 || parsed.hash.length > 0) {
    throw new Error(`live 服务基地址不能带路径、查询或片段：${value}`)
  }
  return parsed
}

/** 把 API 路径解析成 live 服务的绝对 URL；每次调用都重新执行守卫。 */
export function liveApiUrl(path: string): string {
  return new URL(path, assertLiveBaseUrl().origin).toString()
}

/**
 * 动态发现隔离产品库里的夹具。
 *
 * 依据当前绑定状态区分“已绑定项目（A）”与“无绑定项目（B）”，并对 ready 源按
 * linux/private 关键词排序；结果按基地址缓存一次，避免用例中途写入影响夹具身份判定。
 * 全部来自 API，不硬编码任何 UUID。
 */
export function discoverFixture(request: APIRequestContext): Promise<LiveFixture> {
  const cacheKey = liveApiUrl("/projects")
  const cached = fixturePromises.get(cacheKey)
  if (cached) return cached
  const created = loadFixture(request)
  fixturePromises.set(cacheKey, created)
  return created
}

async function loadFixture(request: APIRequestContext): Promise<LiveFixture> {
  const projects = await readJson<LiveProjectSummary[]>(request, "/projects")
  if (projects.length === 0) throw new Error("隔离产品库没有任何项目，无法执行真实参考源旅程")
  const sources = await readJson<LiveReferenceSource[]>(request, "/reference-sources")
  const readySources = sources.filter((source) => source.state === "ready")
  const linuxSource = readySources.find((source) => /linux|7\.2/i.test(sourceLabel(source))) ?? readySources[0]
  const privateSource = readySources.find((source) => /private/i.test(sourceLabel(source))) ?? null

  const bindingCounts = new Map<string, number>()
  for (const project of projects) {
    const configuration = await readJson<LiveReferenceConfiguration>(request, `/projects/${project.id}/references`)
    bindingCounts.set(project.id, configuration.bindings.length)
  }
  const preferredId = process.env.MAI_REFERENCES_LIVE_PROJECT_ID
  const preferred = preferredId ? projects.find((project) => project.id === preferredId) : undefined
  if (preferredId && (!preferred || !bindingCounts.get(preferred.id))) {
    throw new Error(`指定的隔离项目 ${preferredId} 不存在或没有参考源绑定`)
  }
  const projectA = preferred ?? projects.find((project) => (bindingCounts.get(project.id) ?? 0) > 0) ?? projects[0]
  const projectB = projects.find((project) => project.id !== projectA.id && (bindingCounts.get(project.id) ?? 0) === 0)
    ?? projects.find((project) => project.id !== projectA.id)

  return { projects, sources, linuxSource, privateSource, projectA, projectB }
}

function sourceLabel(source: LiveReferenceSource): string {
  return `${source.name} ${source.revision_label} ${source.host_path}`
}

/** 把项目绑定恢复成初始集合，并等待应用终态。已经一致时不写。 */
export async function restoreConfiguration(
  request: APIRequestContext,
  projectId: string,
  bindings: LiveReferenceBinding[],
): Promise<void> {
  const current = await readJson<LiveReferenceConfiguration>(request, `/projects/${projectId}/references`)
  if (!sameBindings(current.bindings, bindings)) {
    await putConfiguration(request, projectId, current.revision, bindings)
  }
  await waitForApplicationTerminal(request, projectId)
}

/** 等到项目应用进入终态：不是 pending/applying，且没有环境仍在 waiting。 */
export async function waitForApplicationTerminal(
  request: APIRequestContext,
  projectId: string,
): Promise<LiveReferenceStatus> {
  const deadline = Date.now() + APPLICATION_WAIT_TIMEOUT_MS
  let latest: LiveReferenceStatus | null = null
  while (Date.now() < deadline) {
    latest = await readJson<LiveReferenceStatus>(request, `/projects/${projectId}/references/status`)
    if (isTerminal(latest)) return latest
    await new Promise((resolve) => setTimeout(resolve, APPLICATION_POLL_INTERVAL_MS))
  }
  throw new Error(
    `项目 ${projectId} 的参考源应用在 ${APPLICATION_WAIT_TIMEOUT_MS}ms 内未收敛到终态：${describeStatus(latest)}`,
  )
}

function isTerminal(status: LiveReferenceStatus): boolean {
  return TERMINAL_STATES.has(status.state) && status.counts.waiting === 0
}

function describeStatus(status: LiveReferenceStatus | null): string {
  if (!status) return "没有取到 status"
  const { counts } = status
  return `state=${status.state} applied=${counts.applied} waiting=${counts.waiting} failed=${counts.failed} deferred=${counts.deferred} no_environment=${counts.no_environment}`
}

/** 用同项目、同绑定集合推进 durable revision（模拟“别人先保存了”）。 */
export function bumpConfiguration(
  request: APIRequestContext,
  projectId: string,
  current: LiveReferenceConfiguration,
  mutate: (bindings: LiveReferenceBinding[]) => LiveReferenceBinding[],
): Promise<LiveReferenceConfiguration> {
  return putConfiguration(request, projectId, current.revision, mutate(current.bindings))
}

/** 整体替换项目绑定（CAS）；非 200 直接失败并带上响应体，避免静默改动。 */
export async function putConfiguration(
  request: APIRequestContext,
  projectId: string,
  expectedRevision: number,
  bindings: LiveReferenceBinding[],
): Promise<LiveReferenceConfiguration> {
  const raw = await sendJson(request, "PUT", `/projects/${projectId}/references`, {
    expected_revision: expectedRevision,
    bindings,
  })
  expect(
    raw.status,
    `PUT /projects/${projectId}/references 期望 200，实际 ${raw.status}：${raw.text.slice(0, 400)}`,
  ).toBe(200)
  return JSON.parse(raw.text) as LiveReferenceConfiguration
}

/** GET 一个 live JSON 端点；非 200 直接失败。 */
export async function readJson<T>(request: APIRequestContext, path: string): Promise<T> {
  const raw = await sendJson(request, "GET", path)
  expect(raw.status, `GET ${path} 期望 200，实际 ${raw.status}：${raw.text.slice(0, 400)}`).toBe(200)
  return JSON.parse(raw.text) as T
}

interface RawResponse {
  status: number
  text: string
}

async function sendJson(
  request: APIRequestContext,
  method: "GET" | "PUT" | "POST",
  path: string,
  body?: unknown,
): Promise<RawResponse> {
  const response = await request.fetch(liveApiUrl(path), { method, data: body })
  return { status: response.status(), text: await response.text() }
}

/** 优先编辑指定源的 enabled 绑定；否则退回第一条 enabled 绑定，再退回第一条。 */
export function chooseEditableBinding(
  bindings: LiveReferenceBinding[],
  preferred: LiveReferenceSource | undefined,
): LiveReferenceBinding {
  const matched = preferred
    ? bindings.find((binding) => binding.enabled && binding.source_id === preferred.id)
    : undefined
  return matched ?? bindings.find((binding) => binding.enabled) ?? bindings[0]
}

/** 生成一个符合服务端 alias 规则、且不与现有别名冲突的值。 */
export function uniqueAlias(existing: string[], base: string): string {
  const taken = new Set(existing.map((alias) => alias.trim()))
  for (let index = 0; index < 50; index += 1) {
    const candidate = index === 0 ? base : `${base}-${index}`
    if (!taken.has(candidate)) return candidate
  }
  throw new Error(`无法为 ${base} 生成不冲突的 alias`)
}

/** 绑定的规范化签名，用于比较请求体与服务器事实。 */
export function bindingSignature(binding: LiveReferenceBinding) {
  return {
    id: binding.id,
    source_id: binding.source_id,
    alias: binding.alias.trim(),
    description: binding.description.trim(),
    enabled: binding.enabled,
  }
}

/** 规范化后逐条比较（顺序参与语义）。 */
export function sameBindings(left: LiveReferenceBinding[], right: LiveReferenceBinding[]): boolean {
  if (left.length !== right.length) return false
  return left.every((binding, index) => {
    const other = right[index]
    return binding.id === other.id
      && binding.source_id === other.source_id
      && binding.alias.trim() === other.alias.trim()
      && binding.description.trim() === other.description.trim()
      && binding.enabled === other.enabled
  })
}

/** 请求路径（不含查询）。 */
export function requestUrlPath(url: string): string {
  return new URL(url).pathname
}

/** 是否命中某个项目的参考源配置端点。 */
export function isReferenceConfigurationRequest(
  candidate: { method(): string; url(): string },
  projectId: string,
  method: string,
): boolean {
  return candidate.method() === method && requestUrlPath(candidate.url()) === `/projects/${projectId}/references`
}

function isLoopbackHost(hostname: string): boolean {
  return hostname === "127.0.0.1" || hostname === "localhost" || hostname === "[::1]" || hostname === "::1"
}
