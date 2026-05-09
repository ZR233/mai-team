---
name: github-pr-review
description: Review every open GitHub pull request in the current Mai project that the authenticated account has not reviewed yet, newest first, and submit GitHub reviews.
metadata:
  short-description: Review unreviewed project PRs
---

# GitHub PR Review

Use this skill only inside a Mai Project maintainer or project child agent. The project repository is at `/workspace/repo`, and GitHub access must go through the project GitHub MCP tools.

## Workflow

1. Identify repository and account.
   - Use `mcp__github__get_me` to get the authenticated user login.
   - Use the project system prompt or `/workspace/repo` remote metadata to identify `owner` and `repo`.
   - If the project GitHub MCP tools are unavailable, explain that this skill requires a GitHub-backed Mai Project.

2. Build the PR queue.
   - List all open PRs with `mcp__github__list_pull_requests`.
   - Sort PRs by `updated_at` descending.
   - For each PR, read review data and skip it if any submitted review author login matches the authenticated user.
   - Prefer current GitHub MCP tools such as `mcp__github__pull_request_read` with methods like `get`, `get_files`, `get_reviews`, and `get_review_comments` when available.
   - If only legacy tools are exposed, use `mcp__github__get_pull_request` for details and the closest available review/file/comment tools.
   - If MCP does not expose reviews/files/comments, use the read-only `github_api_get` fallback with paths such as `/repos/OWNER/REPO/pulls/NUMBER/reviews`, `/repos/OWNER/REPO/pulls/NUMBER/files`, and `/repos/OWNER/REPO/pulls/NUMBER/comments`.

3. Create and maintain a todo list.
   - Call `update_todo_list` once after filtering, with one item per PR: `Review PR #<number>: <title>`.
   - Keep at most one item `in_progress`.
   - Mark each PR `completed` immediately after its GitHub review has been submitted.
   - If no PRs match, set a short completed todo stating that no open PRs need review.

4. Review each PR.
   - Inspect PR metadata, changed files, diff, existing comments, checks context when available, and the relevant local code in `/workspace/repo`.
   - Focus on bugs, regressions, security/data-loss risks, missing tests, broken edge cases, and behavior mismatches.
   - Do not request changes for style-only preferences.
   - Keep review bodies concise and include concrete file/function references when possible.

5. Submit the GitHub review.
   - Prefer `mcp__github__pull_request_review_write` when available.
   - Use `REQUEST_CHANGES` if there are blocking findings.
   - Use `APPROVE` if the PR is safe to merge and no blocking findings remain.
   - Use `COMMENT` only when the PR is not clearly approvable but has no blocking issue.
   - If the MCP server exposes `create_pull_request_review` instead, use it with the equivalent event.
   - If submission fails because the account is the PR author or GitHub rejects the event, leave a normal PR comment when a comment tool is available, otherwise report the failure in the final response.

## Final Response

Summarize reviewed PRs in newest-first order with:

- PR number and title
- Review event submitted
- Blocking findings, if any
- Any PR skipped and the reason
