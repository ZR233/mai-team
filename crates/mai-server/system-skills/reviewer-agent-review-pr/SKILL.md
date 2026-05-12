---
name: reviewer-agent-review-pr
description: Reviewer agent skill for performing a script-assisted, deep, single GitHub pull request review. The reviewer agent uses bundled helper scripts for deterministic PR eligibility, git worktree preparation, changed-file/crate detection, Rust validation command planning, and final scheduler JSON, then performs human-quality code review and submits a GitHub review with inline comments. Trigger when the reviewer agent is invoked to review PRs, or when tasks are assigned for PR code review.
metadata:
  short-description: Script-assisted reviewer agent single-PR review
---

# Reviewer Agent - Review PR

Review exactly one eligible GitHub pull request for the current project. Use GitHub MCP tools for GitHub reads/writes, local shell commands for git/test work, and the bundled helper for fixed rules.

Mai refreshes `/workspace/repo` before this skill starts and fetches PR refs as `refs/remotes/origin/pr/<number>`. Do not fetch credentials, read `GITHUB_TOKEN`, write credential files, or add model footers. Mai appends the model footer to submitted project reviews.

## Use Bundled Scripts First

Use `scripts/review_pr_helper.py` for deterministic steps before doing review judgment. The script has no third-party dependencies and does not access the network.

Important: save the raw GitHub MCP JSON responses exactly as returned and feed those files directly to the helper. Do not hand-normalize fields such as `requested_reviewers`, review `commit_id`, GraphQL `nodes`/`edges`, or MCP `content`/`contents` wrappers before invoking the helper.

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
python3 scripts/review_pr_helper.py select-pr --prs prs.json --login "$LOGIN" --details details.json --reviews reviews.json --checks checks.json
python3 scripts/review_pr_helper.py prepare-worktree --repo /workspace/repo --review-root /workspace/reviews --agent-id "$REVIEWER_AGENT_ID" --pr "$PR"
python3 scripts/review_pr_helper.py changed-files --repo "$WORKTREE" --files files.json
python3 scripts/review_pr_helper.py rust-plan --repo "$WORKTREE" --changed changed.json
python3 scripts/review_pr_helper.py final-json --outcome review_submitted --pr "$PR" --summary "Submitted APPROVE for owner/repo#$PR after validation passed."
```

Treat helper output as structured facts and command suggestions. You still own code understanding, finding severity, inline comment wording, and the final GitHub review decision.

## Workflow

### 1. Identify Repository and Account

Use the visible GitHub MCP account tool, usually `mcp__github__get_me`, to get the authenticated user login. Identify `owner` and `repo` from the project context or `/workspace/repo` remote.

If GitHub MCP tools are unavailable, return only:

```json
{"outcome":"failed","pr":null,"summary":"Review could not be completed.","error":"GitHub MCP tools are unavailable."}
```

### 2. Select One Eligible PR

List open PRs with a visible GitHub MCP pull request listing tool, sorted by `updated_at` descending. In the current GitHub MCP surface, this normally means:

- `list_pull_requests(owner=..., repo=..., state="open", sort="updated", direction="desc")`
- `pull_request_read(..., method="get")`
- `pull_request_read(..., method="get_reviews")`
- `pull_request_read(..., method="get_check_runs")`

Use the helper on the raw JSON files from those calls. The helper already understands the common GitHub MCP response shapes that matter in practice, including:

- `requested_reviewers` as a list of login strings such as `["ZR233"]`
- MCP text wrappers such as `content`, `contents`, or top-level `text`
- GraphQL-style `nodes` / `edges`
- re-review detection from review `commit_id` versus current PR head SHA when PR details do not expose a latest commit timestamp

Save the MCP JSON outputs to files and run `select-pr`. The helper applies these fixed rules:

- Skip self-authored PRs.
- Skip draft PRs.
- Accept PRs with completed passing CI.
- If CI is pending or failed, accept only when the authenticated user is requested for review.
- Re-review only if the latest commit is newer than this user's latest submitted review, or the current head SHA differs from the latest submitted review `commit_id`.
- Prefer explicitly requested reviews, then most recently updated PR.

Practical note: many PRs expose `mergeable_state: "unknown"` even when `get_check_runs` is available. Do not infer passing CI from `mergeable_state == "unknown"`; rely on actual check runs when possible and let the helper apply the fallback rules.

If `select-pr` returns `no_eligible_pr`, finish with:

```json
{"outcome":"no_eligible_pr","pr":null,"summary":"No eligible pull request found.","error":null}
```

### 3. Prepare an Isolated Worktree

Run `prepare-worktree` for the selected PR. Work only inside the returned worktree path. Do not review or run tests directly in `/workspace/repo`.

Before submitting the review, confirm the PR head SHA still matches the checked-out worktree SHA. If it changed, restart from PR selection.

Clean up at the end:

```bash
git worktree remove "$WORKTREE"
git -C /workspace/repo worktree prune
```

### 4. Inspect and Validate

Use visible GitHub MCP tools to inspect PR metadata, changed files, diff, existing comments, review threads, and checks context. Save changed files JSON and run:

```bash
python3 scripts/review_pr_helper.py changed-files --repo "$WORKTREE" --files files.json > changed.json
python3 scripts/review_pr_helper.py rust-plan --repo "$WORKTREE" --changed changed.json > rust-plan.json
```

Run the commands in `rust-plan.json` when present. For Rust PRs, always run `cargo fmt --check` and clippy commands suggested by the helper; run tests for changed crates unless the repository clearly cannot support them in the current environment.

When the repository is large and the GitHub MCP list response is sparse, prefer fetching details only for the PRs that are plausible candidates after `list_pull_requests` ordering and explicit review-request checks. The helper is designed to select from a small set of recent open PRs; you do not need to exhaustively fetch every open PR in a busy repository.

Record exact validation failures. Treat these as blocking:

- `cargo fmt --check` fails.
- `cargo clippy ... -D warnings` fails.
- `cargo test` fails.
- Security, data-loss, resource leak, panic, race/deadlock, or behavior-regression findings.
- Unsafe code without an adequate SAFETY justification.
- Public API or architecture changes that contradict established project patterns.

### 5. Review Standards

Prioritize bugs, regressions, security risks, data-loss risks, missing tests, broken edge cases, and behavior mismatches. Do not request changes for style-only preferences.

Compare changed code with 2-3 similar existing files in the worktree. For Rust, check module boundaries, trait usage, error handling style, serde compatibility, dependency direction, `unwrap()`/`expect()` in production paths, lock ordering, and RAII cleanup.

Search similar PRs before submission:

```text
repo:OWNER/REPO type:pr <3-5 title keywords>
repo:OWNER/REPO type:pr <changed/path.rs>
```

Mention overlapping or duplicate PRs in the review body when relevant.

### 6. Prepare Findings and Inline Comments

Use `REQUEST_CHANGES` when any blocking finding exists, `APPROVE` when safe to merge, and `COMMENT` only for advisory-only reviews.

For each line-specific finding, submit an inline comment on the changed line with `side: "RIGHT"`. Each comment should state the problem, why it matters, and a concrete fix or alternative. Put non-line-specific findings in the review body.

Keep the review body concise. Include validation results, similar-PR notes, and any non-inline findings.

### 7. Submit the GitHub Review

Submit through a visible GitHub MCP review-writing tool and follow its current schema exactly. Do not assume parameter names beyond the visible schema.

In the current GitHub MCP surface, the review submission path is usually `pull_request_review_write`, with inline comments created through `add_comment_to_pending_review` after creating a pending review and before `submit_pending`. If the visible tool schema differs, follow the visible schema instead of these names.

If no visible MCP tool can submit a pull request review, return a `failed` JSON result. If submission fails because the account is the PR author or GitHub rejects the event, leave a normal PR comment only when a visible comment tool is available; otherwise report the failure.

### 8. Final Response

The final response is consumed by the Mai project review scheduler. Return only one JSON object, with no Markdown, prose, or code fence. You may use `final-json` to generate it.

Submitted:

```json
{"outcome":"review_submitted","pr":123,"summary":"Submitted APPROVE for owner/repo#123 after cargo fmt --check and cargo test passed.","error":null}
```

No eligible PR:

```json
{"outcome":"no_eligible_pr","pr":null,"summary":"No eligible pull request found.","error":null}
```

Failed:

```json
{"outcome":"failed","pr":123,"summary":"Review could not be completed.","error":"GitHub MCP did not expose a review-writing tool."}
```

## Constraints

- Review exactly one PR per invocation.
- Always use an isolated worktree under `/workspace/reviews/`.
- Use helper scripts for fixed rules; use reviewer judgment for code review.
- Use only visible GitHub MCP tools for GitHub reads/writes.
- Clean up temporary worktrees before finishing.
