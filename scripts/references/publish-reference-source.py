#!/usr/bin/env python3
"""从本地 Git 对象发布一个不含 .git 的只读源码快照（Linux）。

不会执行 checkout/filter/hook/archive，不下载 LFS 或子模块。
生产默认要求 root；--allow-unprivileged 仅供测试。
本脚本不修改 mai-team 数据库，也不完成 Docker/服务端验收。
"""
from __future__ import annotations

import argparse
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile
from typing import BinaryIO

CHUNK_SIZE = 1024 * 1024
OID = re.compile(r"^[0-9a-f]{40}(?:[0-9a-f]{24})?$")


class PublishError(RuntimeError):
    """输入不符合安全发布规则或 Git 操作失败。"""


def git_environment() -> dict[str, str]:
    # 不继承 GIT_DIR/WORK_TREE/CONFIG_COUNT 等可能改变对象来源的覆盖配置。
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update({
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_OPTIONAL_LOCKS": "0",
        "GIT_NO_LAZY_FETCH": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_ALLOW_PROTOCOL": "",  # 禁止对象缺失时通过传输协议拉取。
        "LC_ALL": "C",
    })
    return env


def git_command(repo: Path, *args: str) -> list[str]:
    return ["git", "--no-optional-locks", "--no-replace-objects", "-c",
            "core.hooksPath=/dev/null", "-C", str(repo), *args]


def run_git(repo: Path, *args: str) -> bytes:
    result = subprocess.run(git_command(repo, *args), env=git_environment(),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            check=False)
    if result.returncode:
        message = result.stderr.decode("utf-8", errors="replace")[-2000:]
        raise PublishError(f"Git 读取失败：{message.strip()}")
    return result.stdout


def safe_relative_path(raw: bytes) -> str:
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as exc:
        raise PublishError("暂不支持非 UTF-8 的 Git 路径") from exc
    parts = text.split("/")
    if (not text or text.startswith("/") or any(x in ("", ".", "..", ".git") for x in parts)
            or any(ord(c) < 32 or ord(c) == 127 for c in text)):
        raise PublishError(f"不安全或不支持的路径：{text!r}")
    if str(PurePosixPath(text)) != text:
        raise PublishError(f"非规范路径：{text!r}")
    return text


def parse_tree(data: bytes) -> list[tuple[str, str, str]]:
    entries: list[tuple[str, str, str]] = []
    seen: set[str] = set()
    for record in data.split(b"\0"):
        if not record:
            continue
        try:
            header, raw_path = record.split(b"\t", 1)
            mode_b, kind_b, oid_b = header.split(b" ")
            mode, kind, oid = mode_b.decode(), kind_b.decode(), oid_b.decode()
        except (ValueError, UnicodeDecodeError) as exc:
            raise PublishError("无法解析 git ls-tree 记录") from exc
        path = safe_relative_path(raw_path)
        if mode == "160000" or kind == "commit":
            raise PublishError(f"不自动发布子模块：{path}；请单独发布并登记")
        if mode not in ("100644", "100755", "120000") or kind != "blob" or not OID.fullmatch(oid):
            raise PublishError(f"不支持的 Git 对象：{mode} {kind} {path}")
        if path in seen:
            raise PublishError(f"重复路径：{path}")
        seen.add(path)
        entries.append((path, mode, oid))
    if not entries:
        raise PublishError("拒绝发布空源码树")
    entries.sort(key=lambda item: item[0])
    # 所有叶对象都不能是另一条路径的父目录，防止恶意树导致写入穿越 symlink。
    leaves = {p for p, _, _ in entries}
    for path, _, _ in entries:
        for parent in PurePosixPath(path).parents:
            if str(parent) in leaves:
                raise PublishError(f"文件/符号链接与目录路径冲突：{path}")
    return entries


def fsync_directory(path: Path) -> None:
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic_publish_noreplace(staging: Path, output: Path) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    renameat2 = getattr(libc, "renameat2", None)
    if renameat2 is None:
        raise PublishError("本机 libc 缺少 renameat2；不采用可能覆盖目标的降级发布")
    renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int,
                          ctypes.c_char_p, ctypes.c_uint]
    renameat2.restype = ctypes.c_int
    if renameat2(-100, os.fsencode(staging), -100, os.fsencode(output), 1) != 0:
        code = ctypes.get_errno()
        if code == errno.EEXIST:
            raise PublishError(f"目标已存在，拒绝覆盖：{output}")
        raise OSError(code, os.strerror(code), str(output))


def cleanup_staging(path: Path) -> None:
    # 仅清理本进程创建、尚未发布的临时目录；不跟随 symlink。
    if not path.exists():
        return
    for directory, _, _ in os.walk(path, followlinks=False):
        os.chmod(directory, 0o700)
    shutil.rmtree(path)


def read_exact(stream: BinaryIO, count: int) -> bytes:
    data = stream.read(count)
    if len(data) != count:
        raise PublishError("Git 对象数据提前结束")
    return data


def validate_output_parent(output: Path, allow_unprivileged: bool) -> None:
    if os.path.lexists(output):
        raise PublishError(f"目标已存在，拒绝覆盖：{output}")
    if not output.parent.is_dir():
        raise PublishError(f"输出父目录需事先由管理员创建：{output.parent}")
    if output.parent.resolve(strict=True) != output.parent:
        raise PublishError("输出祖先目录不能使用符号链接")
    if not allow_unprivileged:
        for path in (output.parent, *output.parent.parents):
            info = path.stat()
            if info.st_uid != 0 or info.st_mode & 0o022:
                raise PublishError(f"生产输出祖先必须 root 持有且无组/其他用户写权限：{path}")


def publish(repo: Path, revision: str, output: Path, allow_unprivileged: bool) -> dict[str, object]:
    if sys.platform != "linux":
        raise PublishError("本脚本只支持 Linux")
    if not allow_unprivileged and os.geteuid() != 0:
        raise PublishError("生产发布请以 root 执行；本地测试可加 --allow-unprivileged")
    repo = repo.resolve(strict=True)
    validate_output_parent(output, allow_unprivileged)
    commit = run_git(repo, "rev-parse", "--verify", "--end-of-options",
                     f"{revision}^{{commit}}").decode().strip()
    tree = run_git(repo, "rev-parse", "--verify", "--end-of-options",
                   f"{commit}^{{tree}}").decode().strip()
    if not OID.fullmatch(commit) or not OID.fullmatch(tree):
        raise PublishError("Git 返回了无效对象 ID")
    entries = parse_tree(run_git(repo, "ls-tree", "-r", "-z", "--full-tree", commit))
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.publishing-", dir=output.parent))
    root = staging / "repo"
    root.mkdir()
    manifest_entries: list[dict[str, object]] = []
    links: list[tuple[Path, str]] = []
    total_bytes = 0
    try:
        with tempfile.TemporaryFile() as errors:
            proc = subprocess.Popen(git_command(repo, "cat-file", "--batch"),
                                    env=git_environment(), stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=errors)
            assert proc.stdin is not None and proc.stdout is not None
            try:
                for relative, mode, oid in entries:
                    proc.stdin.write((oid + "\n").encode("ascii"))
                    proc.stdin.flush()
                    header = proc.stdout.readline().decode("ascii", errors="strict").strip().split()
                    if len(header) != 3 or header[0] != oid or header[1] != "blob":
                        raise PublishError(f"Git blob 缺失/格式错误：{relative}")
                    size = int(header[2])
                    if size < 0 or size > 512 * 1024 * 1024:
                        raise PublishError(f"单文件超过 512 MiB 发布上限：{relative}")
                    if mode == "120000" and size > 65536:
                        raise PublishError(f"符号链接过长：{relative}")
                    destination = root / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    digest = hashlib.sha256()
                    if mode == "120000":
                        raw = read_exact(proc.stdout, size)
                        digest.update(raw)
                        target = raw.decode("utf-8", errors="strict")
                        if (not target or target.startswith("/") or
                                any(ord(c) < 32 or ord(c) == 127 for c in target)):
                            raise PublishError(f"不安全的符号链接目标：{relative}")
                        links.append((destination, target))
                    else:
                        with destination.open("xb") as handle:
                            remaining = size
                            while remaining:
                                chunk = read_exact(proc.stdout, min(CHUNK_SIZE, remaining))
                                handle.write(chunk)
                                digest.update(chunk)
                                remaining -= len(chunk)
                            handle.flush()
                            os.fchmod(handle.fileno(), 0o555 if mode == "100755" else 0o444)
                            os.fsync(handle.fileno())
                    if read_exact(proc.stdout, 1) != b"\n":
                        raise PublishError("Git batch 分隔符错误")
                    manifest_entries.append({
                        "path": relative, "git_mode": mode, "git_blob": oid,
                        "kind": "symlink" if mode == "120000" else "file",
                        "size": size, "sha256": digest.hexdigest(),
                    })
                    total_bytes += size
                proc.stdin.close()
                if proc.wait(timeout=30) != 0:
                    raise PublishError("git cat-file 返回非零退出状态")
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
                proc.stdout.close()
                if not proc.stdin.closed:
                    proc.stdin.close()
        for destination, target in links:
            destination.symlink_to(target)
        for destination, _ in links:
            try:
                resolved = destination.resolve(strict=True)
            except (OSError, RuntimeError) as exc:
                raise PublishError(f"符号链接断链或循环：{destination.relative_to(root)}") from exc
            if not resolved.is_relative_to(root):
                raise PublishError(f"符号链接逃离源码根：{destination.relative_to(root)}")
        tree_document = {"format": "mai.reference.tree.v1", "entries": manifest_entries}
        canonical = json.dumps(tree_document, ensure_ascii=False, sort_keys=True,
                               separators=(",", ":")).encode("utf-8")
        tree_digest = "sha256:" + hashlib.sha256(canonical).hexdigest()
        manifest = {
            "format": "mai.reference.source.v1",
            "provenance": {"kind": "git_tree", "commit_sha": commit, "tree_sha": tree},
            "manifest_digest": tree_digest,
            "file_count": len(entries), "total_bytes": total_bytes,
            "entries": manifest_entries,
            "note": "Git 原始对象快照；不含 .git/子模块下载/LFS 实体下载；摘要不是数字签名。",
        }
        with (staging / "source-manifest.json").open("x", encoding="utf-8") as handle:
            json.dump(manifest, handle, ensure_ascii=False, sort_keys=True, indent=2)
            handle.write("\n")
            handle.flush()
            os.fchmod(handle.fileno(), 0o444)
            os.fsync(handle.fileno())
        for directory, _, _ in os.walk(staging, topdown=False, followlinks=False):
            os.chmod(directory, 0o555)
            fsync_directory(Path(directory))
        atomic_publish_noreplace(staging, output)
        fsync_directory(output.parent)
        return {
            "source_path": str(output / "repo"), "commit_sha": commit,
            "manifest_digest": tree_digest, "file_count": len(entries),
            "total_bytes": total_bytes,
        }
    except BaseException:
        cleanup_staging(staging)
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True, help="本机完整 Git 仓库或 bare mirror")
    parser.add_argument("--revision", required=True, help="commit/tag；先解析成完整 commit SHA")
    parser.add_argument("--output", type=Path, required=True, help="不存在的最终版本目录，父目录需已存在")
    parser.add_argument("--allow-unprivileged", action="store_true", help="仅测试：允许非 root/临时目录发布")
    args = parser.parse_args()
    try:
        if not args.output.is_absolute() or ".." in args.output.parts:
            raise PublishError("output 必须是规范绝对路径")
        result = publish(args.repo, args.revision, args.output, args.allow_unprivileged)
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except (PublishError, OSError, ValueError, subprocess.SubprocessError) as exc:
        print(f"发布失败：{exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
