import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import { chromium } from 'playwright'

const styles = await readFile(new URL('../../styles.css', import.meta.url), 'utf8')

const longStatus = 'SyncingReviewerWorkspaceAndWaitingForRequiredPullRequestStatusChecks'
const longOutcome = 'ReviewSubmittedAfterAllRequiredStatusChecksCompletedSuccessfully'
const longSummary = [
  'PR #128: validate project review with a large status set',
  'ci/linux-x86_64, ci/linux-aarch64, clippy, docs, integration, container build, security audit'
].join(' - ')

function reviewRunsFixture() {
  const runs = Array.from({ length: 12 }, (_, index) => `
    <article class="review-run-card">
      <button type="button" class="review-run-summary">
        <span class="review-run-status active">${longStatus}-${index}</span>
        <span class="review-run-main">
          <strong>${longSummary}</strong>
          <small>5/18/2026, 10:${String(index).padStart(2, '0')}:00 AM · ${index + 1}m</small>
        </span>
        <span class="review-run-outcome">${longOutcome}-${index}</span>
      </button>
    </article>
  `).join('')

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
            width: 100vw;
            height: 100vh;
            overflow: hidden;
            background: #eef2f7;
          }
        </style>
      </head>
      <body>
        <main class="layout-harness">
          <section class="project-panel">
            <section class="review-runs">
              <header class="review-runs-head">
                <div>
                  <h3>Recent Runs</h3>
                  <p>Kept for 5 days</p>
                </div>
                <span>12</span>
              </header>
              <div class="review-run-list">${runs}</div>
            </section>
          </section>
        </main>
      </body>
    </html>`
}

const browser = await chromium.launch({ headless: true })

try {
  for (const width of [720, 420]) {
    const page = await browser.newPage({ viewport: { width, height: 560 } })
    await page.setContent(reviewRunsFixture())

    const metrics = await page.evaluate(() => {
      const summaries = [...document.querySelectorAll('.review-run-summary')]
      const first = summaries[0]
      const main = first.querySelector('.review-run-main')

      return {
        documentClientWidth: document.documentElement.clientWidth,
        documentScrollWidth: document.documentElement.scrollWidth,
        firstMainWidth: main.getBoundingClientRect().width,
        firstSummaryClientWidth: first.clientWidth,
        firstSummaryScrollWidth: first.scrollWidth,
        summaryOverflowCount: summaries.filter((summary) => summary.scrollWidth > summary.clientWidth).length
      }
    })

    assert.ok(
      metrics.documentScrollWidth <= metrics.documentClientWidth,
      `review run list should not create page overflow at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.equal(
      metrics.summaryOverflowCount,
      0,
      `review run status chips should stay inside their cards at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.ok(
      metrics.firstMainWidth >= Math.min(220, metrics.firstSummaryClientWidth - 24),
      `PR title should remain readable beside long status chips at ${width}px: ${JSON.stringify(metrics)}`
    )

    await page.close()
  }
} finally {
  await browser.close()
}
