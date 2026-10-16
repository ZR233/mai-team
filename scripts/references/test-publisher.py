#!/usr/bin/env python3
"""发布器的离线合成 Git fixture 测试；不访问网络，不测试 mai-team 或 Docker。"""
from pathlib import Path
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("publish-reference-source.py")


class PublisherTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="mai-ref-publisher-test-"))
        self.repo = self.temp / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Local Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        (self.repo / "plain.c").write_bytes(b"int fixture(void) { return 1; }\n")
        self.commit()

    def tearDown(self):
        for directory, _, _ in os.walk(self.temp, followlinks=False):
            os.chmod(directory, 0o700)
        shutil.rmtree(self.temp)

    def git(self, *args):
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
        result = subprocess.run(["git", "-C", str(self.repo), *args], env=env,
                                capture_output=True, check=True)
        return result.stdout

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD").decode().strip()

    def export(self, name="out", revision="HEAD"):
        return subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo),
                               "--revision", revision, "--output", str(self.temp / name),
                               "--allow-unprivileged"], capture_output=True, text=True)

    def good(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def rejected(self, result, name="out"):
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.temp / name).exists())
        self.assertFalse(list(self.temp.glob(f".{name}.publishing-*")))

    def test_committed_bytes_only_and_no_git(self):
        expected = (self.repo / "plain.c").read_bytes()
        (self.repo / "plain.c").write_text("dirty content\n")
        (self.repo / "untracked.txt").write_text("not committed\n")
        self.good(self.export())
        root = self.temp / "out" / "repo"
        self.assertEqual((root / "plain.c").read_bytes(), expected)
        self.assertFalse((root / "untracked.txt").exists())
        self.assertFalse((root / ".git").exists())

    def test_archive_attributes_do_not_drop_or_substitute(self):
        (self.repo / ".gitattributes").write_text("plain.c export-ignore\nsubst.txt export-subst\n")
        text = "$Format:%H$\n"
        (self.repo / "subst.txt").write_text(text)
        self.commit()
        self.good(self.export())
        root = self.temp / "out" / "repo"
        self.assertTrue((root / "plain.c").exists())
        self.assertEqual((root / "subst.txt").read_text(), text)

    def test_binary_unicode_and_executable_modes(self):
        (self.repo / "字节.bin").write_bytes(b"\x00\xff\xfe\n")
        executable = self.repo / "script.sh"
        executable.write_text("#!/bin/sh\necho fixture\n")
        executable.chmod(0o755)
        self.commit()
        self.good(self.export())
        root = self.temp / "out" / "repo"
        self.assertEqual((root / "字节.bin").read_bytes(), b"\x00\xff\xfe\n")
        self.assertEqual(stat.S_IMODE((root / "plain.c").stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE((root / "script.sh").stat().st_mode), 0o555)
        self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o555)

    def test_internal_symlink(self):
        (self.repo / "alias.c").symlink_to("plain.c")
        self.commit()
        self.good(self.export())
        link = self.temp / "out" / "repo" / "alias.c"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.read_bytes(), (self.repo / "plain.c").read_bytes())

    def test_absolute_symlink_rejected(self):
        (self.repo / "external").symlink_to("/etc/passwd")
        self.commit()
        self.rejected(self.export())

    def test_relative_escape_rejected(self):
        (self.repo / "external").symlink_to("../../../repo/plain.c")
        self.commit()
        self.rejected(self.export())

    def test_dangling_symlink_rejected(self):
        (self.repo / "dangling").symlink_to("does-not-exist")
        self.commit()
        self.rejected(self.export())

    def test_symlink_cycle_rejected(self):
        (self.repo / "a").symlink_to("b")
        (self.repo / "b").symlink_to("a")
        self.commit()
        self.rejected(self.export())

    def test_gitlink_rejected(self):
        head = self.git("rev-parse", "HEAD").decode().strip()
        self.git("update-index", "--add", "--cacheinfo", f"160000,{head},submodule")
        self.git("commit", "-qm", "gitlink fixture")
        self.rejected(self.export())

    def test_existing_output_not_overwritten(self):
        self.good(self.export())
        manifest = self.temp / "out" / "source-manifest.json"
        before = manifest.read_bytes()
        result = self.export()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(manifest.read_bytes(), before)

    def test_reproducible_manifest_and_file_digest(self):
        first = self.good(self.export("out1"))
        second = self.good(self.export("out2"))
        self.assertEqual(first["manifest_digest"], second["manifest_digest"])
        manifest = json.loads((self.temp / "out1" / "source-manifest.json").read_text())
        for entry in manifest["entries"]:
            self.assertEqual(entry["sha256"], hashlib.sha256((self.repo / entry["path"]).read_bytes()).hexdigest())
        self.assertEqual((self.temp / "out1" / "source-manifest.json").read_bytes(),
                         (self.temp / "out2" / "source-manifest.json").read_bytes())

    def test_fixed_old_commit(self):
        old = self.git("rev-parse", "HEAD").decode().strip()
        expected = (self.repo / "plain.c").read_bytes()
        (self.repo / "plain.c").write_text("int newer;\n")
        self.commit()
        result = self.good(self.export(revision=old))
        self.assertEqual(result["commit_sha"], old)
        self.assertEqual((self.temp / "out" / "repo" / "plain.c").read_bytes(), expected)

    def test_lfs_pointer_is_exported_as_committed_bytes(self):
        pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:" + b"a" * 64 + b"\nsize 1048576\n"
        (self.repo / "asset.bin").write_bytes(pointer)
        self.commit()
        self.good(self.export())
        self.assertEqual((self.temp / "out/repo/asset.bin").read_bytes(), pointer)


if __name__ == "__main__":
    unittest.main(verbosity=2)
