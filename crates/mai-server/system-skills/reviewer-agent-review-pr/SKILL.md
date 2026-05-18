---
name: reviewer-agent-review-pr
description: Reviewer agent skill for performing a script-assisted, deep, single GitHub pull request review. The reviewer agent uses bundled helper scripts for deterministic PR eligibility, reviewer clone checkout, changed-file/crate detection, Rust validation command planning, and final scheduler JSON, then performs human-quality code review and submits a GitHub review with inline comments. Trigger when the reviewer agent is invoked to review PRs, or when tasks are assigned for PR code review.
metadata:
  short-description: Script-assisted reviewer agent single-PR review
---

# Reviewer Agent - Review PR

Review exactly one eligible GitHub pull request for the current project. Use Mai's visible `github_api_get` and `github_api_request` tools for GitHub reads/writes, local shell commands for git/test work, and the bundled helper for fixed rules.

Mai refreshes the reviewer-owned clone at `/workspace/repo` before this skill starts. PR refs are available in the local clone, commonly as `refs/remotes/origin/pr/<number>` or `refs/pull/<number>/head`. Do not fetch credentials, read `GITHUB_TOKEN`, write credential files, or add model footers. Mai appends the model footer to submitted project reviews.

## Use Bundled Scripts First

Use `scripts/review_pr_helper.py` for deterministic steps before doing review judgment. The script has no third-party dependencies and does not access the network.

Important: save the raw JSON responses from `github_api_get` or `github_api_request` exactly as returned and feed those files directly to the helper. Do not hand-normalize fields such as `requested_reviewers`, review `commit_id`, GraphQL `nodes`/`edges`, or response wrappers before invoking the helper.

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
python3 scripts/review_pr_helper.py select-pr --prs prs.json --login "$LOGIN" --details details.json --reviews reviews.json --checks checks.json --target-pr "$PR"
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

### 1. Identify Repository and Account

Use `github_api_get` with path `/user` to get the authenticated user login. Identify `owner` and `repo` from the project context or `/workspace/repo` remote.

If `github_api_get` or `github_api_request` is unavailable, return only:

```json
{"outcome":"failed","pr":null,"summary":"Review could not be completed.","error":"Mai GitHub API tools are unavailable."}
```

### 2. Confirm the Target PR

Mai's initial message must name a target pull request. Review only that PR. If no target pull request is present, return a failed JSON result instead of scanning for another PR.

Fetch the target PR with visible Mai GitHub API tools. Use `github_api_get` with these REST paths:

- `/repos/OWNER/REPO/pulls/PR`
- `/repos/OWNER/REPO/pulls/PR/reviews`
- `/repos/OWNER/REPO/commits/HEAD_SHA/check-runs`
- `/repos/OWNER/REPO/commits/HEAD_SHA/status`

Use the helper on the raw JSON files from those calls. The helper already understands the common GitHub response shapes that matter in practice, including:

- `requested_reviewers` as a list of login strings such as `["ZR233"]`
- response wrappers such as `content`, `contents`, or top-level `text`
- re-review detection from review `commit_id` versus current PR head SHA when PR details do not expose a latest commit timestamp

Save the GitHub JSON outputs to files and run `select-pr --target-pr <number>`; if that target is ineligible, finish with `no_eligible_pr`. The helper applies these fixed rules:

- Skip self-authored PRs.
- Skip draft PRs.
- Accept PRs with completed passing CI.
- If CI is pending or failed, accept only when the authenticated user is requested for review.
- Re-review only if the latest commit is newer than this user's latest submitted review, or the current head SHA differs from the latest submitted review `commit_id`.

Practical note: many PRs expose `mergeable_state: "unknown"` even when `get_check_runs` is available. Do not infer passing CI from `mergeable_state == "unknown"`; rely on actual check runs when possible and let the helper apply the fallback rules.

If `select-pr` returns `no_eligible_pr`, finish with:

```json
{"outcome":"no_eligible_pr","pr":null,"summary":"No eligible pull request found.","error":null}
```

### 3. Prepare the Reviewer Clone

Run `prepare-review` for the target PR. Work only inside `/workspace/repo`, which is this reviewer agent's isolated clone. Treat the returned `repo` value as `REVIEW_REPO`; in Mai it should be `/workspace/repo`.

Before submitting the review, confirm the PR head SHA still matches the checked-out clone SHA. If it changed, finish with `no_eligible_pr`; the scheduler will queue a fresh signal.

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

Submit through `github_api_request`.

Use a single REST request when possible:

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

Set `event` to `REQUEST_CHANGES`, `APPROVE`, or `COMMENT`. If submission fails because the account is the PR author or GitHub rejects the event, leave a normal PR comment with `github_api_request` to `POST /repos/OWNER/REPO/issues/PR/comments` when appropriate; otherwise report the failure.

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
{"outcome":"failed","pr":123,"summary":"Review could not be completed.","error":"GitHub rejected the review submission."}
```

## Constraints

- Review exactly one PR per invocation.
- Always work in the reviewer-owned clone at `/workspace/repo`.
- Use helper scripts for fixed rules; use reviewer judgment for code review.
- Use only visible Mai GitHub API tools for GitHub reads/writes.
- Leave cleanup to Mai; reviewer agent deletion removes the clone.
