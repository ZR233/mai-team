#!/usr/bin/env python3
"""Deterministic helpers for Mai reviewer-agent-review-pr.

The helper intentionally avoids GitHub credentials and network access. Feed it
JSON captured from visible GitHub MCP tools and local git state; it returns
small JSON objects that the reviewer can rely on for repetitive rules.
"""

from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


PASSING_CONCLUSIONS = {"success", "neutral", "skipped"}
FAILING_CONCLUSIONS = {
    "action_required",
    "cancelled",
    "failure",
    "startup_failure",
    "timed_out",
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    select = subparsers.add_parser("select-pr", help="select one eligible PR")
    select.add_argument("--prs", default="-", help="PR list JSON path, or - for stdin")
    select.add_argument("--login", required=True, help="authenticated GitHub login")
    select.add_argument("--details", help="optional PR details JSON path")
    select.add_argument("--reviews", help="optional reviews JSON path")
    select.add_argument("--checks", help="optional checks/status JSON path")

    worktree = subparsers.add_parser("prepare-worktree", help="create or reuse review worktree")
    worktree.add_argument("--repo", default="/workspace/repo")
    worktree.add_argument("--review-root", default="/workspace/reviews")
    worktree.add_argument("--agent-id", required=True)
    worktree.add_argument("--pr", required=True, type=int)

    changed = subparsers.add_parser("changed-files", help="summarize changed files")
    changed.add_argument("--files", help="GitHub PR files JSON path")
    changed.add_argument("--repo", default=".")
    changed.add_argument("--base", help="base git rev for local diff")
    changed.add_argument("--head", help="head git rev for local diff")

    rust_plan = subparsers.add_parser("rust-plan", help="generate Rust validation commands")
    rust_plan.add_argument("--changed", help="changed-files JSON output path")
    rust_plan.add_argument("--files", help="GitHub PR files JSON path")
    rust_plan.add_argument("--repo", default=".")

    final_json = subparsers.add_parser("final-json", help="emit scheduler final JSON")
    final_json.add_argument(
        "--outcome",
        required=True,
        choices=["review_submitted", "no_eligible_pr", "failed"],
    )
    final_json.add_argument("--pr", type=int)
    final_json.add_argument("--summary", required=True)
    final_json.add_argument("--error")

    subparsers.add_parser("test", help="run built-in unit tests")

    args = parser.parse_args()
    if args.command == "select-pr":
        write_json(
            select_pr(
                load_json_path(args.prs),
                args.login,
                details=load_json_path(args.details) if args.details else None,
                reviews=load_json_path(args.reviews) if args.reviews else None,
                checks=load_json_path(args.checks) if args.checks else None,
            )
        )
    elif args.command == "prepare-worktree":
        write_json(prepare_worktree(args.repo, args.review_root, args.agent_id, args.pr))
    elif args.command == "changed-files":
        write_json(changed_files_summary(args.repo, files=args.files, base=args.base, head=args.head))
    elif args.command == "rust-plan":
        write_json(rust_plan_summary(args.repo, changed=args.changed, files=args.files))
    elif args.command == "final-json":
        write_json(final_result(args.outcome, args.pr, args.summary, args.error))
    elif args.command == "test":
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReviewPrHelperTests)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1
    return 0


def write_json(value: Any) -> None:
    print(json.dumps(value, ensure_ascii=False, separators=(",", ":")))


def load_json_path(path: str | None) -> Any:
    if not path or path == "-":
        raw = sys.stdin.read()
    else:
        raw = Path(path).read_text(encoding="utf-8")
    return unwrap_mcp_json(json.loads(raw))


def unwrap_mcp_json(value: Any) -> Any:
    """Accept raw JSON or common MCP wrappers containing JSON text."""
    if isinstance(value, dict) and isinstance(value.get("content"), list):
        texts = [
            item.get("text")
            for item in value["content"]
            if isinstance(item, dict) and isinstance(item.get("text"), str)
        ]
        if len(texts) == 1:
            try:
                return json.loads(texts[0])
            except json.JSONDecodeError:
                return texts[0]
    if isinstance(value, dict) and isinstance(value.get("text"), str):
        try:
            return json.loads(value["text"])
        except json.JSONDecodeError:
            return value
    return value


def select_pr(
    prs_json: Any,
    login: str,
    *,
    details: Any = None,
    reviews: Any = None,
    checks: Any = None,
) -> dict[str, Any]:
    prs = normalize_list(prs_json, list_keys=("pull_requests", "pullRequests", "items", "nodes"))
    details_by_pr = index_by_pr(details)
    reviews_by_pr = index_reviews_by_pr(reviews)
    checks_by_pr = index_by_pr(checks)
    skipped: list[dict[str, Any]] = []
    eligible: list[dict[str, Any]] = []

    for pr in prs:
        number = pr_number(pr)
        detail = merge_dicts(pr, details_by_pr.get(number))
        if author_login(detail) == login:
            skipped.append(skip(number, "self_authored"))
            continue
        if bool_field(detail, "isDraft", "draft"):
            skipped.append(skip(number, "draft"))
            continue

        requested = is_review_requested(detail, login)
        ci = ci_state(merge_dicts(detail, checks_by_pr.get(number)))
        if ci != "passed" and not requested:
            skipped.append(skip(number, f"ci_{ci}"))
            continue

        review_items = reviews_by_pr.get(number, [])
        last_review_at = latest_user_review_at(review_items, login)
        commit_at = latest_commit_at(detail)
        if last_review_at is not None and commit_at is None:
            skipped.append(skip(number, "missing_latest_commit_time"))
            continue
        if last_review_at is not None and commit_at is not None and commit_at <= last_review_at:
            skipped.append(skip(number, "already_reviewed_latest_commit"))
            continue

        eligible.append(
            {
                "number": number,
                "title": first_string(detail, "title"),
                "updated_at": first_string(detail, "updated_at", "updatedAt"),
                "head_sha": head_sha(detail),
                "requested_review": requested,
                "ci_state": ci,
                "source": pr,
            }
        )

    eligible.sort(
        key=lambda item: (
            1 if item["requested_review"] else 0,
            parse_time(item.get("updated_at")) or dt.datetime.min.replace(tzinfo=dt.timezone.utc),
            item.get("number") or 0,
        ),
        reverse=True,
    )
    if not eligible:
        return {"outcome": "no_eligible_pr", "selected_pr": None, "skipped": skipped}
    return {"outcome": "selected_pr", "selected_pr": eligible[0], "skipped": skipped}


def normalize_list(value: Any, list_keys: tuple[str, ...]) -> list[Any]:
    value = unwrap_mcp_json(value)
    if value is None:
        return []
    if isinstance(value, list):
        return [unwrap_mcp_json(item) for item in value]
    if isinstance(value, dict):
        for key in list_keys:
            if isinstance(value.get(key), list):
                return [unwrap_mcp_json(item) for item in value[key]]
        if isinstance(value.get("data"), dict):
            return normalize_list(value["data"], list_keys)
    return []


def index_by_pr(value: Any) -> dict[int | None, Any]:
    out: dict[int | None, Any] = {}
    if value is None:
        return out
    if isinstance(value, dict):
        for key, item in value.items():
            if isinstance(item, (dict, list)) and str(key).isdigit():
                out[int(key)] = unwrap_mcp_json(item)
        number = pr_number(value)
        if number is not None:
            out[number] = value
            return out
    for item in normalize_list(value, list_keys=("pull_requests", "pullRequests", "items", "nodes", "check_runs", "statuses")):
        number = pr_number(item)
        if number is not None:
            out[number] = item
    return out


def index_reviews_by_pr(value: Any) -> dict[int | None, list[Any]]:
    out: dict[int | None, list[Any]] = {}
    if value is None:
        return out
    if isinstance(value, dict):
        for key, item in value.items():
            if str(key).isdigit():
                out[int(key)] = normalize_list(item, list_keys=("reviews", "items", "nodes"))
        if out:
            return out
    for item in normalize_list(value, list_keys=("reviews", "items", "nodes")):
        out.setdefault(pr_number(item), []).append(item)
    return out


def merge_dicts(left: Any, right: Any) -> dict[str, Any]:
    merged: dict[str, Any] = {}
    if isinstance(left, dict):
        merged.update(left)
    if isinstance(right, dict):
        merged.update(right)
    return merged


def pr_number(value: Any) -> int | None:
    if not isinstance(value, dict):
        return None
    for key in ("number", "pull_number", "pullNumber", "pr", "iid"):
        if key in value:
            try:
                return int(value[key])
            except (TypeError, ValueError):
                return None
    url = first_string(value, "url", "html_url", "htmlUrl")
    if url:
        match = re.search(r"/pull/(\d+)", url)
        if match:
            return int(match.group(1))
    return None


def skip(number: int | None, reason: str) -> dict[str, Any]:
    return {"pr": number, "reason": reason}


def author_login(value: dict[str, Any]) -> str | None:
    for key in ("author", "user"):
        nested = value.get(key)
        if isinstance(nested, dict) and isinstance(nested.get("login"), str):
            return nested["login"]
    return first_string(value, "author_login", "authorLogin", "user_login", "userLogin")


def bool_field(value: dict[str, Any], *keys: str) -> bool:
    for key in keys:
        if key in value:
            return bool(value[key])
    return False


def first_string(value: Any, *keys: str) -> str | None:
    if not isinstance(value, dict):
        return None
    for key in keys:
        item = value.get(key)
        if isinstance(item, str):
            return item
    return None


def is_review_requested(value: dict[str, Any], login: str) -> bool:
    decision = first_string(value, "reviewDecision", "review_decision")
    if decision and decision.upper() != "REVIEW_REQUIRED":
        decision_required = False
    else:
        decision_required = decision is None or decision.upper() == "REVIEW_REQUIRED"
    reviewers = collect_logins(value.get("requested_reviewers"))
    reviewers |= collect_logins(value.get("requestedReviewers"))
    reviewers |= collect_logins(value.get("reviewRequests"))
    reviewers |= collect_logins(value.get("review_requests"))
    return decision_required and login in reviewers


def collect_logins(value: Any) -> set[str]:
    value = unwrap_mcp_json(value)
    if isinstance(value, dict):
        if isinstance(value.get("login"), str):
            return {value["login"]}
        out: set[str] = set()
        for key in ("nodes", "items", "users", "reviewers"):
            out |= collect_logins(value.get(key))
        requested_reviewer = value.get("requestedReviewer")
        if requested_reviewer is not None:
            out |= collect_logins(requested_reviewer)
        return out
    if isinstance(value, list):
        out = set()
        for item in value:
            out |= collect_logins(item)
        return out
    return set()


def ci_state(value: dict[str, Any]) -> str:
    runs = normalize_list(value.get("check_runs") or value.get("checkRuns"), list_keys=("check_runs", "checkRuns", "nodes"))
    statuses = normalize_list(value.get("statuses") or value.get("status"), list_keys=("statuses", "contexts", "nodes"))
    states = runs + statuses
    if not states:
        combined = first_string(value, "mergeable_state", "mergeStateStatus", "status", "conclusion")
        if combined:
            lowered = combined.lower()
            if lowered in {"clean", "success", "passed"}:
                return "passed"
            if lowered in {"failure", "failed", "dirty", "blocked"}:
                return "failed"
            if lowered in {"pending", "queued", "in_progress"}:
                return "pending"
        return "unknown"

    any_pending = False
    any_failed = False
    for item in states:
        if not isinstance(item, dict):
            continue
        status = str(item.get("status") or item.get("state") or "").lower()
        conclusion = str(item.get("conclusion") or item.get("result") or "").lower()
        if status and status not in {"completed", "success"}:
            any_pending = True
        if conclusion in FAILING_CONCLUSIONS or status in {"failure", "error", "failed"}:
            any_failed = True
        if conclusion and conclusion not in PASSING_CONCLUSIONS and status == "completed":
            any_failed = True
    if any_failed:
        return "failed"
    if any_pending:
        return "pending"
    return "passed"


def latest_user_review_at(reviews: list[Any], login: str) -> dt.datetime | None:
    times = []
    for review in reviews:
        if not isinstance(review, dict):
            continue
        if author_login(review) != login:
            continue
        submitted = parse_time(first_string(review, "submitted_at", "submittedAt", "created_at", "createdAt"))
        if submitted is not None:
            times.append(submitted)
    return max(times) if times else None


def latest_commit_at(value: dict[str, Any]) -> dt.datetime | None:
    for key in ("latest_commit_at", "latestCommitAt", "committedDate", "committed_at", "pushed_at"):
        parsed = parse_time(first_string(value, key))
        if parsed is not None:
            return parsed
    head = value.get("head")
    if isinstance(head, dict):
        commit = head.get("commit")
        if isinstance(commit, dict):
            parsed = parse_time(first_string(commit, "committedDate", "committed_at", "date"))
            if parsed is not None:
                return parsed
    commits = normalize_list(value.get("commits"), list_keys=("nodes", "items", "commits"))
    times = []
    for item in commits:
        if isinstance(item, dict):
            commit = item.get("commit") if isinstance(item.get("commit"), dict) else item
            parsed = parse_time(first_string(commit, "committedDate", "committed_at", "date", "authoredDate"))
            if parsed is not None:
                times.append(parsed)
    return max(times) if times else None


def head_sha(value: dict[str, Any]) -> str | None:
    head = value.get("head")
    if isinstance(head, dict):
        for key in ("sha", "oid"):
            if isinstance(head.get(key), str):
                return head[key]
    return first_string(value, "head_sha", "headSha", "headRefOid")


def parse_time(value: str | None) -> dt.datetime | None:
    if not value:
        return None
    normalized = value.strip()
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    try:
        parsed = dt.datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(dt.timezone.utc)


def run_git(repo: str | Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=check,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def prepare_worktree(repo: str, review_root: str, agent_id: str, pr: int) -> dict[str, Any]:
    repo_path = Path(repo)
    pr_ref = f"refs/remotes/origin/pr/{pr}"
    head = run_git(repo_path, "rev-parse", pr_ref).stdout.strip()
    agent_root = Path(review_root) / agent_id
    agent_root.mkdir(parents=True, exist_ok=True)

    for candidate in sorted(glob.glob(str(agent_root / f"review-pr-{pr}-*"))):
        path = Path(candidate)
        if not (path / ".git").exists():
            continue
        status = run_git(path, "status", "--short").stdout.strip()
        if status:
            continue
        current = run_git(path, "rev-parse", "HEAD").stdout.strip()
        if current == head:
            return {"worktree": str(path), "head_sha": head, "pr_ref": pr_ref, "action": "reused"}
        run_git(path, "checkout", "--detach", head)
        return {"worktree": str(path), "head_sha": head, "pr_ref": pr_ref, "action": "updated"}

    worktree = Path(tempfile.mkdtemp(prefix=f"review-pr-{pr}-", dir=str(agent_root)))
    worktree.rmdir()
    run_git(repo_path, "worktree", "add", "--detach", str(worktree), head)
    return {"worktree": str(worktree), "head_sha": head, "pr_ref": pr_ref, "action": "created"}


def changed_files_summary(
    repo: str,
    *,
    files: str | None = None,
    base: str | None = None,
    head: str | None = None,
) -> dict[str, Any]:
    repo_path = Path(repo)
    changed = changed_file_names(load_json_path(files)) if files else []
    if not changed and base and head:
        diff = run_git(repo_path, "diff", "--name-only", f"{base}...{head}").stdout
        changed = [line.strip() for line in diff.splitlines() if line.strip()]
    crates = changed_crates(repo_path, changed)
    return {
        "files": changed,
        "changed_crates": crates,
        "is_rust_workspace": (repo_path / "Cargo.toml").exists(),
        "cargo_tomls": [str(Path(crate) / "Cargo.toml") for crate in crates],
    }


def changed_file_names(value: Any) -> list[str]:
    names: list[str] = []
    for item in normalize_list(value, list_keys=("files", "items", "nodes")):
        if isinstance(item, str):
            names.append(item)
        elif isinstance(item, dict):
            name = first_string(item, "filename", "path", "name")
            if name:
                names.append(name)
    return sorted(dict.fromkeys(names))


def changed_crates(repo: Path, files: list[str]) -> list[str]:
    crates: list[str] = []
    for name in files:
        parts = Path(name).parts
        if len(parts) >= 2 and parts[0] == "crates":
            crate = str(Path(parts[0]) / parts[1])
            if (repo / crate / "Cargo.toml").exists() and crate not in crates:
                crates.append(crate)
    return sorted(crates)


def rust_plan_summary(repo: str, *, changed: str | None = None, files: str | None = None) -> dict[str, Any]:
    repo_path = Path(repo)
    if changed:
        summary = load_json_path(changed)
    else:
        summary = changed_files_summary(repo, files=files)
    crates = summary.get("changed_crates") if isinstance(summary, dict) else []
    if not isinstance(crates, list):
        crates = []
    is_rust = (repo_path / "Cargo.toml").exists() or any((repo_path / crate / "Cargo.toml").exists() for crate in crates)
    commands: list[dict[str, str]] = []
    if is_rust:
        commands.append({"kind": "fmt", "cwd": str(repo_path), "command": "cargo fmt --check"})
        valid_crates = [crate for crate in crates if (repo_path / crate / "Cargo.toml").exists()]
        if valid_crates:
            for crate in valid_crates:
                manifest = str(Path(crate) / "Cargo.toml")
                commands.append(
                    {
                        "kind": "clippy",
                        "cwd": str(repo_path),
                        "command": f"cargo clippy --manifest-path {manifest} --all-features -- -D warnings",
                    }
                )
                commands.append(
                    {
                        "kind": "test",
                        "cwd": str(repo_path),
                        "command": f"cargo test --manifest-path {manifest} --all-features",
                    }
                )
        elif (repo_path / "Cargo.toml").exists():
            commands.append(
                {
                    "kind": "clippy",
                    "cwd": str(repo_path),
                    "command": "cargo clippy --all-features -- -D warnings",
                }
            )
            commands.append({"kind": "test", "cwd": str(repo_path), "command": "cargo test --all-features"})
    return {"is_rust_workspace": is_rust, "changed_crates": crates, "commands": commands}


def final_result(outcome: str, pr: int | None, summary: str, error: str | None) -> dict[str, Any]:
    if outcome == "review_submitted" and pr is None:
        raise SystemExit("review_submitted requires --pr")
    if outcome == "no_eligible_pr":
        pr = None
        error = None
    if outcome == "failed" and not error:
        raise SystemExit("failed requires --error")
    return {"outcome": outcome, "pr": pr, "summary": summary, "error": error}


class ReviewPrHelperTests(unittest.TestCase):
    def test_select_pr_skips_self_and_draft(self) -> None:
        result = select_pr(
            [
                {"number": 1, "author": {"login": "me"}, "check_runs": [{"status": "completed", "conclusion": "success"}]},
                {"number": 2, "author": {"login": "alice"}, "isDraft": True, "check_runs": [{"status": "completed", "conclusion": "success"}]},
            ],
            "me",
        )
        self.assertEqual(result["outcome"], "no_eligible_pr")
        self.assertEqual([item["reason"] for item in result["skipped"]], ["self_authored", "draft"])

    def test_select_pr_ci_passed_is_eligible(self) -> None:
        result = select_pr(
            [
                {
                    "number": 3,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-01T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                }
            ],
            "me",
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["number"], 3)

    def test_select_pr_failed_ci_only_when_review_requested(self) -> None:
        result = select_pr(
            [
                {
                    "number": 4,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-01T00:00:00Z",
                    "reviewDecision": "REVIEW_REQUIRED",
                    "requested_reviewers": [{"login": "me"}],
                    "check_runs": [{"status": "completed", "conclusion": "failure"}],
                }
            ],
            "me",
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["ci_state"], "failed")

    def test_select_pr_requires_new_commit_after_review(self) -> None:
        prs = [
            {
                "number": 5,
                "author": {"login": "alice"},
                "updated_at": "2026-01-01T00:00:00Z",
                "latest_commit_at": "2026-01-01T00:00:00Z",
                "check_runs": [{"status": "completed", "conclusion": "success"}],
            },
            {
                "number": 6,
                "author": {"login": "bob"},
                "updated_at": "2026-01-02T00:00:00Z",
                "latest_commit_at": "2026-01-03T00:00:00Z",
                "check_runs": [{"status": "completed", "conclusion": "success"}],
            },
        ]
        reviews = {
            "5": [{"user": {"login": "me"}, "submitted_at": "2026-01-02T00:00:00Z"}],
            "6": [{"user": {"login": "me"}, "submitted_at": "2026-01-02T00:00:00Z"}],
        }
        result = select_pr(prs, "me", reviews=reviews)
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["number"], 6)
        self.assertEqual(result["skipped"][0]["reason"], "already_reviewed_latest_commit")

    def test_requested_review_priority_then_updated_at(self) -> None:
        result = select_pr(
            [
                {
                    "number": 7,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-03T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
                {
                    "number": 8,
                    "author": {"login": "bob"},
                    "updated_at": "2026-01-01T00:00:00Z",
                    "reviewDecision": "REVIEW_REQUIRED",
                    "requested_reviewers": [{"login": "me"}],
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
            ],
            "me",
        )
        self.assertEqual(result["selected_pr"]["number"], 8)

    def test_changed_files_extracts_crates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "crates/demo").mkdir(parents=True)
            (root / "crates/demo/Cargo.toml").write_text("[package]\nname='demo'\n", encoding="utf-8")
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            files = root / "files.json"
            files.write_text(json.dumps([{"filename": "crates/demo/src/lib.rs"}, {"filename": "README.md"}]), encoding="utf-8")
            summary = changed_files_summary(str(root), files=str(files))
        self.assertEqual(summary["changed_crates"], ["crates/demo"])
        self.assertTrue(summary["is_rust_workspace"])

    def test_final_json_shape(self) -> None:
        result = final_result("review_submitted", 9, "Submitted APPROVE.", None)
        self.assertEqual(set(result.keys()), {"outcome", "pr", "summary", "error"})
        self.assertEqual(result["pr"], 9)
        self.assertIsNone(result["error"])


if __name__ == "__main__":
    raise SystemExit(main())
