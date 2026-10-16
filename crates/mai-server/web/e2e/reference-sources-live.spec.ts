import { expect, test, type APIRequestContext, type Page, type TestInfo } from "@playwright/test"

import {
  bindingSignature,
  bumpConfiguration,
  chooseEditableBinding,
  discoverFixture,
  isReferenceConfigurationRequest,
  liveApiUrl,
  readJson,
  requestUrlPath,
  restoreConfiguration,
  uniqueAlias,
  type LiveProjectSummary,
  type LiveReferenceBinding,
  type LiveReferenceConfiguration,
  type LiveReferenceSource,
  type LiveReferenceSourcePolicy,
} from "./reference-sources-live.helpers"

/**
 * 参考源 UI 的真实隔离服务器端到端验收（desktop 1440x900 / tablet 820x1180 / mobile 390x844）。
 *
 * 本文件使用真实 API；乱序验证旅程仅延迟真实响应的交付，不伪造响应内容。
 * 页面来自隔离服务自己的 release 前端，数据来自隔离产品库。写入只有两类，且都走 revision
 * CAS：通过 API 模拟“别人先保存了”，以及页面自身的真实保存；每个会写配置的旅程都会在
 * finally 里把项目绑定恢复成初始状态并等待应用终态，不删除任何项目、源或宿主内容。
 *
 * 夹具形状（隔离产品库准备，全部通过 API 动态发现，不硬编码 UUID）：两个测试项目，
 * 项目 A 绑定 linux/private 两个源、项目 B 不绑 private，source list 里有 Linux 7.2.10
 * 与 private 两棵树。缺失的夹具用 test.skip 如实标记为未覆盖，不伪造通过。
 *
 * 运行方式（在 crates/mai-server/web 下，先确认隔离服务已转发到 127.0.0.1:18080）：
 * `npx playwright test --config playwright.live.config.ts`。
 * 编辑目标优先选 Linux 参考源的绑定；产物落在 MAI_REFERENCES_LIVE_ARTIFACTS 指定的临时目录。
 */

const browserDiagnostics = new WeakMap<Page, { pageErrors: string[]; console: string[] }>()

test.beforeEach(async ({ page }) => {
  const diagnostics = { pageErrors: [] as string[], console: [] as string[] }
  browserDiagnostics.set(page, diagnostics)
  page.on("pageerror", (error) => diagnostics.pageErrors.push(error.message))
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") {
      diagnostics.console.push(`${message.type()}: ${message.text()}`)
    }
  })
})

test.afterEach(async ({ page }, testInfo) => {
  const diagnostics = browserDiagnostics.get(page)
  await testInfo.attach("browser-diagnostics", {
    body: JSON.stringify(diagnostics, null, 2),
    contentType: "application/json",
  })
  expect(diagnostics?.pageErrors ?? [], "真实页面不得存在未处理的运行时错误").toEqual([])
})

const REFERENCE_SOURCES_SETTINGS_MARKER =
  "Register fixed read-only source snapshots from the Docker host and track verification."
const APPLICATION_STATUS_SECTION = "section[aria-label='Reference application status']"
/** 应用摘要的两种合法文案：全部已应用，或按状态计数。 */
const APPLICATION_SUMMARY =
  /All \d+ agent environments match revision \d+\.|\d+ applied · \d+ waiting · \d+ failed · \d+ deferred · \d+ without an environment\./

test("设置页展示隔离服务登记的参考源事实", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  if (fixture.sources.length === 0) {
    test.skip(true, "隔离产品库没有登记任何参考源，设置页事实无对象可核对")
    return
  }
  const policy = await readJson<LiveReferenceSourcePolicy>(request, "/reference-source-policy")

  await openReferenceSourcesSettings(page)
  expect(fixture.sources.some((source) => source.name.includes("__maiReferenceXss")), "应包含真实登记的恶意 HTML 显示名夹具").toBe(true)
  for (const source of fixture.sources) {
    // 行定位同时约束 name 与 host_path：两者都是登记事实，避免嵌套前缀互相匹配。
    const row = page.getByRole("row").filter({ hasText: source.name }).filter({ hasText: source.host_path })
    await expect(row, `参考源 ${source.name} 的登记行应可见`).toBeVisible()
    await expect(row).toContainText(source.revision_label)
    await expect(row).toContainText(source.approved_for_agent_access ? "approved for agent access" : "not approved")
    await expect(row).toContainText(source.provenance.kind === "git_tree" ? "Git snapshot" : "Directory snapshot")
    if (source.state === "ready") {
      await expect(row).toContainText(`${source.file_count} files · ${source.byte_count} bytes`)
    }
    if (source.provenance.commit_sha) await expect(row).toContainText(source.provenance.commit_sha)
  }
  await capture(page, testInfo, "settings-reference-sources-live")
  await expect(page.locator('img[src="x"]')).toHaveCount(0)
  expect(await page.evaluate(() => (window as Window & { __maiReferenceXss?: unknown }).__maiReferenceXss)).toBeUndefined()

  // 策略块来自服务端配置，UI 不能扩大这些限制。
  const policySection = page.getByRole("heading", { name: "Deployment policy", exact: true }).locator("xpath=ancestor::section[1]")
  await expect(policySection).toContainText(`Allowed roots: ${policy.allowed_roots.join(", ") || "—"}`)
  await expect(policySection).toContainText(`Container root: ${policy.reference_container_root}`)
  await expect(policySection).toContainText(`Alias pattern: ${policy.alias_pattern}`)
  await expect(policySection).toContainText(`Max bindings per project: ${policy.max_bindings_per_project}`)
  await assertNoHorizontalOverflow(page)
})

test("较晚交付的真实验证响应不能覆盖较新的 ready 事实", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const source = fixture.sources.find((item) => item.name.includes("__maiReferenceXss"))
  expect(source, "必须先完成真实资料注入夹具准备").toBeDefined()
  if (!source) return
  // 先确认真实 SSE 已完成服务端订阅，避免极小目录验证在页面连接建立前完成。
  const connected = page.waitForResponse((response) => requestUrlPath(response.url()) === "/events/product" && response.status() === 200)
  await openReferenceSourcesSettings(page)
  await connected
  let release: () => void = () => undefined
  let acknowledge: () => void = () => undefined
  const held = new Promise<void>((resolve) => { release = resolve })
  const accepted = new Promise<void>((resolve) => { acknowledge = resolve })
  let status = 0
  // 请求和响应都来自真实服务器；仅延迟旧 202 的交付，不伪造内容。
  const pattern = `**/reference-sources/${source.id}/verify`
  await page.route(pattern, async (route) => {
    const response = await route.fetch()
    status = response.status()
    acknowledge()
    await held
    await route.fulfill({ response })
  })
  const refreshed = page.waitForResponse(async (response) => {
    if (requestUrlPath(response.url()) !== "/reference-sources" || response.request().method() !== "GET") return false
    const items = await response.json() as LiveReferenceSource[]
    return items.some((item) => item.id === source.id && item.state === "ready" && item.updated_at !== source.updated_at)
  })
  const row = page.getByRole("row").filter({ hasText: source.name }).filter({ hasText: source.host_path })
  try {
    await row.getByRole("button", { name: "Verify", exact: true }).click()
    await accepted
    expect(status).toBe(202)
    await refreshed
    await expect(row.locator("td").nth(1)).toContainText(/ready/i)
    release()
    await expect(row.getByRole("button", { name: "Verify", exact: true })).toBeEnabled()
    await expect(row.locator("td").nth(1)).toContainText(/ready/i)
    const final = await readJson<LiveReferenceSource>(request, `/reference-sources/${source.id}`)
    expect(final.state).toBe("ready")
    await capture(page, testInfo, "verification-response-order-live")
  } finally {
    release()
    await page.unrouteAll({ behavior: "wait" })
  }
})

test("项目 References 展示真实绑定并在保存后给出已保存与应用状态", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const project = fixture.projectA
  if (!project) {
    test.skip(true, "隔离产品库没有项目，无法执行项目 References 旅程")
    return
  }
  const original = await readJson<LiveReferenceConfiguration>(request, `/projects/${project.id}/references`)
  if (original.bindings.length === 0) {
    test.skip(true, `项目 ${project.name} 没有参考源绑定（夹具应准备 linux/private），跳过保存旅程`)
    return
  }

  await withRestoredConfiguration(request, project, original.bindings, testInfo, async () => {
    await openReferencesView(page, project.id)
    await expect(page.getByTestId("reference-configuration-summary")).toContainText(
      `Saved configuration revision ${original.revision}.`,
    )
    for (const binding of original.bindings) {
      const row = bindingRow(page, binding.id)
      await expect(row).toBeVisible()
      await expect(row.getByLabel("Alias")).toHaveValue(binding.alias)
      await expect(row).toContainText(`/references/${binding.alias}`)
      const source = fixture.sources.find((candidate) => candidate.id === binding.source_id)
      if (source) await expect(row.getByLabel("Source")).toContainText(source.name)
    }
    await capture(page, testInfo, "project-references-live")

    // 优先编辑 Linux 参考源的绑定：夹具里的 linux 树是这轮验收的主角。
    const target = chooseEditableBinding(original.bindings, fixture.linuxSource)
    const nextAlias = uniqueAlias(original.bindings.map((binding) => binding.alias), "draft-live")
    const row = bindingRow(page, target.id)
    await row.getByLabel("Alias").fill(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

    const saveResponse = page.waitForResponse((response) =>
      isReferenceConfigurationRequest(response.request(), project.id, "PUT"))
    const saveRequest = page.waitForRequest((candidate) =>
      isReferenceConfigurationRequest(candidate, project.id, "PUT"))
    await page.getByRole("button", { name: "Save and apply" }).click()

    const payload = (await saveRequest).postDataJSON() as {
      expected_revision: number
      bindings: LiveReferenceBinding[]
    }
    expect(payload.expected_revision).toBe(original.revision)
    expect(payload.bindings.map(bindingSignature)).toEqual(
      original.bindings.map((binding) =>
        bindingSignature(binding.id === target.id ? { ...binding, alias: nextAlias } : binding)),
    )

    const response = await saveResponse
    expect(response.status(), "页面保存应成功（409 走 CAS 冲突旅程）").toBe(200)
    const saved = (await response.json()) as LiveReferenceConfiguration
    expect(saved.revision).toBeGreaterThan(original.revision)

    // 已保存提示：草稿收敛、按钮失活、配置摘要前进到服务器返回的 revision。
    await expect(page.getByTestId("reference-draft-dirty")).toHaveCount(0)
    await expect(page.getByRole("button", { name: "Save and apply" })).toBeDisabled()
    await expect(page.getByTestId("reference-configuration-summary")).toContainText(
      `Saved configuration revision ${saved.revision}.`,
    )
    await expect(row.getByLabel("Alias")).toHaveValue(nextAlias)

    // 已保存 != 全部已应用：应用事实来自 status，只接受两种真实文案。
    await expect(page.locator(APPLICATION_STATUS_SECTION)).toContainText(`Configuration revision ${saved.revision} ·`)
    expect(
      ((await page.getByTestId("reference-application-summary").textContent()) ?? "").trim(),
      "应用摘要必须是已应用文案或计数文案",
    ).toMatch(APPLICATION_SUMMARY)
    await capture(page, testInfo, "project-references-saved-live")
  })
})

test("跨项目草稿互相独立，真实 SSE 失效只重新取数", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const projectA = fixture.projectA
  const projectB = fixture.projectB
  if (!projectA || !projectB) {
    test.skip(true, "隔离产品库只有一个项目，无法核对跨项目草稿隔离")
    return
  }
  const original = await readJson<LiveReferenceConfiguration>(request, `/projects/${projectA.id}/references`)
  const otherBindings = (await readJson<LiveReferenceConfiguration>(request, `/projects/${projectB.id}/references`)).bindings
  if (original.bindings.length === 0) {
    test.skip(true, `项目 ${projectA.name} 没有参考源绑定，跳过跨项目草稿旅程`)
    return
  }

  await withRestoredConfiguration(request, projectA, original.bindings, testInfo, async () => {
    page.on("dialog", (dialog) => void dialog.accept())
    // 记录页面每次重新取数拿到的 durable revision，作为 SSE 失效的证据。
    const observedRevisions: number[] = []
    page.on("response", (response) => {
      if (response.request().method() !== "GET") return
      if (requestUrlPath(response.url()) !== `/projects/${projectA.id}/references`) return
      void response
        .json()
        .then((body: unknown) => {
          const revision = (body as LiveReferenceConfiguration | null)?.revision
          if (typeof revision === "number") observedRevisions.push(revision)
        })
        .catch(() => undefined)
    })
    await openReferencesView(page, projectA.id)

    const target = chooseEditableBinding(original.bindings, fixture.linuxSource)
    const nextAlias = uniqueAlias(original.bindings.map((binding) => binding.alias), "draft-live")
    await bindingRow(page, target.id).getByLabel("Alias").fill(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

    // 切到项目 B：离开时有确认弹窗，由上面的 dialog handler 接受；草稿按项目保留。
    await selectProject(page, projectB)
    if (otherBindings.length === 0) {
      await expect(page.getByText("No reference sources bound")).toBeVisible()
    } else {
      await expect(bindingRow(page, otherBindings[0].id)).toBeVisible()
    }
    await expect(page.getByTestId("reference-draft-dirty")).toHaveCount(0)
    const privateSource = fixture.privateSource
    if (privateSource) {
      expect(
        otherBindings.some((binding) => binding.source_id === privateSource.id),
        `项目 ${projectB.name} 不应绑定 private 源`,
      ).toBe(false)
    }
    await capture(page, testInfo, "project-references-other-project-live")

    // 切回 A：草稿仍在，且仍是未保存状态。
    await selectProject(page, projectA)
    await expect(bindingRow(page, target.id).getByLabel("Alias")).toHaveValue(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

    // 真实 SSE：外部 API 推进 durable revision；面板没有手动刷新入口，
    // 只能靠 project_references_changed 失效后重新取数。
    const externalAlias = uniqueAlias(original.bindings.map((binding) => binding.alias), "server-live")
    const bumped = await bumpConfiguration(request, projectA.id, original, (bindings) =>
      bindings.map((binding) => (binding.id === target.id ? { ...binding, alias: externalAlias } : binding)))
    await expect
      .poll(() => observedRevisions.includes(bumped.revision), {
        message: "外部 API 推进 durable revision 后，页面必须经 SSE 失效重新取数并拿到新版本",
        timeout: 30_000,
      })
      .toBe(true)

    // 重新取数只更新基线，不覆盖用户草稿。
    await expect(page.getByTestId("reference-configuration-summary")).toContainText(
      `Saved configuration revision ${bumped.revision}.`,
    )
    await expect(bindingRow(page, target.id).getByLabel("Alias")).toHaveValue(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()
    await assertNoHorizontalOverflow(page)
    await capture(page, testInfo, "project-references-sse-refetch-live")
  })
})

test("真实 CAS 冲突保留草稿并可重载服务器版本", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const project = fixture.projectA
  if (!project) {
    test.skip(true, "隔离产品库没有项目，无法执行 CAS 冲突旅程")
    return
  }
  const original = await readJson<LiveReferenceConfiguration>(request, `/projects/${project.id}/references`)
  if (original.bindings.length === 0) {
    test.skip(true, `项目 ${project.name} 没有参考源绑定，跳过 CAS 冲突旅程`)
    return
  }

  await withRestoredConfiguration(request, project, original.bindings, testInfo, async () => {
    await openReferencesView(page, project.id)
    const target = chooseEditableBinding(original.bindings, fixture.linuxSource)
    const nextAlias = uniqueAlias(original.bindings.map((binding) => binding.alias), "draft-live")
    const row = bindingRow(page, target.id)
    await row.getByLabel("Alias").fill(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()

    // 同项目、同绑定集合，只改一个 alias：真实的“别人先保存了”场景。
    const serverAlias = uniqueAlias(original.bindings.map((binding) => binding.alias), "server-live")
    const external = await bumpConfiguration(request, project.id, original, (bindings) =>
      bindings.map((binding) => (binding.id === target.id ? { ...binding, alias: serverAlias } : binding)))
    expect(external.revision).toBeGreaterThan(original.revision)

    const conflictResponse = page.waitForResponse((response) =>
      isReferenceConfigurationRequest(response.request(), project.id, "PUT"))
    await page.getByRole("button", { name: "Save and apply" }).click()
    expect((await conflictResponse).status(), "页面仍以旧 revision 提交，服务端必须返回 409").toBe(409)

    const conflictAlert = page.locator("[data-slot='alert']").filter({ hasText: "Configuration revision conflict" })
    await expect(conflictAlert).toBeVisible()
    await expect(conflictAlert).toContainText(`server now has revision ${external.revision}`)
    // 冲突不清空草稿，也不自动覆盖服务器版本。
    await expect(row.getByLabel("Alias")).toHaveValue(nextAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toBeVisible()
    await capture(page, testInfo, "project-references-cas-conflict-live")

    await page.getByRole("button", { name: `Reload server revision ${external.revision}` }).click()
    await expect(row.getByLabel("Alias")).toHaveValue(serverAlias)
    await expect(page.getByTestId("reference-draft-dirty")).toHaveCount(0)
    await capture(page, testInfo, "project-references-cas-reloaded-live")
  })
})

test("停止并应用确认弹窗取消不触发后端请求", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const project = fixture.projectA
  if (!project) {
    test.skip(true, "隔离产品库没有项目，无法执行停止并应用弹窗旅程")
    return
  }

  await openReferencesView(page, project.id)
  await expect(page.locator(APPLICATION_STATUS_SECTION)).toBeVisible()

  const applyCalls: string[] = []
  page.on("request", (candidate) => {
    if (requestUrlPath(candidate.url()) === `/projects/${project.id}/references/apply`) {
      applyCalls.push(`${candidate.method()} ${candidate.url()}`)
    }
  })

  await page.getByRole("button", { name: "Stop tasks and apply now" }).click()
  const dialog = page.getByRole("alertdialog")
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText("Stop running tasks and apply references?")
  await expect(dialog.getByRole("button", { name: "Confirm stop and apply" })).toBeVisible()
  await capture(page, testInfo, "project-references-stop-and-apply-live")

  await dialog.getByRole("button", { name: "Cancel" }).click()
  await expect(page.getByRole("alertdialog")).toHaveCount(0)
  // 取消只关闭弹窗：面板保持原样，且没有任何 apply 请求发出。
  await expect(page.locator(APPLICATION_STATUS_SECTION)).toBeVisible()
  expect(applyCalls, "取消确认弹窗不得触发 /references/apply").toEqual([])
})

test("参考源界面在三个视口无横向溢出", async ({ page, request }, testInfo) => {
  const fixture = await discoverFixture(request)
  const project = fixture.projectA
  if (!project) {
    test.skip(true, "隔离产品库没有项目，无法核对项目 References 布局")
    return
  }

  await openReferencesView(page, project.id)
  const alias = page.locator("[data-reference-binding] input").first()
  await alias.fill(await alias.inputValue())
  await assertNoHorizontalOverflow(page)
  await capture(page, testInfo, "references-overflow-live")

  await openReferenceSourcesSettings(page)
  await assertNoHorizontalOverflow(page)
  await capture(page, testInfo, "settings-overflow-live")

  if (isCompactViewport(page)) {
    await openReferencesView(page, project.id)
    await page.getByRole("button", { name: "Open Projects" }).click()
    await expect(page.getByRole("dialog")).toBeVisible()
    await assertNoHorizontalOverflow(page)
    await capture(page, testInfo, "references-drawer-overflow-live")
  }
})

/** 打开项目 References 标签，并确认页面确实来自 live 隔离服务。 */
async function openReferencesView(page: Page, projectId: string) {
  await page.goto(`/projects/${encodeURIComponent(projectId)}?view=references`)
  expectLivePage(page)
  await expect(page.getByRole("heading", { name: "Reference sources" })).toBeVisible()
}

/** 打开设置页的参考源分区。 */
async function openReferenceSourcesSettings(page: Page) {
  await page.goto("/settings/reference-sources")
  expectLivePage(page)
  await expect(page.getByText(REFERENCE_SOURCES_SETTINGS_MARKER)).toBeVisible()
}

/** 页面必须由 live 基地址提供，防止误跑在 vite dev server 上。 */
function expectLivePage(page: Page) {
  expect(new URL(page.url()).origin, "页面必须来自 live 隔离服务").toBe(new URL(liveApiUrl("/")).origin)
}

/** SPA 内切换项目；窄视口先打开资源抽屉，未保存草稿的确认弹窗由调用方接受。 */
async function selectProject(page: Page, project: LiveProjectSummary) {
  const compact = isCompactViewport(page)
  if (compact) await page.getByRole("button", { name: "Open Projects" }).click()
  const sidebar = compact ? page.getByRole("dialog") : page.locator("[data-resource-sidebar]")
  await sidebar.getByRole("button", { name: new RegExp(escapeRegExp(project.name)) }).first().click()
  await expect(page).toHaveURL(new RegExp(`/projects/${escapeRegExp(project.id)}`))
  await page.getByRole("tab", { name: "References", exact: true }).click()
  await expect(page.getByRole("heading", { name: "Reference sources" })).toBeVisible()
}

function bindingRow(page: Page, bindingId: string) {
  return page.locator(`[data-reference-binding="${bindingId}"]`)
}

function isCompactViewport(page: Page): boolean {
  return (page.viewportSize()?.width ?? 0) < 1024
}

async function capture(page: Page, testInfo: TestInfo, name: string) {
  await expect(page.locator("body")).not.toHaveText("")
  await expect(page.locator("vite-error-overlay, nextjs-portal")).toHaveCount(0)
  const width = page.viewportSize()?.width ?? 0
  await page.screenshot({ path: testInfo.outputPath(`${name}-${width}.png`), fullPage: true })
}

/** 页面根节点不得出现横向滚动；移动端是硬性验收项。 */
async function assertNoHorizontalOverflow(page: Page) {
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const root = document.documentElement
          const rows = Array.from(document.querySelectorAll("[data-reference-binding]"))
          const escaping = rows.flatMap((row) => {
            const card = row.getBoundingClientRect()
            return Array.from(row.querySelectorAll("input, textarea, [data-slot='select-trigger']"))
              .filter((control) => {
                const rect = control.getBoundingClientRect()
                return rect.left < card.left - 1 || rect.right > card.right + 1
              })
          })
          const shifted = rows.filter((row) => {
            const rect = row.getBoundingClientRect()
            return rect.left < -1 || rect.right > root.clientWidth + 1
          })
          return Math.max(root.scrollWidth - root.clientWidth, root.scrollLeft, escaping.length * 2, shifted.length * 2)
        }),
      { message: "页面不得出现横向溢出" },
    )
    .toBeLessThanOrEqual(1)
}

/**
 * 跑完会写真实配置的旅程，并在 finally 里用 CAS 把项目绑定恢复到初始状态。
 *
 * 恢复顺序：重新读取当前 revision → 以它作为 expected_revision 整体替换回初始绑定 →
 * 等到项目应用状态收敛。用例本身已失败时，恢复失败只记录 annotation，避免掩盖首个错误。
 */
async function withRestoredConfiguration(
  request: APIRequestContext,
  project: LiveProjectSummary,
  bindings: LiveReferenceBinding[],
  testInfo: TestInfo,
  journey: () => Promise<void>,
): Promise<void> {
  let journeyError: unknown = null
  let restoreError: Error | null = null
  try {
    await journey()
  } catch (error) {
    journeyError = error
  } finally {
    try {
      await restoreConfiguration(request, project.id, bindings)
      testInfo.annotations.push({
        type: "restored-references",
        description: `项目 ${project.name} 已恢复 ${bindings.length} 条初始参考源绑定并等待收敛`,
      })
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      if (journeyError) {
        testInfo.annotations.push({ type: "restore-failed", description: message })
        console.error(`恢复项目 ${project.name} 的初始参考源绑定失败：${message}`)
      } else {
        restoreError = new Error(`恢复项目 ${project.name} 的初始参考源绑定失败：${message}`)
      }
    }
  }
  if (journeyError) throw journeyError
  if (restoreError) throw restoreError
}

/** 按字符转义正则元字符；项目名里可能出现 . - ( ) 等字符。 */
function escapeRegExp(value: string): string {
  const meta = "\\^$.*+?()[]{}|/"
  return Array.from(value, (char) => (meta.includes(char) ? `\\${char}` : char)).join("")
}
