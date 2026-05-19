---
name: reviewer-agent-review-pr
description: Reviewer agent skill for performing a script-assisted, deep, single GitHub pull request review. The reviewer agent reviews the target PR already selected by Mai, uses bundled helper scripts for reviewer clone checkout, changed-file/crate detection, Rust validation command planning, and final scheduler JSON, then performs human-quality code review and submits a GitHub review with inline comments. Trigger when the reviewer agent is invoked to review PRs, or when tasks are assigned for PR code review.
metadata:
  short-description: Script-assisted reviewer agent single-PR review
---

# Reviewer Agent - Review PR

Review exactly one target GitHub pull request for the current project. Mai's system selector is responsible for choosing the PR before this skill starts. Use Mai's visible `github_api_get` and `github_api_request` tools for GitHub reads/writes, local shell commands for git/test work, and the bundled helper for deterministic local preparation.

Mai refreshes the reviewer-owned clone at `/workspace/repo` before this skill starts. PR refs are available in the local clone, commonly as `refs/remotes/origin/pr/<number>` or `refs/pull/<number>/head`. Do not fetch credentials, read `GITHUB_TOKEN`, write credential files, or add model footers. Mai appends the model footer to submitted project reviews.

## Use Bundled Scripts First

Use `scripts/review_pr_helper.py` for deterministic local steps before doing review judgment. The script has no third-party dependencies and does not access the network.

Important: save changed-file JSON responses from `github_api_get` or `github_api_request` exactly as returned and feed those files directly to the helper. Do not hand-normalize file lists, GraphQL `nodes`/`edges`, or response wrappers before invoking the helper.

Preferred invocation:

```bash
python3 scripts/review_pr_helper.py test
```

If `scripts/review_pr_helper.py` is not directly readable from the container, read the skill resource `skill:///reviewer-agent-review-pr/scripts/review_pr_helper.py`, copy its text to `/tmp/review_pr_helper.py`, and run:

```bash
python3 /tmp/review_pr_helper.py test
```

The helper commands are:

```bash
python3 scripts/review_pr_helper.py prepare-review --repo /workspace/repo --agent-id "$REVIEWER_AGENT_ID" --pr "$PR"
python3 scripts/review_pr_helper.py changed-files --repo "$REVIEW_REPO" --files files.json
python3 scripts/review_pr_helper.py rust-plan --repo "$REVIEW_REPO" --changed changed.json
python3 scripts/review_pr_helper.py final-json --outcome review_submitted --pr "$PR" --summary "Submitted APPROVE for owner/repo#$PR after validation passed."
```

Treat helper output as structured facts and command suggestions. You still own code understanding, finding severity, inline comment wording, and the final GitHub review decision.

When dogfooding this skill outside Mai, `/workspace/repo` may not exist. Use the local clone as `--repo` and make sure PR refs exist first:

```bash
git fetch origin '+refs/pull/*/head:refs/remotes/origin/pr/*'
python3 scripts/review_pr_helper.py prepare-review --repo /path/to/repo --agent-id "$REVIEWER_AGENT_ID" --pr "$PR"
```

## Workflow

### 1. Identify Repository

Identify `owner` and `repo` from the project context or `/workspace/repo` remote. Do not look up the authenticated user login before review submission; rely on GitHub's review submission result and use the fallback path if GitHub rejects the review.

If `github_api_get` or `github_api_request` is unavailable, return only:

```json
{"outcome":"failed","pr":null,"summary":"Review could not be completed.","error":"Mai GitHub API tools are unavailable."}
```

### 2. Confirm the Target PR

Mai's initial message must name a target pull request. Review only that PR. If no target pull request is present, return a failed JSON result instead of scanning for another PR.

Fetch the target PR with visible Mai GitHub API tools. Use `github_api_get` with these REST paths as needed for review context:

- `/repos/OWNER/REPO/pulls/PR`
- `/repos/OWNER/REPO/pulls/PR/reviews`
- `/repos/OWNER/REPO/pulls/PR/comments`
- `/repos/OWNER/REPO/issues/PR/comments`
- `/repos/OWNER/REPO/commits/HEAD_SHA/check-runs`
- `/repos/OWNER/REPO/commits/HEAD_SHA/status`

Do not scan for another PR, do not replace the target PR, and do not skip the target PR because of draft state, author identity, existing reviews, or CI status. Treat those facts as review context only.

Practical note: many PRs expose `mergeable_state: "unknown"` even when `get_check_runs` is available. Do not infer passing CI from `mergeable_state == "unknown"`; rely on actual check runs when describing validation context.

Read previous review comments and PR comments before making a decision. Check whether each earlier actionable comment is technically reasonable, whether the current PR revision resolves it, and whether any unresolved reasonable comment should remain blocking or be mentioned in the review body.

Check CI status and check runs before submitting. If CI is failing, inspect the failing checks enough to decide whether the failure is caused by this PR's changes. Do not approve when a current CI failure is caused by the PR; request changes or comment with the failure context instead.

### 3. Prepare the Reviewer Clone

Run `prepare-review` for the target PR. Work only inside `/workspace/repo`, which is this reviewer agent's isolated clone. Treat the returned `repo` value as `REVIEW_REPO`; in Mai it should be `/workspace/repo`.

Before submitting the review, confirm the PR head SHA still matches the checked-out clone SHA. If it changed, return a failed final JSON result; the scheduler will queue a fresh signal.

### 4. Inspect and Validate

Use visible Mai GitHub API tools to inspect PR metadata, changed files, diff, existing comments, review threads, and checks context. Save changed files JSON and run:

```bash
python3 scripts/review_pr_helper.py changed-files --repo "$REVIEW_REPO" --files files.json > changed.json
python3 scripts/review_pr_helper.py rust-plan --repo "$REVIEW_REPO" --changed changed.json > rust-plan.json
```

Run the commands in `rust-plan.json` when present. For Rust PRs, always run `cargo fmt --check` and clippy commands suggested by the helper; run tests for changed crates unless the repository clearly cannot support them in the current environment.

`rust-plan` is intentionally conservative. If it still falls back to broad workspace commands for a large repository, prefer the repository's established CI entry point for the changed area and record any broad-command failures separately from PR-local validation. For example, StarryOS syscall test changes in `tgoskits` should be validated with the StarryOS QEMU test command used by that repository, not only with workspace-wide `cargo test`.

When the repository is large, fetch only the target PR details needed for validation and review context.

Record exact validation failures. Treat these as blocking:

- `cargo fmt --check` fails.
- `cargo clippy ... -D warnings` fails.
- `cargo test` fails.
- Security, data-loss, resource leak, panic, race/deadlock, or behavior-regression findings.
- Unsafe code without an adequate SAFETY justification.
- Public API or architecture changes that contradict established project patterns.

### 5. Review Standards

Prioritize bugs, regressions, security risks, data-loss risks, missing tests, broken edge cases, and behavior mismatches. Do not request changes for style-only preferences.

Compare changed code with 2-3 similar existing files in the clone. For Rust, check module boundaries, trait usage, error handling style, serde compatibility, dependency direction, `unwrap()`/`expect()` in production paths, lock ordering, and RAII cleanup.

Analyze the impact on existing behavior, not only the changed lines. Identify which existing features, APIs, workflows, data formats, persistence paths, background jobs, and tests could be affected. Decide whether the PR is truly isolated or whether it changes shared contracts or cross-cutting behavior.

Assess concurrency and liveness risks. For code involving async work, locks, channels, callbacks, transactions, or shared mutable state, analyze whether the PR can introduce deadlocks, lock-order inversions, missed wakeups, starvation, blocking-in-async, or resource leaks.

Assess design quality and maintainability. Check whether responsibilities are placed in the right module, whether abstractions match existing patterns, and whether the PR introduces overly large functions, overly large structs, catch-all facades, duplicated logic, or unclear ownership boundaries. Treat design issues as blocking when they create real correctness, maintenance, or architecture risk.

Search similar PRs before submission:

```text
repo:OWNER/REPO type:pr <3-5 title keywords>
repo:OWNER/REPO type:pr <changed/path.rs>
```

Mention overlapping or duplicate PRs in the review body when relevant.

### 6. Prepare Findings and Inline Comments

Use `REQUEST_CHANGES` when any blocking finding exists, `APPROVE` when safe to merge, and `COMMENT` only for advisory-only reviews.

For each line-specific finding, prepare an inline comment on the changed line with `side: "RIGHT"`. Do not submit inline comments individually. Collect every inline comment into the final review request's `comments` array. Each comment should state the problem, why it matters, and a concrete fix or alternative. Put non-line-specific findings in the review body.

Keep the review body concise. Include validation results, similar-PR notes, and any non-inline findings.

The review body must explicitly cover:

- What the PR changes and which problems it solves.
- Which existing features or contracts may be affected, including whether the change appears isolated.
- CI status, local validation results, and whether any failing CI appears caused by this PR.
- Previous review comments considered, whether they are reasonable, and whether they are resolved.
- Remaining unresolved issues, risks, or test gaps. If none remain, say so clearly.

### 7. Submit the GitHub Review

Submit through `github_api_request`. Use exactly one final review request. Do not create a pending review, do not create an empty review first, do not submit `/pulls/PR/reviews/REVIEW_ID/events`, and never call `POST /repos/OWNER/REPO/pulls/PR/comments` for reviewer inline comments. GitHub's direct review-comment endpoint has a different schema and is not used by the Mai reviewer flow.

Use this single REST request shape:

```json
{
  "method": "POST",
  "path": "/repos/OWNER/REPO/pulls/PR/reviews",
  "body": {
    "event": "REQUEST_CHANGES",
    "body": "Review body with validation results.",
    "comments": [
      {
        "path": "src/lib.rs",
        "line": 123,
        "side": "RIGHT",
        "body": "Inline finding."
      }
    ]
  }
}
```

Set `event` to `REQUEST_CHANGES`, `APPROVE`, or `COMMENT`. If GitHub rejects the review submission for any reason where a normal PR comment is still appropriate, leave that comment with `github_api_request` to `POST /repos/OWNER/REPO/issues/PR/comments`; otherwise report the failure.

### 8. Final Response

The final response is consumed by the Mai project review scheduler. Return only one JSON object, with no Markdown, prose, or code fence. You may use `final-json` to generate it.

Submitted:

```json
{"outcome":"review_submitted","pr":123,"summary":"Submitted APPROVE for owner/repo#123 after cargo fmt --check and cargo test passed.","error":null}
```

Failed:

```json
{"outcome":"failed","pr":123,"summary":"Review could not be completed.","error":"GitHub rejected the review submission."}
```

## Constraints

- Review exactly one PR per invocation.
- Always work in the reviewer-owned clone at `/workspace/repo`.
- Use helper scripts for local preparation and final scheduler JSON; use reviewer judgment for code review.
- Use only visible Mai GitHub API tools for GitHub reads/writes.
- Leave cleanup to Mai; reviewer agent deletion removes the clone.
