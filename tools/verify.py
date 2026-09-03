#!/usr/bin/env python3
"""Run AsterFiles validation and write machine-readable artifacts."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / "artifacts"
VERIFY_DIR = ARTIFACTS / "verify"
LOG_DIR = ARTIFACTS / "logs"
STATE_DIR = ARTIFACTS / "state"
SUMMARY = VERIFY_DIR / "summary.json"
DEBUG = ROOT / "target" / "debug" / "asterfiles.exe"
RELEASE = ROOT / "target" / "release" / "asterfiles.exe"


def emit(event: dict[str, object]) -> None:
    print(json.dumps(event, ensure_ascii=False), flush=True)


def run_step(name: str, command: list[str]) -> dict[str, object]:
    started = time.monotonic()
    log_path = LOG_DIR / f"verify-{name}.log"
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


def build_metadata(path: Path) -> dict[str, object] | None:
    if not path.is_file():
        return None
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    modified = datetime.fromtimestamp(path.stat().st_mtime, timezone.utc).isoformat()
    return {"path": str(path), "modified_utc": modified, "sha256": digest}


def main() -> int:
    release_requested = "--release" in sys.argv[1:]
    unknown = [argument for argument in sys.argv[1:] if argument != "--release"]
    if unknown:
        raise SystemExit(f"unknown argument: {unknown[0]}")
    for directory in (VERIFY_DIR, LOG_DIR, STATE_DIR):
        directory.mkdir(parents=True, exist_ok=True)

    steps = [
        ("format", ["cargo", "fmt", "--check"]),
        (
            "clippy",
            ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
        ),
        ("test", ["cargo", "test"]),
        ("agent-state", [
            "cargo", "run", "--", "--agent-scenario", "permission-denied",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "permission-denied.json"),
        ]),
        ("agent-file-operation-running", [
            "cargo", "run", "--", "--agent-scenario", "file-operation-running",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "file-operations" / "running.json"),
        ]),
        ("agent-file-operation-conflict", [
            "cargo", "run", "--", "--agent-scenario", "file-operation-conflict",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "file-operations" / "conflict.json"),
        ]),
        ("agent-file-operation-partial", [
            "cargo", "run", "--", "--agent-scenario", "file-operation-partial",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "file-operations" / "partial.json"),
        ]),
        ("agent-drag-drop-foundation", [
            "cargo", "run", "--", "--agent-scenario", "drag-drop-foundation",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "drag-drop" / "foundation.json"),
        ]),
        ("agent-multi-window-state-layering", [
            "cargo", "run", "--", "--agent-scenario", "multi-window-state-layering",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "multi-window" / "state-layering.json"),
        ]),
        ("agent-tab-reorder", [
            "cargo", "run", "--", "--agent-scenario", "tab-reorder",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "tab-reorder" / "state.json"),
        ]),
        ("agent-tab-detach", [
            "cargo", "run", "--", "--agent-scenario", "tab-detach",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "tab-detach" / "state.json"),
        ]),
        ("agent-tab-cross-window", [
            "cargo", "run", "--", "--agent-scenario", "tab-cross-window",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "tab-cross-window" / "state.json"),
        ]),
        ("agent-explorer-pins", [
            "cargo", "run", "--", "--agent-scenario", "explorer-pins",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "explorer-pins" / "shell.json"),
        ]),
        ("agent-shell-thumbnail", [
            "cargo", "run", "--", "--agent-scenario", "shell-thumbnail",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "thumbnails" / "shell-png.json"),
        ]),
        ("agent-quick-menu-search", [
            "cargo", "run", "--", "--agent-scenario", "quick-menu-search",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "context-menu" / "search.json"),
        ]),
        ("agent-quick-menu-popup", [
            "cargo", "run", "--", "--agent-scenario", "quick-menu-popup",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "context-menu" / "popup.json"),
        ]),
        ("agent-folder-size-scheduler", [
            "cargo", "run", "--", "--agent-scenario", "folder-size-scheduler",
            "--no-ui", "--agent-state-out", str(STATE_DIR / "folder-size" / "scheduler.json"),
        ]),
        ("release", ["cargo", "build", "--release"])
        if release_requested
        else ("debug", ["cargo", "build"]),
    ]
    results = []
    for name, command in steps:
        result = run_step(name, command)
        results.append(result)

    passed = len(results) == len(steps) and all(step["exit_code"] == 0 for step in results)
    build_name = "release" if release_requested else "debug"
    build_passed = any(
        step["name"] == build_name and step["exit_code"] == 0 for step in results
    )
    summary = {
        "schema_version": 1,
        "status": "passed" if passed else "failed",
        "generated_utc": datetime.now(timezone.utc).isoformat(),
        "repository": str(ROOT),
        "artifacts": {
            "root": str(ARTIFACTS),
            "logs": str(LOG_DIR),
            "state": str(STATE_DIR),
        },
        "steps": results,
        "build_profile": build_name,
        "debug": build_metadata(DEBUG) if build_passed and not release_requested else None,
        "release": build_metadata(RELEASE) if build_passed and release_requested else None,
    }
    SUMMARY.write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    emit({
        "event": "validation_complete",
        "status": summary["status"],
        "summary": str(SUMMARY),
        "build_profile": build_name,
        "artifact": summary[build_name],
    })
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
