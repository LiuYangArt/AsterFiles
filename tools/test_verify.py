#!/usr/bin/env python3
"""Regression tests for the validation orchestrator."""

from __future__ import annotations

import importlib.util
import json
import io
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).with_name("verify.py")
SPEC = importlib.util.spec_from_file_location("asterfiles_verify", MODULE_PATH)
assert SPEC and SPEC.loader
verify = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify)


class VerifyTests(unittest.TestCase):
    def test_modes_are_mutually_exclusive(self) -> None:
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            verify.parse_args(["--quick", "--release"])

    def test_validation_modes_build_debug_exactly_once(self) -> None:
        for quick in (True, False):
            steps = verify.validation_steps(quick=quick, include_release=False)
            self.assertEqual(sum(command == ["cargo", "build"] for _, command in steps), 1)

    def test_full_validation_reuses_debug_for_scenarios(self) -> None:
        steps = verify.validation_steps(quick=False, include_release=False)
        scenario_commands = [command for name, command in steps if name.startswith("agent-")]
        self.assertTrue(scenario_commands)
        self.assertTrue(all(command[0] == str(verify.DEBUG) for command in scenario_commands))
        self.assertTrue(all("cargo" not in command for command in scenario_commands))

    def test_fail_fast_skips_following_steps(self) -> None:
        calls = []

        def runner(name: str, command: list[str]) -> dict[str, object]:
            calls.append(name)
            return {
                "name": name,
                "command": command,
                "status": "failed" if name == "first" else "passed",
                "exit_code": 1 if name == "first" else 0,
            }

        with patch.object(verify, "emit"):
            results = verify.execute_steps(
                [("first", ["false"]), ("second", ["true"])],
                keep_going=False,
                runner=runner,
            )
        self.assertEqual(calls, ["first"])
        self.assertEqual(results[1]["status"], "skipped")

    def test_keep_going_runs_following_steps(self) -> None:
        calls = []

        def runner(name: str, command: list[str]) -> dict[str, object]:
            calls.append(name)
            return {
                "name": name,
                "command": command,
                "status": "failed" if name == "first" else "passed",
                "exit_code": 1 if name == "first" else 0,
            }

        verify.execute_steps(
            [("first", ["false"]), ("second", ["true"])],
            keep_going=True,
            runner=runner,
        )
        self.assertEqual(calls, ["first", "second"])

    def test_validation_reuse_requires_matching_fingerprint_and_debug_build(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stamp = root / "stamp.json"
            debug = root / "asterfiles.exe"
            debug.write_bytes(b"debug")
            stamp.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "status": "passed",
                        "fingerprint": "same",
                    }
                ),
                encoding="utf-8",
            )
            self.assertTrue(verify.reusable_full_validation(stamp, "same", debug))
            self.assertFalse(verify.reusable_full_validation(stamp, "changed", debug))
            debug.unlink()
            self.assertFalse(verify.reusable_full_validation(stamp, "same", debug))

    def test_reused_validation_points_to_preserved_full_summary(self) -> None:
        self.assertNotEqual(verify.SUMMARY, verify.FULL_DEBUG_SUMMARY)

    def test_process_filter_only_matches_repository_builds(self) -> None:
        self.assertTrue(verify.same_path(verify.DEBUG, verify.DEBUG))
        self.assertFalse(
            verify.same_path(
                Path(r"C:\OtherProject\target\debug\asterfiles.exe"), verify.DEBUG
            )
        )

    def test_stop_repository_processes_ignores_other_projects(self) -> None:
        with patch.object(
            verify,
            "process_rows",
            return_value=[(7, Path(r"C:\OtherProject\target\debug\asterfiles.exe"))],
        ), patch.object(verify, "run_capture") as run_capture:
            self.assertEqual(verify.stop_repository_processes(), [])
            run_capture.assert_not_called()

    def test_stop_repository_processes_stops_repository_debug_build(self) -> None:
        completed = verify.subprocess.CompletedProcess([], 0, "", "")
        with patch.object(
            verify, "process_rows", return_value=[(42, verify.DEBUG)]
        ), patch.object(verify, "run_capture", return_value=completed) as run_capture, patch.object(
            verify, "emit"
        ):
            self.assertEqual(verify.stop_repository_processes(), [42])
            self.assertIn("42", run_capture.call_args.args[0][-1])


if __name__ == "__main__":
    unittest.main()
