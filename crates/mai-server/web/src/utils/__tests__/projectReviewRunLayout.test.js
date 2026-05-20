import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'

import { chromium } from 'playwright'

const styles = await readFile(new URL('../../styles.css', import.meta.url), 'utf8')

const longStatus = 'SyncingReviewerWorkspaceAndWaitingForRequiredPullRequestStatusChecks'
const longOutcome = 'ChangesRequestedAfterAllRequiredStatusChecksCompletedSuccessfully'
const longSummary = [
  'PR #128: validate project review with a large status set',
  'ci/linux-x86_64, ci/linux-aarch64, clippy, docs, integration, container build, security audit'
].join(' - ')

function reviewRunsFixture() {
  const runs = Array.from({ length: 12 }, (_, index) => `
    <article class="review-run-card">
      <div class="review-run-summary">
        <button type="button" class="review-run-main-action">
          <span class="review-run-pr">PR #${128 + index}</span>
          <span class="review-run-main">
            <strong>${longSummary}</strong>
            <small>5/18/2026, 10:${String(index).padStart(2, '0')}:00 AM · ${index + 1}m</small>
          </span>
        </button>
        <div class="review-run-chip-row">
          <span class="review-run-status active">${longStatus}-${index}</span>
          <span class="review-run-outcome request-changes">${longOutcome}-${index}</span>
        </div>
        <div class="review-run-token-row">
          <span><strong>${120_000 + index}</strong><small>Tokens</small></span>
          <span><strong>${80_000 + index}</strong><small>Cache hit</small></span>
        </div>
        <a class="review-run-pr-link" href="https://github.com/owner/repo/pull/${128 + index}">Open PR</a>
      </div>
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
          <section class="project-panel review-status-panel">
            <header class="settings-section-header">
              <div>
                <h2>Review Status</h2>
                <p>owner/repo</p>
              </div>
              <span class="section-status green">Ready</span>
            </header>
            <div class="settings-summary review-status-summary">
              <div class="settings-summary-item">
                <span>Auto Review</span>
                <strong>Enabled</strong>
                <small>Running</small>
              </div>
              <div class="settings-summary-item">
                <span>Outcome</span>
                <strong>Review Submitted</strong>
                <small>Finished now</small>
              </div>
              <div class="settings-summary-item">
                <span>Next Run</span>
                <strong>As soon as possible</strong>
                <small>Reviewer active</small>
              </div>
            </div>
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
      const panel = document.querySelector('.review-status-panel')
      const summary = document.querySelector('.review-status-summary')
      const runs = document.querySelector('.review-runs')
      const list = document.querySelector('.review-run-list')
      const prLink = first.querySelector('.review-run-pr-link')
      const tokenRow = first.querySelector('.review-run-token-row')
      const main = first.querySelector('.review-run-main')

      return {
        documentClientWidth: document.documentElement.clientWidth,
        documentScrollWidth: document.documentElement.scrollWidth,
        firstMainWidth: main.getBoundingClientRect().width,
        firstSummaryClientWidth: first.clientWidth,
        firstSummaryScrollWidth: first.scrollWidth,
        listClientHeight: list.clientHeight,
        listScrollHeight: list.scrollHeight,
        panelBottom: panel.getBoundingClientRect().bottom,
        prLinkText: prLink?.textContent?.trim(),
        runsOverflowY: getComputedStyle(runs).overflowY,
        summaryBottom: summary.getBoundingClientRect().bottom,
        tokenRowText: tokenRow?.textContent || '',
        viewportHeight: window.innerHeight,
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
    assert.ok(
      metrics.summaryBottom < metrics.viewportHeight,
      `review status summary should stay visible above long recent runs at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.ok(
      metrics.panelBottom <= metrics.viewportHeight,
      `review panel should not push content below the viewport at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.equal(metrics.runsOverflowY, 'auto')
    assert.ok(
      metrics.listScrollHeight > metrics.listClientHeight,
      `recent runs should scroll internally at ${width}px: ${JSON.stringify(metrics)}`
    )
    assert.equal(metrics.prLinkText, 'Open PR')
    assert.ok(
      metrics.tokenRowText.includes('Tokens') && metrics.tokenRowText.includes('Cache hit'),
      `review run card should expose token usage at ${width}px: ${JSON.stringify(metrics)}`
    )

    await page.close()
  }
} finally {
  await browser.close()
}
