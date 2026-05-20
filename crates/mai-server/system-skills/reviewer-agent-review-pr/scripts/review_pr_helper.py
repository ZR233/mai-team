#!/usr/bin/env python3
"""Deterministic helpers for Mai reviewer-agent-review-pr.

The helper intentionally avoids GitHub credentials and network access. Feed it
JSON captured from visible Mai GitHub API tools and local git state; it returns
small JSON objects that the reviewer can rely on for repetitive rules.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

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
        choices=["review_submitted", "failed"],
    )
    final_json.add_argument("--review-event", choices=["approve", "request_changes", "comment"])
    final_json.add_argument("--pr", type=int)
    final_json.add_argument("--summary", required=True)
    final_json.add_argument("--error")

    subparsers.add_parser("test", help="run built-in unit tests")

    args = parser.parse_args()
    if args.command == "prepare-review":
        write_json(prepare_review_checkout(args.repo, args.agent_id, args.pr))
    elif args.command == "changed-files":
        write_json(changed_files_summary(args.repo, files=args.files, base=args.base, head=args.head))
    elif args.command == "rust-plan":
        write_json(rust_plan_summary(args.repo, changed=args.changed, files=args.files))
    elif args.command == "final-json":
        write_json(final_result(args.outcome, args.review_event, args.pr, args.summary, args.error))
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


def first_string(value: Any, *keys: str) -> str | None:
    if not isinstance(value, dict):
        return None
    for key in keys:
        item = value.get(key)
        if isinstance(item, str):
            return item
    return None


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


def final_result(
    outcome: str,
    review_event: str | None,
    pr: int | None,
    summary: str,
    error: str | None,
) -> dict[str, Any]:
    if outcome == "review_submitted" and pr is None:
        raise SystemExit("review_submitted requires --pr")
    if outcome == "review_submitted" and review_event is None:
        raise SystemExit("review_submitted requires --review-event")
    if outcome != "review_submitted" and review_event is not None:
        raise SystemExit("--review-event is only valid for review_submitted")
    if outcome == "failed" and not error:
        raise SystemExit("failed requires --error")
    return {
        "outcome": outcome,
        "review_event": review_event,
        "pr": pr,
        "summary": summary,
        "error": error,
    }


class ReviewPrHelperTests(unittest.TestCase):
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
        result = final_result("review_submitted", "approve", 9, "Submitted APPROVE.", None)
        self.assertEqual(set(result.keys()), {"outcome", "review_event", "pr", "summary", "error"})
        self.assertEqual(result["review_event"], "approve")
        self.assertEqual(result["pr"], 9)
        self.assertIsNone(result["error"])

    def test_final_json_requires_review_event_for_submitted_review(self) -> None:
        with self.assertRaises(SystemExit):
            final_result("review_submitted", None, 9, "Submitted review.", None)


if __name__ == "__main__":
    raise SystemExit(main())
