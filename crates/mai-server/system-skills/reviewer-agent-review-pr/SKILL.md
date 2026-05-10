---
name: reviewer-agent-review-pr
description: Reviewer agent skill for performing a deep, single GitHub pull request review. The reviewer agent selects the most eligible unreviewed PR (CI completed or review requested), checks out code locally via git worktree, runs local tests for Rust/cargo projects, checks design and architecture conformance, verifies code style against repository conventions, searches for similar PRs, and submits a GitHub review with inline comments. Trigger when the reviewer agent is invoked to review PRs, or when tasks are assigned for PR code review.
metadata:
  short-description: Reviewer agent deep single-PR review with worktree and MCP tools
---

# Reviewer Agent — Review PR

This skill is designed for a **reviewer agent** in the multi-agent team workflow. It performs a deep, focused review of a single GitHub pull request on behalf of the reviewer agent role.

The skill selects the most eligible unreviewed PR, checks out the code in an isolated git worktree, runs targeted local validation, analyzes design and architecture, searches for duplicate PRs, and submits a complete GitHub review with inline comments.

As a reviewer agent, your role is to provide objective, thorough code review. GitHub interactions must use the GitHub MCP tools that are actually visible in the current tool list; git worktree and local test execution use Bash commands.

The repository is pre-cloned and refreshed by Mai at `/workspace/repo` before this skill starts. Do not fetch credentials or configure tokens yourself. Use local refs under `/workspace/repo`, including `refs/remotes/origin/pr/<number>`, when creating review worktrees.

## Workflow

### 1. Identify Repository and Account

Use the visible GitHub MCP account tool, usually `mcp__github__get_me`, to get the authenticated user login. Identify `owner` and `repo` from the project context or the git remote of the current working directory.

If the GitHub MCP tools are unavailable, report that this skill requires a configured GitHub MCP server and exit.

### 2. Select an Eligible PR

List all open PRs with a visible GitHub MCP pull request listing tool, sorted by `updated_at` descending.

Apply eligibility filters in order:

- **Skip self-authored PRs**: exclude any PR where `author.login` equals the authenticated user.
- **Skip drafts**: exclude PRs where `isDraft` is true.
- **CI gate**: check CI status for each remaining PR. A PR is eligible if all CI checks have completed and passed. If CI is still running or has failures, the PR is only eligible when `reviewDecision` is `REVIEW_REQUIRED` and the authenticated user is in the requested reviewers list. Use a visible GitHub MCP pull request read tool when available, following its schema exactly, or use `github_api_get` with path `/repos/OWNER/REPO/pulls/NUMBER` and check the status/check-runs fields.
- **Freshness**: for each remaining PR, fetch the current user's reviews through a visible GitHub MCP review/read tool when available, and fetch the latest commit timestamp from PR details. A PR is eligible if the user has never reviewed it, OR the PR's latest commit time is strictly newer than the user's last submitted review timestamp. Compare commit dates, not `updatedAt` — comments and CI runs can update the PR without new code.
- **Review request priority**: among eligible PRs, prefer the one where the authenticated user is explicitly requested as a reviewer.

Pick the single most recently updated eligible PR. If no PRs match, finish with only:

```json
{"outcome":"no_eligible_pr","pr":null,"summary":"No eligible pull request found.","error":null}
```

### 3. Follow Review Standards

Adopt this review methodology:

- Inspect PR metadata, changed files, diff, existing comments, review threads, and checks context.
- Focus on: bugs, regressions, security risks, data-loss risks, missing tests, broken edge cases, and behavior mismatches.
- Do not request changes for style-only preferences.
- Keep review bodies concise with concrete file and function references when possible.
- Use `REQUEST_CHANGES` for blocking findings, `APPROVE` when safe to merge, `COMMENT` only for purely advisory notes.
- If submission fails because the account is the PR author or GitHub rejects the event, leave a normal PR comment only when a visible comment tool is available; otherwise report the failure.

### 4. Create Git Worktree for Isolated Review

The scheduler fetches PR refs before this skill starts. Verify the local PR ref exists and create an isolated worktree under `/workspace/reviews/<reviewer-agent-id>/`:

```bash
cd /workspace/repo
git rev-parse refs/remotes/origin/pr/<pr>
mkdir -p /workspace/reviews/<reviewer-agent-id>
WORKTREE=$(mktemp -d /workspace/reviews/<reviewer-agent-id>/review-pr-<pr>-XXXXXX)
git worktree add --detach "$WORKTREE" origin/pr/<pr>
```

If a worktree for this PR already exists at a known path, verify it is clean and at the expected PR head before reusing:

```bash
git -C "$WORKTREE" status --short
git -C "$WORKTREE" rev-parse HEAD
git rev-parse refs/remotes/origin/pr/<pr>
```

If the existing worktree is stale and clean, update it with a detached checkout to the fetched PR head. If it has local changes, create a fresh worktree at a new path instead of overwriting.

Work exclusively inside this worktree for code inspection and test execution. When the review is complete, clean up:

```bash
git worktree remove "$WORKTREE"
git -C /workspace/repo worktree prune
```

### 5. Code and Design Review

Inspect the PR thoroughly within the worktree:

**Obtain the diff and changed files** via visible GitHub MCP pull request read/file tools when available, following their current schemas exactly. Also inspect the PR body.

**Architecture and design review:**
- Does the change introduce new abstractions, modules, or layers? Assess whether they fit the existing project architecture and dependency graph.
- Check for adherence to the repository's established patterns. Read 2-3 existing files in the repo that are similar to the changed code for comparison.
- For Rust projects: assess module structure, trait usage, error handling patterns (`thiserror` vs `anyhow`), and whether the change respects ownership and borrowing conventions.
- If the PR adds or modifies a public API, check for backwards compatibility and serde attribute correctness.
- Verify that the change does not introduce circular dependencies between crates.

**Code style conformance:**
- Compare naming conventions (functions, types, variables, modules) against 2-3 similar existing files in the repository.
- Check for idiomatic patterns: appropriate use of `Result`/`Option`, `use` statement grouping, derive macro usage.
- Verify the change follows the same comment style, whitespace patterns, and organizational conventions as the surrounding code.

**Security and correctness:**
- Flag `unwrap()` and `expect()` in non-test, non-benchmark code paths.
- Inspect any `unsafe` blocks for missing SAFETY comments and soundness justification.
- Verify input validation and error handling for new or modified public APIs.
- Check for potential deadlocks (lock ordering), race conditions, or resource leaks.
- For code that allocates or manages resources, verify RAII compliance and proper cleanup.

### 6. Local Test Execution

If the PR description mentions testing, the changed files include test code, or CI checks show test runs, execute tests locally in the worktree:

**For Rust workspace projects:**

Identify changed crates from the changed files:
```bash
cd "$WORKTREE"
# Extract crate directories from changed file paths
CHANGED_CRATES=$(echo "<changed files from PR>" | grep -oE '^crates/[^/]+' | sort -u)
```

Run formatting check:
```bash
cargo fmt --check
```

Run clippy on changed crates:
```bash
for crate in $CHANGED_CRATES; do
  if [ -f "$crate/Cargo.toml" ]; then
    cargo clippy --manifest-path "$crate/Cargo.toml" --all-features -- -D warnings
  fi
done
```

Run tests on changed crates:
```bash
for crate in $CHANGED_CRATES; do
  if [ -f "$crate/Cargo.toml" ]; then
    cargo test --manifest-path "$crate/Cargo.toml" --all-features
  fi
done
```

**Failure classification:**

| Failure | Severity |
|---------|----------|
| `cargo fmt --check` fails | Blocking — code is not formatted per project standard |
| `cargo clippy` with `-D warnings` fails | Blocking — lint violations that the project treats as errors |
| `cargo test` fails | Blocking — broken functionality |
| Bug fix without a reproduction test | Concern — note in review but not necessarily blocking if fix is clearly correct |

Record exact command output for each failure — include it in the inline comment or review body so the author can reproduce.

If the project has no `Cargo.toml` (not a Rust project), skip Rust-specific checks. Adapt the validation to match the project's build system (e.g., `npm test`, `go test ./...`, `pytest`).

### 7. Similar PR Check

Search for PRs that overlap with or may duplicate the current PR:

Use a visible GitHub MCP search tool to search issues and pull requests with keywords extracted from the PR title (3-5 significant words), restricted to the current repository:

```
repo:OWNER/REPO type:pr <key terms>
```

Also search for PRs touching the same files using a visible GitHub MCP search tool:
```
repo:OWNER/REPO type:pr <path:key/file.rs>
```

Review findings:
- If there are open PRs touching the same files, note potential merge conflicts.
- If there are merged PRs with very similar changes, check whether the current PR is a re-submission or duplicate.
- If there is a clearly duplicate PR attempting the same change, flag it prominently.
- Include similar PR references in the review body. This is informational, not blocking unless there is an actual conflict or duplication.

### 8. Prepare Findings and Inline Comments

For each finding, classify as blocking or non-blocking:

**Blocking findings** (require `REQUEST_CHANGES`):
- Test failures (local or CI)
- `cargo fmt --check` failure
- `cargo clippy` violations with `-D warnings`
- Security vulnerabilities (unsafe patterns, missing validation)
- Behavior that contradicts documented requirements or established project conventions
- Missing error handling that could cause panics in production code
- `unsafe` blocks without SAFETY comments or soundness justification
- Architectural decisions that conflict with established project patterns
- Resource leaks or incorrect RAII implementation

**Non-blocking findings** (can accompany `APPROVE`):
- Minor style variations that do not contradict project conventions
- Suggestions for documentation improvements
- Questions about design choices that appear reasonable
- Minor optimization opportunities
- Test coverage suggestions for existing but untested code

For each finding that ties to a specific line, prepare an inline comment anchored to the changed line, using `side=RIGHT` to comment on the new code. Each comment should include:

1. The concrete problem or observation
2. Why it matters (correctness, convention, security, performance)
3. A suggested fix or alternative

If a finding cannot be attached to a specific diff line, include it in the review body instead.

### 9. Submit the GitHub Review

Before submission, confirm the PR head SHA has not changed. Fetch the current state through a visible GitHub MCP PR read tool and compare `head.sha` with the commit checked out in the worktree.

Submit the review through a visible GitHub MCP review-writing tool when one is available. Follow that tool's current schema exactly; do not assume parameter names beyond the schema shown to you. Use:
- `event`: `REQUEST_CHANGES` if any blocking finding exists, `APPROVE` if none, `COMMENT` only for purely advisory notes.
- `body`: concise summary including decision, local validation results, similar PR notes, and any findings that could not be attached to specific lines.
- `comments`: array of inline comments, each with `path`, `line` (use the right-side line number), `side: "RIGHT"`, and `body`.

If no visible MCP tool can submit a pull request review, report that review submission is unavailable in the final response.

Do not look for `GITHUB_TOKEN`, run ad hoc GitHub REST scripts, or write credential files inside the agent container.

If submission fails because the head SHA changed, re-fetch the PR state and restart from the eligibility check.

### 10. Final Response

The final response is consumed by the Mai project review scheduler. Return **only** one JSON object, with no Markdown, prose, or code fence.

If a review was submitted:

```json
{"outcome":"review_submitted","pr":123,"summary":"Submitted APPROVE for owner/repo#123 after cargo fmt --check and cargo test passed.","error":null}
```

If no PR was eligible:

```json
{"outcome":"no_eligible_pr","pr":null,"summary":"No eligible pull request found.","error":null}
```

If the review could not be completed:

```json
{"outcome":"failed","pr":123,"summary":"Review could not be completed.","error":"GitHub MCP did not expose a review-writing tool."}
```

## Key Constraints

- Review exactly one PR per invocation. Do not batch.
- Always use an isolated git worktree under `/workspace/reviews/`; never review in `/workspace/repo`.
- Run `cargo fmt --check` and `cargo clippy` for Rust PRs regardless of CI status — CI may run different targets.
- Search for similar PRs before submitting the review.
- Clean up the temporary worktree after review completion.
- If GitHub MCP tools are not available, report the missing dependency and exit before making any changes.
