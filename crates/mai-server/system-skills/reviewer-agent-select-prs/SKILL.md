---
name: reviewer-agent-select-prs
description: Project PR selector skill for finding eligible open GitHub pull requests and queuing them into Mai's review pool without submitting reviews or modifying repositories.
metadata:
  short-description: Select eligible PRs into review pool
---

# Reviewer Agent - Select PRs

Find eligible open pull requests for the current Mai project and queue them with `queue_project_review_prs`. Do not submit reviews, comment on GitHub, change files, or start other agents.

## Use Bundled Helper

Use the helper from the review skill:

```bash
python3 /workspace/skills/reviewer-agent-review-pr/scripts/review_pr_helper.py test
python3 /workspace/skills/reviewer-agent-review-pr/scripts/review_pr_helper.py select-prs --prs prs.json --login "$LOGIN" --details details.json --reviews reviews.json --checks checks.json
```

If that path is unavailable, read `skill:///reviewer-agent-review-pr/scripts/review_pr_helper.py`, copy it to `/tmp/review_pr_helper.py`, and run the same commands through `/tmp/review_pr_helper.py`.

Save raw GitHub MCP JSON responses exactly as returned and pass those files to the helper. Do not hand-normalize `requested_reviewers`, review `commit_id`, GraphQL `nodes`/`edges`, or MCP `content`/`contents` wrappers.

## Workflow

### 1. Identify Account

Use a visible GitHub MCP account tool, usually `mcp__github__get_me`, to get the authenticated login. Identify `owner` and `repo` from the initial message.

If GitHub MCP tools are unavailable, return only:

```json
{"outcome":"failed","prs":[],"summary":"PR selection could not be completed.","error":"GitHub MCP tools are unavailable."}
```

### 2. Gather Candidate Data

List open PRs sorted by `updated_at` descending. In the current GitHub MCP surface, this normally means:

- `list_pull_requests(owner=..., repo=..., state="open", sort="updated", direction="desc")`
- `pull_request_read(..., method="get")`
- `pull_request_read(..., method="get_reviews")`
- `pull_request_read(..., method="get_check_runs")`

Fetch details, reviews, and check runs for recent open PRs that may be eligible. You do not need to exhaustively inspect every open PR in a busy repository when recent candidates are enough.

### 3. Select Eligible PRs

Run `select-prs` on the raw JSON files. The helper applies these fixed rules:

- Skip self-authored PRs.
- Skip draft PRs.
- Accept PRs with completed passing CI.
- If CI is pending or failed, accept only when the authenticated user is requested for review.
- Re-review only if the latest commit is newer than this user's latest submitted review, or the current head SHA differs from the latest submitted review `commit_id`.

### 4. Queue PRs

For every selected PR, call:

```json
{"prs":[{"number":123,"head_sha":"abc123","reason":"selector"}]}
```

Use one `queue_project_review_prs` call for all selected PRs when possible. Include `head_sha` when available. If there are no selected PRs, do not call the queue tool.

### 5. Final Response

Return only one JSON object, with no Markdown, prose, or code fence.

Queued:

```json
{"outcome":"queued","prs":[123,124],"summary":"Queued 2 eligible pull requests for review.","error":null}
```

No eligible PR:

```json
{"outcome":"no_eligible_pr","prs":[],"summary":"No eligible pull request found.","error":null}
```

Failed:

```json
{"outcome":"failed","prs":[],"summary":"PR selection could not be completed.","error":"GitHub MCP tools are unavailable."}
```

## Constraints

- Only select and queue PRs; never submit reviews.
- Do not modify the repository.
- Do not expose or infer a project id; the queue tool derives it from this agent.
- Use helper rules for eligibility.
