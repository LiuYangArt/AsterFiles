#!/usr/bin/env python3
"""Run AsterFiles validation and write machine-readable artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Sequence


ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / "artifacts"
VERIFY_DIR = ARTIFACTS / "verify"
LOG_DIR = ARTIFACTS / "logs"
STATE_DIR = ARTIFACTS / "state"
SUMMARY = VERIFY_DIR / "summary.json"
FULL_DEBUG_SUMMARY = VERIFY_DIR / "full-debug-summary.json"
DEBUG = ROOT / "target" / "debug" / "asterfiles.exe"
RELEASE = ROOT / "target" / "release" / "asterfiles.exe"
VALIDATION_STAMP = VERIFY_DIR / "full-debug-success.json"

SCENARIOS = [
    ("agent-state", "permission-denied", STATE_DIR / "permission-denied.json"),
    (
        "agent-file-operation-running",
        "file-operation-running",
        STATE_DIR / "file-operations" / "running.json",
    ),
    (
        "agent-file-operation-conflict",
        "file-operation-conflict",
        STATE_DIR / "file-operations" / "conflict.json",
    ),
    (
        "agent-file-operation-partial",
        "file-operation-partial",
        STATE_DIR / "file-operations" / "partial.json",
    ),
    (
        "agent-drag-drop-foundation",
        "drag-drop-foundation",
        STATE_DIR / "drag-drop" / "foundation.json",
    ),
    (
        "agent-multi-window-state-layering",
        "multi-window-state-layering",
        STATE_DIR / "multi-window" / "state-layering.json",
    ),
    ("agent-tab-reorder", "tab-reorder", STATE_DIR / "tab-reorder" / "state.json"),
    ("agent-tab-detach", "tab-detach", STATE_DIR / "tab-detach" / "state.json"),
    (
        "agent-tab-cross-window",
        "tab-cross-window",
        STATE_DIR / "tab-cross-window" / "state.json",
    ),
    (
        "agent-explorer-pins",
        "explorer-pins",
        STATE_DIR / "explorer-pins" / "shell.json",
    ),
    (
        "agent-shell-thumbnail",
        "shell-thumbnail",
        STATE_DIR / "thumbnails" / "shell-png.json",
    ),
    (
        "agent-quick-menu-search",
        "quick-menu-search",
        STATE_DIR / "context-menu" / "search.json",
    ),
    (
        "agent-quick-menu-popup",
        "quick-menu-popup",
        STATE_DIR / "context-menu" / "popup.json",
    ),
    (
        "agent-folder-size-scheduler",
        "folder-size-scheduler",
        STATE_DIR / "folder-size" / "scheduler.json",
    ),
    (
        "agent-network-foundation",
        "network-foundation",
        STATE_DIR / "network" / "foundation.json",
    ),
    (
        "agent-quick-access",
        "quick-access",
        STATE_DIR / "quick-access" / "state.json",
    ),
    (
        "agent-file-list-type-select",
        "file-list-type-select",
        STATE_DIR / "file-list" / "type-select.json",
    ),
]


def emit(event: dict[str, object]) -> None:
    print(json.dumps(event, ensure_ascii=False), flush=True)


def parse_args(arguments: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run fail-fast AsterFiles validation with reusable build artifacts."
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--quick",
        action="store_true",
        help="Run format, clippy, tests, and a Debug build without agent scenarios.",
    )
    mode.add_argument(
        "--release",
        action="store_true",
        help="Run or reuse full Debug validation, then build Release.",
    )
    parser.add_argument(
        "--keep-going",
        action="store_true",
        help="Continue independent checks after failures for diagnostic collection.",
    )
    parser.add_argument(
        "--no-reuse",
        action="store_true",
        help="Do not reuse a successful full Debug validation for Release.",
    )
    parser.add_argument(
        "--skip-process-check",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    return parser.parse_args(arguments)


def run_capture(command: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )


def worktree_fingerprint() -> str:
    files = run_capture(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"]
    )
    if files.returncode != 0:
        raise RuntimeError(files.stderr.strip() or "unable to list repository files")
    digest = hashlib.sha256()
    for relative_path in filter(None, files.stdout.split("\0")):
        candidate = ROOT / relative_path
        digest.update(relative_path.encode("utf-8"))
        if candidate.is_file():
            digest.update(candidate.read_bytes())
        else:
            digest.update(b"<missing>")
    return digest.hexdigest()


def reusable_full_validation(
    stamp_path: Path, fingerprint: str, debug_path: Path
) -> bool:
    try:
        stamp = json.loads(stamp_path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return False
    return (
        stamp.get("schema_version") == 1
        and stamp.get("status") == "passed"
        and stamp.get("fingerprint") == fingerprint
        and debug_path.is_file()
    )


def process_rows() -> list[tuple[int, Path]]:
    command = [
        "pwsh",
        "-NoLogo",
        "-NoProfile",
        "-Command",
        (
            "Get-CimInstance Win32_Process -Filter \"Name = 'asterfiles.exe'\" | "
            "ForEach-Object { if ($_.ExecutablePath) { "
            "\"$($_.ProcessId)`t$($_.ExecutablePath)\" } }"
        ),
    ]
    result = run_capture(command)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "unable to inspect AsterFiles processes")
    rows = []
    for line in result.stdout.splitlines():
        process_id, separator, executable = line.partition("\t")
        if separator:
            rows.append((int(process_id), Path(executable)))
    return rows


def same_path(left: Path, right: Path) -> bool:
    return str(left.resolve(strict=False)).casefold() == str(right.resolve(strict=False)).casefold()


def stop_repository_processes() -> list[int]:
    targets = {DEBUG.resolve(strict=False), RELEASE.resolve(strict=False)}
    stopped = []
    for process_id, executable in process_rows():
        if not any(same_path(executable, target) for target in targets):
            continue
        result = run_capture(
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-Command",
                f"Stop-Process -Id {process_id} -Force -ErrorAction Stop",
            ]
        )
        if result.returncode != 0:
            raise RuntimeError(
                result.stderr.strip() or f"unable to stop AsterFiles process {process_id}"
            )
        stopped.append(process_id)
    if stopped:
        emit({"event": "repository_processes_stopped", "process_ids": stopped})
    return stopped


def run_step(name: str, command: list[str]) -> dict[str, object]:
    started = time.monotonic()
    log_path = LOG_DIR / f"verify-{name}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("w", encoding="utf-8", newline="") as log:
        process = subprocess.run(
            command,
            cwd=ROOT,
            stdout=log,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
    result = {
        "name": name,
        "command": command,
        "exit_code": process.returncode,
        "duration_seconds": round(time.monotonic() - started, 3),
        "log": str(log_path),
        "status": "passed" if process.returncode == 0 else "failed",
    }
    emit({"event": "validation_step", **result})
    return result


def skipped_step(name: str, command: list[str], failed_step: str) -> dict[str, object]:
    result = {
        "name": name,
        "command": command,
        "exit_code": None,
        "duration_seconds": 0,
        "log": None,
        "status": "skipped",
        "reason": f"stopped after {failed_step}",
    }
    emit({"event": "validation_step", **result})
    return result


def build_metadata(path: Path) -> dict[str, object] | None:
    if not path.is_file():
        return None
    modified = datetime.fromtimestamp(path.stat().st_mtime, timezone.utc).isoformat()
    return {"path": str(path), "modified_utc": modified}


def scenario_steps() -> list[tuple[str, list[str]]]:
    return [
        (
            name,
            [
                str(DEBUG),
                "--agent-scenario",
                scenario,
                "--no-ui",
                "--agent-state-out",
                str(output),
            ],
        )
        for name, scenario, output in SCENARIOS
    ]


def validation_steps(quick: bool, include_release: bool) -> list[tuple[str, list[str]]]:
    steps = [
        ("verify-tests", [sys.executable, str(ROOT / "tools" / "test_verify.py")]),
        ("format", ["cargo", "fmt", "--check"]),
        (
            "clippy",
            ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
        ),
        ("test", ["cargo", "test"]),
    ]
    steps.append(("debug", ["cargo", "build"]))
    if not quick:
        steps.extend(scenario_steps())
    if include_release:
        steps.append(("release", ["cargo", "build", "--release"]))
    return steps


def execute_steps(
    steps: list[tuple[str, list[str]]],
    keep_going: bool,
    runner: Callable[[str, list[str]], dict[str, object]] = run_step,
) -> list[dict[str, object]]:
    results = []
    failed_step = None
    for name, command in steps:
        if failed_step and not keep_going:
            results.append(skipped_step(name, command, failed_step))
            continue
        result = runner(name, command)
        results.append(result)
        if result["status"] == "failed" and failed_step is None:
            failed_step = name
    return results


def write_summary(
    results: list[dict[str, object]],
    mode: str,
    reused_full_validation: bool,
) -> dict[str, object]:
    passed = all(step["status"] in {"passed", "reused"} for step in results)
    debug_passed = any(step["name"] == "debug" and step["status"] == "passed" for step in results)
    release_passed = any(
        step["name"] == "release" and step["status"] == "passed" for step in results
    )
    total_duration = round(
        sum(float(step["duration_seconds"]) for step in results), 3
    )
    summary = {
        "schema_version": 2,
        "status": "passed" if passed else "failed",
        "duration_seconds": total_duration,
        "generated_utc": datetime.now(timezone.utc).isoformat(),
        "repository": str(ROOT),
        "mode": mode,
        "reused_full_validation": reused_full_validation,
        "artifacts": {
            "root": str(ARTIFACTS),
            "logs": str(LOG_DIR),
            "state": str(STATE_DIR),
        },
        "steps": results,
        "debug": build_metadata(DEBUG) if debug_passed or reused_full_validation else None,
        "release": build_metadata(RELEASE) if release_passed else None,
    }
    SUMMARY.write_text(
        json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    emit(
        {
            "event": "validation_complete",
            "status": summary["status"],
            "summary": str(SUMMARY),
            "mode": mode,
            "reused_full_validation": reused_full_validation,
            "duration_seconds": total_duration,
        }
    )
    return summary


def main(arguments: Sequence[str] | None = None) -> int:
    args = parse_args(arguments)
    for directory in (VERIFY_DIR, LOG_DIR, STATE_DIR):
        directory.mkdir(parents=True, exist_ok=True)

    if not args.skip_process_check:
        try:
            stop_repository_processes()
        except RuntimeError as error:
            emit({"event": "validation_setup_failed", "error": str(error)})
            return 1

    fingerprint = worktree_fingerprint()
    reuse = (
        args.release
        and not args.no_reuse
        and reusable_full_validation(VALIDATION_STAMP, fingerprint, DEBUG)
    )
    mode = "release" if args.release else "quick" if args.quick else "full-debug"
    if reuse:
        reused = {
            "name": "full-debug",
            "command": ["python", "tools/verify.py"],
            "exit_code": 0,
            "duration_seconds": 0,
            "log": str(FULL_DEBUG_SUMMARY),
            "status": "reused",
        }
        emit({"event": "validation_step", **reused})
        results = [reused]
        results.extend(execute_steps([("release", ["cargo", "build", "--release"])], False))
    else:
        results = execute_steps(
            validation_steps(args.quick, args.release),
            args.keep_going,
        )

    summary = write_summary(results, mode, reuse)
    if summary["status"] == "passed" and not args.quick and not args.release:
        FULL_DEBUG_SUMMARY.write_text(
            json.dumps(summary, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        VALIDATION_STAMP.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "status": "passed",
                    "fingerprint": fingerprint,
                    "generated_utc": summary["generated_utc"],
                },
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
