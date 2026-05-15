import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import { chromium } from 'playwright'

const styles = await readFile(new URL('../../styles.css', import.meta.url), 'utf8')

const longActivity = [
  'Reasoning',
  '**Describing the Project Core** I am now zeroing in on how to introduce the mai-team project.',
  'I am starting by examining the architecture, as outlined in AGENTS.md and the crate structure.',
  'The goal is to create a concise, yet informative introduction that keeps streaming while the turn runs.'
].join(' · ')

const statusItems = [
  ['CTX', '128k'],
  ['USED', '118k'],
  ['LEFT', '10k'],
  ['MODEL', 'deepseek-reasoner']
]

function statusFixture() {
  const itemMarkup = statusItems
    .map(([label, value]) => `<div class="status-line-item"><span>${label}</span><strong>${value}</strong></div>`)
    .join('')

  return `<!doctype html>
    <html>
      <head>
        <meta charset="utf-8" />
        <style>
          ${styles}
          body {
            margin: 0;
          }
          .layout-harness {
            overflow: auto;
            width: 100vw;
          }
          .composer-shell {
            border-top: 1px solid #d8e0ea;
          }
          .composer-box {
            height: 88px;
            border: 1px solid #d8e0ea;
            margin: 16px 18px;
          }
        </style>
      </head>
      <body>
        <main class="layout-harness">
          <section class="context-status-line">
            <div class="status-line-left">
              <span class="status-line-state">
                <span class="status-dot green"></span>
                <strong>Running Turn</strong>
              </span>
              <span class="status-line-activity">${longActivity}</span>
            </div>
            <div class="status-line-items">${itemMarkup}</div>
          </section>
          <section class="composer-shell">
            <div class="composer-box"></div>
          </section>
        </main>
      </body>
    </html>`
}

const browser = await chromium.launch({ headless: true })

try {
  for (const width of [2048, 1280, 900, 600]) {
    const page = await browser.newPage({ viewport: { width, height: 220 } })
    await page.setContent(statusFixture())

    const metrics = await page.evaluate(() => {
      const status = document.querySelector('.context-status-line')
      const activity = document.querySelector('.status-line-activity')
      const items = document.querySelector('.status-line-items')
      const statusStyle = getComputedStyle(status)
      const activityStyle = getComputedStyle(activity)
      const itemsStyle = getComputedStyle(items)

      return {
        activityClientWidth: activity.clientWidth,
        activityFlexGrow: activityStyle.flexGrow,
        activityFlexShrink: activityStyle.flexShrink,
        activityMinWidth: activityStyle.minWidth,
        activityScrollWidth: activity.scrollWidth,
        documentClientWidth: document.documentElement.clientWidth,
        documentScrollWidth: document.documentElement.scrollWidth,
        itemsOverflowX: itemsStyle.overflowX,
        statusClientWidth: status.clientWidth,
        statusDisplay: statusStyle.display,
        statusScrollWidth: status.scrollWidth
      }
    })

    assert.equal(metrics.statusDisplay, 'grid')
    assert.equal(metrics.activityMinWidth, '0px')
    assert.equal(metrics.activityFlexGrow, '1')
    assert.equal(metrics.activityFlexShrink, '1')
    assert.notEqual(metrics.itemsOverflowX, 'auto')
    assert.ok(
      metrics.documentScrollWidth <= metrics.documentClientWidth,
      `status line should not create document overflow at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.ok(
      metrics.statusScrollWidth <= metrics.statusClientWidth,
      `status line should clip its own long content at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.ok(
      metrics.activityScrollWidth > metrics.activityClientWidth,
      `long activity should be visually ellipsized at ${width}px: ${JSON.stringify(metrics)}`
    )

    await page.close()
  }
} finally {
  await browser.close()
}
