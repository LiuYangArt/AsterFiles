"""Focused regression tests for the bounded build/cache lifecycle."""
import importlib.util
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SPEC = importlib.util.spec_from_file_location("build_policy", Path(__file__).with_name("build.py"))
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)


class BuildPolicyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        build.ROOT = root
        build.TARGET = root / "target"
        build.CACHE_ROOT = root / ".cache"
        build.STORE = build.CACHE_ROOT / "kache"
        build.EVIDENCE = root / "artifacts" / "verify"
        build.KACHE = build.CACHE_ROOT / "tools" / "kache.exe"
        build.KACHE_VERSION = "test"
        build.TARGET_LIMIT = 1024 * 1024
        build.TOTAL_LIMIT = 10 * 1024 * 1024
        self.idle_patch = mock.patch.object(build, "assert_idle")
        self.gc_patch = mock.patch.object(build, "gc")
        self.idle_patch.start()
        self.gc_patch.start()
        self.addCleanup(self.idle_patch.stop)
        self.addCleanup(self.gc_patch.stop)
        build.KACHE.parent.mkdir(parents=True)
        build.KACHE.write_bytes(b"test")
        for name in ("rust-toolchain.toml", "Cargo.lock", "Cargo.toml", ".kache.toml"):
            (root / name).write_text(name, encoding="utf-8")
        (root / ".cargo").mkdir()
        (root / ".cargo" / "config.toml").write_text("", encoding="utf-8")

    def tearDown(self):
        self.tmp.cleanup()

    def test_finish_budget_cleans_target_and_keeps_final_executable(self):
        exe = build.TARGET / "debug" / "asterfiles.exe"
        exe.parent.mkdir(parents=True)
        exe.write_bytes(b"runnable")
        (build.TARGET / "debug" / "old.o").write_bytes(b"x" * 10000)
        build.TARGET_LIMIT = 4096
        build.finish()
        self.assertEqual(exe.read_bytes(), b"runnable")
        self.assertFalse((build.TARGET / "debug" / "old.o").exists())

    def test_prepare_and_status_allow_missing_target(self):
        self.assertFalse(build.TARGET.exists())
        with mock.patch.object(build, "run", return_value=mock.Mock(stdout="kache test\n")):
            build.prepare()
        empty = build.measure(build.TARGET)
        self.assertEqual(empty["files"], 0)
        self.assertEqual(empty["allocated_bytes"], 0)
        build.report("status")
        self.assertTrue((build.EVIDENCE / "build-cache-status.json").is_file())

    def test_prepare_preserves_existing_outputs_on_first_and_repeated_runs(self):
        artifact = build.TARGET / "debug" / "existing.o"
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"reusable")
        with mock.patch.object(build, "run", return_value=mock.Mock(stdout="kache test\n")):
            for _ in range(2):
                build.prepare()
                self.assertEqual(artifact.read_bytes(), b"reusable")

    def test_prepare_leaves_input_invalidation_to_cargo(self):
        artifact = build.TARGET / "debug" / "existing.o"
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"reusable")
        inputs = ["rust-toolchain.toml", "Cargo.lock", "Cargo.toml",
                  ".cargo/config.toml", ".kache.toml"]
        member = build.ROOT / "crates" / "domain" / "Cargo.toml"
        with mock.patch.object(build, "run", return_value=mock.Mock(stdout="kache test\n")):
            build.prepare()
            for name in inputs:
                with self.subTest(changed=name):
                    (build.ROOT / name).write_text("changed", encoding="utf-8")
                    build.prepare()
                    self.assertEqual(artifact.read_bytes(), b"reusable")
            member.parent.mkdir(parents=True)
            for change in ("added", "changed", None):
                with self.subTest(member=change):
                    if change is None:
                        member.unlink()
                    else:
                        member.write_text(change, encoding="utf-8")
                    build.prepare()
                    self.assertEqual(artifact.read_bytes(), b"reusable")

    def test_prepare_over_budget_cleans_outputs_but_keeps_exe_and_pdb(self):
        profile = build.TARGET / "debug"
        profile.mkdir(parents=True)
        exe = profile / "asterfiles.exe"
        pdb = profile / "asterfiles.pdb"
        stale = profile / "stale.o"
        exe.write_bytes(b"runnable")
        pdb.write_bytes(b"symbols")
        stale.write_bytes(b"x" * 10000)
        build.TARGET_LIMIT = 8192
        with mock.patch.object(build, "run", return_value=mock.Mock(stdout="kache test\n")):
            build.prepare()
        self.assertFalse(stale.exists())
        self.assertEqual(exe.read_bytes(), b"runnable")
        self.assertEqual(pdb.read_bytes(), b"symbols")

    def test_explicit_clean_keeps_final_programs_and_symbols(self):
        for profile in ("debug", "release"):
            directory = build.TARGET / profile
            directory.mkdir(parents=True)
            (directory / "asterfiles.exe").write_bytes(b"runnable")
            (directory / "asterfiles.pdb").write_bytes(b"symbols")
            (directory / "stale.o").write_bytes(b"stale")
        self.assertEqual(build.main(["clean"]), 0)
        for profile in ("debug", "release"):
            directory = build.TARGET / profile
            self.assertEqual((directory / "asterfiles.exe").read_bytes(), b"runnable")
            self.assertEqual((directory / "asterfiles.pdb").read_bytes(), b"symbols")
            self.assertFalse((directory / "stale.o").exists())

    def test_safe_tree_accepts_short_names_that_resolve_inside_the_root(self):
        build.TARGET.mkdir()
        original = Path.resolve

        def expand(self, *args, **kwargs):
            root = os.path.normcase(os.path.normpath(str(build.ROOT)))
            text = os.path.normcase(os.path.normpath(str(self)))
            if text == root or text.startswith(root + os.sep):
                return Path("D:/normalized-root") / Path(text).relative_to(root)
            return original(self, *args, **kwargs)

        with mock.patch.object(Path, "resolve", expand):
            self.assertEqual(build.safe_tree(build.TARGET), build.TARGET.absolute())

    def test_redirected_file_and_directory_are_rejected(self):
        target = build.TARGET
        target.mkdir(parents=True)
        outside = Path(self.tmp.name) / "outside"
        outside.mkdir()
        try:
            (target / "link-dir").symlink_to(outside, target_is_directory=True)
        except (OSError, NotImplementedError):
            self.skipTest("symlink creation unavailable")
        with self.assertRaises(RuntimeError):
            build.measure(target)
        (target / "link-dir").unlink()
        (target / "link-file").symlink_to(outside / "file")
        with self.assertRaises(RuntimeError):
            build.measure(target)

    def test_measure_deduplicates_hardlinks(self):
        target = build.TARGET
        target.mkdir(parents=True)
        first = target / "a.bin"
        second = target / "b.bin"
        first.write_bytes(b"12345")
        try:
            os.link(first, second)
        except OSError:
            self.skipTest("hard links unavailable")
        result = build.measure(target)
        self.assertEqual(result["files"], 2)
        self.assertEqual(result["logical_bytes"], 10)
        self.assertEqual(result["unique_bytes"], 5)

    def test_failed_build_runs_finish_and_preserves_failure(self):
        calls = []
        failure = build.subprocess.CalledProcessError(7, ["cargo"])
        with mock.patch.object(build, "prepare", side_effect=lambda: calls.append("prepare")), mock.patch.object(build, "finish", side_effect=lambda: calls.append("finish")), mock.patch.object(build, "run", side_effect=failure):
            self.assertEqual(build.main(["build", "--locked"]), 1)
        self.assertEqual(calls, ["prepare", "finish"])
        output = io.StringIO()
        with mock.patch.object(build, "prepare"), mock.patch.object(build, "finish", side_effect=RuntimeError("cleanup failure")), mock.patch.object(build, "run", side_effect=failure), mock.patch("sys.stderr", output):
            self.assertEqual(build.main(["build", "--locked"]), 1)
        self.assertIn("cleanup failure", output.getvalue())
        self.assertIn("exit status 7", output.getvalue())


if __name__ == "__main__":
    unittest.main()
