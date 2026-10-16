import { defineConfig, devices } from "@playwright/test"

import { assertLiveBaseUrl, resolveLiveBaseUrl } from "./e2e/reference-sources-live.helpers"

const artifacts = process.env.MAI_REFERENCES_LIVE_ARTIFACTS ?? "/tmp/mai-references-live"

/**
 * 参考源 UI 的 live 验收配置：跑在真实隔离服务上，不启动 vite、不 mock 任何 API。
 *
 * - 基地址由 `MAI_REFERENCES_LIVE_URL` 提供（默认 http://127.0.0.1:18080），且必须是环回
 *   地址 + 18080：配置加载时就守卫，避免误连生产 8080 或远端主机。
 * - 服务器自己提供 release 前端，因此这里没有 webServer。
 * - 用例会写真实项目配置，所以强制单 worker、非并行、不重试，避免互相覆盖夹具状态。
 * - 产物单独落到 MAI_REFERENCES_LIVE_ARTIFACTS 指定的临时目录（含 trace 与每视口截图），与 mock 用例的
 *   test-results/playwright-report 互不干扰。
 */
export default defineConfig({
  testDir: "./e2e",
  testMatch: ["reference-sources-live.spec.ts"],
  fullyParallel: false,
  workers: 1,
  forbidOnly: true,
  retries: 0,
  timeout: 900_000,
  expect: { timeout: 20_000 },
  outputDir: `${artifacts}/test-results`,
  reporter: [["line"], ["json", { outputFile: `${artifacts}/results.json` }]],
  use: {
    baseURL: assertLiveBaseUrl(resolveLiveBaseUrl()).origin,
    trace: "on",
    screenshot: "on",
    video: "retain-on-failure",
    actionTimeout: 20_000,
  },
  projects: [
    { name: "live-desktop", use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 } } },
    { name: "live-tablet", use: { ...devices["Desktop Chrome"], viewport: { width: 820, height: 1180 }, hasTouch: true } },
    { name: "live-mobile", use: { ...devices["Desktop Chrome"], viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true } },
  ],
})
