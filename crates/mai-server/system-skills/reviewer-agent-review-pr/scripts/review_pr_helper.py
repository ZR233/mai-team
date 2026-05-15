#!/usr/bin/env python3
"""Deterministic helpers for Mai reviewer-agent-review-pr.

The helper intentionally avoids GitHub credentials and network access. Feed it
JSON captured from visible GitHub MCP tools and local git state; it returns
small JSON objects that the reviewer can rely on for repetitive rules.
"""

from __future__ import annotations

import argparse
import datetime as dt
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
    select.add_argument("--target-pr", type=int, help="only consider this PR number")

    select_many = subparsers.add_parser("select-prs", help="select all eligible PRs")
    select_many.add_argument("--prs", default="-", help="PR list JSON path, or - for stdin")
    select_many.add_argument("--login", required=True, help="authenticated GitHub login")
    select_many.add_argument("--details", help="optional PR details JSON path")
    select_many.add_argument("--reviews", help="optional reviews JSON path")
    select_many.add_argument("--checks", help="optional checks/status JSON path")

    prepare = subparsers.add_parser("prepare-review", help="check out the selected PR in this clone")
    prepare.add_argument("--repo", default="/workspace/repo")
    prepare.add_argument("--agent-id", required=True)
    prepare.add_argument("--pr", required=True, type=int)

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
                target_pr=args.target_pr,
            )
        )
    elif args.command == "select-prs":
        write_json(
            select_prs(
                load_json_path(args.prs),
                args.login,
                details=load_json_path(args.details) if args.details else None,
                reviews=load_json_path(args.reviews) if args.reviews else None,
                checks=load_json_path(args.checks) if args.checks else None,
            )
        )
    elif args.command == "prepare-review":
        write_json(prepare_review_checkout(args.repo, args.agent_id, args.pr))
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
    if isinstance(value, dict):
        for key in ("content", "contents"):
            if isinstance(value.get(key), list):
                texts = [
                    item.get("text")
                    for item in value[key]
                    if isinstance(item, dict) and isinstance(item.get("text"), str)
                ]
                if len(texts) == 1:
                    try:
                        return json.loads(texts[0])
                    except json.JSONDecodeError:
                        return texts[0]
        if isinstance(value.get("text"), str):
            try:
                return json.loads(value["text"])
            except json.JSONDecodeError:
                return value
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
    target_pr: int | None = None,
) -> dict[str, Any]:
    result = select_prs(
        prs_json,
        login,
        details=details,
        reviews=reviews,
        checks=checks,
        target_pr=target_pr,
    )
    selected = result["selected_prs"][0] if result["selected_prs"] else None
    if selected is None:
        return {
            "outcome": "no_eligible_pr",
            "selected_pr": None,
            "target_pr": target_pr,
            "skipped": result["skipped"],
        }
    return {
        "outcome": "selected_pr",
        "selected_pr": selected,
        "target_pr": target_pr,
        "skipped": result["skipped"],
    }


def select_prs(
    prs_json: Any,
    login: str,
    *,
    details: Any = None,
    reviews: Any = None,
    checks: Any = None,
    target_pr: int | None = None,
) -> dict[str, Any]:
    prs = normalize_list(prs_json, list_keys=("pull_requests", "pullRequests", "items", "nodes"))
    details_by_pr = index_by_pr(details)
    reviews_by_pr = index_reviews_by_pr(reviews)
    checks_by_pr = index_by_pr(checks)
    skipped: list[dict[str, Any]] = []
    eligible: list[dict[str, Any]] = []

    for pr in prs:
        number = pr_number(pr)
        if target_pr is not None and number != target_pr:
            continue
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
        last_review = latest_user_review(review_items, login)
        last_review_at = parse_time(first_string(last_review, "submitted_at", "submittedAt", "created_at", "createdAt"))
        last_review_commit_id = first_string(last_review, "commit_id", "commitId")
        commit_at = latest_commit_at(detail)
        current_head_sha = head_sha(detail)
        if (
            last_review is not None
            and current_head_sha is not None
            and last_review_commit_id is not None
            and current_head_sha == last_review_commit_id
        ):
            skipped.append(skip(number, "already_reviewed_latest_commit"))
            continue
        if (
            last_review_at is not None
            and commit_at is None
            and (current_head_sha is None or last_review_commit_id is None)
        ):
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
        return {
            "outcome": "no_eligible_pr",
            "selected_prs": [],
            "target_pr": target_pr,
            "skipped": skipped,
        }
    return {
        "outcome": "selected_prs",
        "selected_prs": eligible,
        "target_pr": target_pr,
        "skipped": skipped,
    }


def normalize_list(value: Any, list_keys: tuple[str, ...]) -> list[Any]:
    value = unwrap_mcp_json(value)
    if value is None:
        return []
    if isinstance(value, list):
        items = []
        for item in value:
            if isinstance(item, dict) and "node" in item:
                items.append(unwrap_mcp_json(item["node"]))
            else:
                items.append(unwrap_mcp_json(item))
        return items
    if isinstance(value, dict):
        for key in list_keys:
            if key in value:
                items = normalize_list(value[key], list_keys)
                if items or value[key] == []:
                    return items
        if isinstance(value.get("edges"), list):
            return normalize_list(value["edges"], list_keys)
        for container_key in (
            "data",
            "repository",
            "organization",
            "viewer",
            "pullRequest",
            "pullRequests",
            "reviews",
            "reviewRequests",
            "files",
            "statusCheckRollup",
            "checkSuites",
            "checkRuns",
        ):
            nested = value.get(container_key)
            if isinstance(nested, (dict, list)):
                items = normalize_list(nested, list_keys)
                if items:
                    return items
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
    if isinstance(value, str):
        return {value}
    if isinstance(value, dict):
        if isinstance(value.get("login"), str):
            return {value["login"]}
        out: set[str] = set()
        for key in ("nodes", "items", "users", "reviewers", "edges"):
            out |= collect_logins(value.get(key))
        node = value.get("node")
        if node is not None:
            out |= collect_logins(node)
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
    review = latest_user_review(reviews, login)
    return parse_time(first_string(review, "submitted_at", "submittedAt", "created_at", "createdAt"))


def latest_user_review(reviews: list[Any], login: str) -> dict[str, Any] | None:
    latest: tuple[dt.datetime, dict[str, Any]] | None = None
    for review in reviews:
        if not isinstance(review, dict):
            continue
        if author_login(review) != login:
            continue
        submitted = parse_time(first_string(review, "submitted_at", "submittedAt", "created_at", "createdAt"))
        if submitted is None:
            continue
        if latest is None or submitted > latest[0]:
            latest = (submitted, review)
    return latest[1] if latest is not None else None


def latest_user_review_at_legacy(reviews: list[Any], login: str) -> dt.datetime | None:
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
    head_ref = value.get("headRef")
    if isinstance(head_ref, dict):
        target = head_ref.get("target")
        if isinstance(target, dict):
            parsed = parse_time(first_string(target, "committedDate", "committed_at", "pushedDate", "date"))
            if parsed is not None:
                return parsed
            history = normalize_list(target.get("history"), list_keys=("nodes", "items", "edges"))
            times = []
            for item in history:
                if isinstance(item, dict):
                    parsed = parse_time(first_string(item, "committedDate", "committed_at", "pushedDate", "date"))
                    if parsed is not None:
                        times.append(parsed)
            if times:
                return max(times)
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
    head_ref = value.get("headRef")
    if isinstance(head_ref, dict):
        for key in ("oid",):
            if isinstance(head_ref.get(key), str):
                return head_ref[key]
        target = head_ref.get("target")
        if isinstance(target, dict) and isinstance(target.get("oid"), str):
            return target["oid"]
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
    completed = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and completed.returncode != 0:
        command = " ".join(["git", "-C", str(repo), *args])
        message = f"git command failed ({completed.returncode}): {command}"
        if completed.stderr.strip():
            message += f"\nstderr:\n{completed.stderr.strip()}"
        if completed.stdout.strip():
            message += f"\nstdout:\n{completed.stdout.strip()}"
        raise SystemExit(message)
    return completed


def prepare_review_checkout(repo: str, agent_id: str, pr: int) -> dict[str, Any]:
    repo_path = Path(repo)
    pr_ref, head = resolve_pr_ref(repo_path, pr)
    branch = f"mai-review/{pr}/{agent_id}"
    run_git(repo_path, "reset", "--hard", "HEAD")
    run_git(repo_path, "clean", "-fdx")
    run_git(repo_path, "checkout", "-B", branch, head)
    run_git(repo_path, "reset", "--hard", head)
    run_git(repo_path, "clean", "-fdx")
    current = run_git(repo_path, "rev-parse", "HEAD").stdout.strip()
    return {
        "repo": str(repo_path),
        "head_sha": current,
        "pr_ref": pr_ref,
        "branch": branch,
        "action": "checked_out",
    }


def resolve_pr_ref(repo: Path, pr: int) -> tuple[str, str]:
    refs = [
        f"refs/remotes/origin/pr/{pr}",
        f"refs/pull/{pr}/head",
        f"refs/remotes/origin/pull/{pr}/head",
    ]
    for ref in refs:
        completed = run_git(repo, "rev-parse", "--verify", ref, check=False)
        if completed.returncode == 0:
            return ref, completed.stdout.strip()
    joined = ", ".join(refs)
    raise SystemExit(f"could not find PR ref for #{pr}; tried {joined}")


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
        path = Path(name)
        if path.is_absolute() or ".." in path.parts:
            continue
        parts = path.parts
        if len(parts) >= 2 and parts[0] == "crates":
            crate = str(Path(parts[0]) / parts[1])
            if (repo / crate / "Cargo.toml").exists() and crate not in crates:
                crates.append(crate)
                continue
        crate = nearest_package_crate(repo, path)
        if crate and crate not in crates:
            crates.append(crate)
    return sorted(crates)


def nearest_package_crate(repo: Path, path: Path) -> str | None:
    for parent in path.parents:
        if str(parent) in {"", "."}:
            continue
        manifest = repo / parent / "Cargo.toml"
        if manifest_is_package(manifest):
            return str(parent)
    return None


def manifest_is_package(path: Path) -> bool:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return False
    return re.search(r"(?m)^\s*\[package\]\s*$", text) is not None


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

    def test_select_prs_returns_all_eligible_prs(self) -> None:
        result = select_prs(
            [
                {
                    "number": 5,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-05T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
                {
                    "number": 2,
                    "author": {"login": "bob"},
                    "updated_at": "2026-01-02T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
                {
                    "number": 7,
                    "author": {"login": "me"},
                    "updated_at": "2026-01-07T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
            ],
            "me",
        )

        self.assertEqual(result["outcome"], "selected_prs")
        self.assertEqual([item["number"] for item in result["selected_prs"]], [5, 2])
        self.assertEqual(result["skipped"], [{"pr": 7, "reason": "self_authored"}])

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

    def test_select_pr_accepts_graphql_connection_wrappers(self) -> None:
        result = select_pr(
            {
                "data": {
                    "repository": {
                        "pullRequests": {
                            "nodes": [
                                {
                                    "number": 10,
                                    "author": {"login": "alice"},
                                    "updatedAt": "2026-01-04T00:00:00Z",
                                    "headRef": {
                                        "target": {
                                            "oid": "abc123",
                                            "committedDate": "2026-01-04T00:00:00Z",
                                        }
                                    },
                                    "checkRuns": {
                                        "nodes": [{"status": "COMPLETED", "conclusion": "SUCCESS"}]
                                    },
                                }
                            ]
                        }
                    }
                }
            },
            "me",
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["number"], 10)
        self.assertEqual(result["selected_pr"]["head_sha"], "abc123")

    def test_changed_files_accepts_mcp_contents_wrapper(self) -> None:
        names = changed_file_names(
            {
                "contents": [
                    {
                        "mimeType": "application/json",
                        "text": json.dumps(
                            {"files": [{"filename": "crates/demo/src/lib.rs"}, {"filename": "README.md"}]}
                        ),
                    }
                ]
            }
        )
        self.assertEqual(names, ["README.md", "crates/demo/src/lib.rs"])

    def test_select_pr_uses_head_ref_commit_time_for_rereview(self) -> None:
        result = select_pr(
            [
                {
                    "number": 11,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-05T00:00:00Z",
                    "headRef": {
                        "target": {
                            "committedDate": "2026-01-05T00:00:00Z",
                            "oid": "def456",
                        }
                    },
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                }
            ],
            "me",
            reviews={"11": [{"user": {"login": "me"}, "submitted_at": "2026-01-04T00:00:00Z"}]},
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["head_sha"], "def456")

    def test_select_pr_accepts_requested_reviewer_string_list(self) -> None:
        result = select_pr(
            [
                {
                    "number": 12,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-06T00:00:00Z",
                    "requested_reviewers": ["me"],
                    "check_runs": [{"status": "completed", "conclusion": "failure"}],
                }
            ],
            "me",
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertTrue(result["selected_pr"]["requested_review"])

    def test_select_pr_uses_review_commit_id_when_commit_time_missing(self) -> None:
        result = select_pr(
            [
                {
                    "number": 13,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-07T00:00:00Z",
                    "head": {"sha": "new-head"},
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                }
            ],
            "me",
            reviews={
                "13": [
                    {
                        "user": {"login": "me"},
                        "commit_id": "old-head",
                        "submitted_at": "2026-01-06T00:00:00Z",
                    }
                ]
            },
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["number"], 13)

    def test_select_pr_skips_when_review_commit_matches_head(self) -> None:
        result = select_pr(
            [
                {
                    "number": 14,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-08T00:00:00Z",
                    "head": {"sha": "same-head"},
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                }
            ],
            "me",
            reviews={
                "14": [
                    {
                        "user": {"login": "me"},
                        "commit_id": "same-head",
                        "submitted_at": "2026-01-07T00:00:00Z",
                    }
                ]
            },
        )
        self.assertEqual(result["outcome"], "no_eligible_pr")
        self.assertEqual(result["skipped"][0]["reason"], "already_reviewed_latest_commit")

    def test_select_pr_target_pr_only_considers_requested_number(self) -> None:
        result = select_pr(
            [
                {
                    "number": 20,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-08T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
                {
                    "number": 21,
                    "author": {"login": "bob"},
                    "updated_at": "2026-01-09T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
            ],
            "me",
            target_pr=20,
        )
        self.assertEqual(result["outcome"], "selected_pr")
        self.assertEqual(result["selected_pr"]["number"], 20)
        self.assertEqual(result["target_pr"], 20)

    def test_select_pr_target_pr_can_be_ineligible(self) -> None:
        result = select_pr(
            [
                {
                    "number": 22,
                    "author": {"login": "alice"},
                    "updated_at": "2026-01-08T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
                {
                    "number": 23,
                    "author": {"login": "me"},
                    "updated_at": "2026-01-09T00:00:00Z",
                    "check_runs": [{"status": "completed", "conclusion": "success"}],
                },
            ],
            "me",
            target_pr=23,
        )
        self.assertEqual(result["outcome"], "no_eligible_pr")
        self.assertEqual(result["target_pr"], 23)
        self.assertEqual(result["skipped"][0]["reason"], "self_authored")

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

    def test_changed_files_extracts_nested_package_crates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "os/StarryOS/kernel/src").mkdir(parents=True)
            (root / "os/StarryOS/kernel/Cargo.toml").write_text(
                "[package]\nname='starry-kernel'\n", encoding="utf-8"
            )
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            files = root / "files.json"
            files.write_text(
                json.dumps(
                    [
                        {"filename": "os/StarryOS/kernel/src/syscall/ipc/msg.rs"},
                        {"filename": "test-suit/starryos/normal/test-msg/test.c"},
                    ]
                ),
                encoding="utf-8",
            )
            summary = changed_files_summary(str(root), files=str(files))
        self.assertEqual(summary["changed_crates"], ["os/StarryOS/kernel"])
        self.assertEqual(summary["cargo_tomls"], ["os/StarryOS/kernel/Cargo.toml"])

    def test_changed_files_ignores_workspace_root_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            files = root / "files.json"
            files.write_text(json.dumps([{"filename": "test-suit/starryos/normal/test.c"}]), encoding="utf-8")
            summary = changed_files_summary(str(root), files=str(files))
        self.assertEqual(summary["changed_crates"], [])

    def test_prepare_review_checkout_uses_current_clone(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-b", "main", str(root)], check=True, stdout=subprocess.PIPE)
            (root / "README.md").write_text("main\n", encoding="utf-8")
            run_git(root, "add", "README.md")
            run_git(root, "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-m", "main")
            main_head = run_git(root, "rev-parse", "HEAD").stdout.strip()
            (root / "README.md").write_text("pr\n", encoding="utf-8")
            run_git(root, "add", "README.md")
            run_git(root, "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-m", "pr")
            pr_head = run_git(root, "rev-parse", "HEAD").stdout.strip()
            run_git(root, "update-ref", "refs/remotes/origin/pr/7", pr_head)
            run_git(root, "checkout", "-B", "main", main_head)

            result = prepare_review_checkout(str(root), "agent-1", 7)

        self.assertEqual(result["repo"], str(root))
        self.assertEqual(result["head_sha"], pr_head)
        self.assertEqual(result["pr_ref"], "refs/remotes/origin/pr/7")
        self.assertEqual(result["branch"], "mai-review/7/agent-1")
        self.assertEqual(result["action"], "checked_out")

    def test_final_json_shape(self) -> None:
        result = final_result("review_submitted", 9, "Submitted APPROVE.", None)
        self.assertEqual(set(result.keys()), {"outcome", "pr", "summary", "error"})
        self.assertEqual(result["pr"], 9)
        self.assertIsNone(result["error"])


if __name__ == "__main__":
    raise SystemExit(main())
