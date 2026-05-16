# Project Review Loop

This document records the runtime contract for automatic project pull request reviews.

## Responsibilities

Each ready project with auto review enabled owns one cancellable review task context. The
context starts a PR pool worker and, depending on the Git provider, may also start a selector
task. Disable, cancel, and delete operations cancel the context, abort child tasks, clear the
PR pool, and clean up any active reviewer agent.

The PR pool is the only handoff between selectors, webhooks, tools, and the reviewer worker:

- Webhook and tool events enqueue concrete PR numbers into the pool.
- Selectors enqueue eligible PR numbers into the pool.
- The pool worker only consumes the pool, reviews one PR at a time, then waits for the next
  pool signal.

## Selector Contract

Selectors are deterministic Rust code. They do not create agents, submit GitHub reviews, or
call model APIs. A selector only reads GitHub, evaluates eligibility, and enqueues selected
PRs with reason `selector`.

Selector scans list open PRs using:

```text
state=open&sort=created&direction=asc&per_page=20&page=N
```

Each page is processed by ascending PR number. The selector continues until GitHub returns an
empty page or a short page. It enqueues only after the full scan succeeds, so a failed scan does
not leave a partial batch in the pool.

Eligibility rules live in `selection.rs`: skip drafts, skip only running or queued CI states,
and suppress re-review when the current reviewer has already reviewed the current head. PRs
authored by the reviewer are eligible under the same rules as all other PRs.

## Provider Cadence

GitHub App relay projects (`GithubAppRelay`) run one startup selector. It starts when the server
starts or review is enabled. If the scan errors, it retries every 10 seconds until a scan
completes. After a successful scan, including a scan with no eligible PRs, the selector exits.

GitHub token projects (`Github`) run a recurring selector. Failed scans retry every 10 seconds.
Successful scans wait 30 minutes before the next scan.

Selector status updates are best-effort UI signals. They must not overwrite an active review:
when a project is syncing or running a reviewer, selector state changes are skipped so the web
continues to show the active review.

## Pool Worker Contract

The pool worker starts by ensuring the review workspace is available. It then loops:

1. Claim the smallest PR number from the pool.
2. If the pool is empty, asynchronously wait for `review_notify` or cancellation.
3. Run exactly one review cycle for the claimed PR.
4. Requeue the claimed PR only for retryable review errors.
5. Return to the pool.

The worker never triggers selector scans. This keeps selector scheduling independent from review
execution and prevents long selector scans from making the web look like a reviewer is stuck.
