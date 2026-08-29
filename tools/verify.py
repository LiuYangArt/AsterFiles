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


def release_metadata() -> dict[str, object] | None:
    if not RELEASE.is_file():
        return None
    digest = hashlib.sha256(RELEASE.read_bytes()).hexdigest()
    modified = datetime.fromtimestamp(RELEASE.stat().st_mtime, timezone.utc).isoformat()
    return {"path": str(RELEASE), "modified_utc": modified, "sha256": digest}


def main() -> int:
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
        ("release", ["cargo", "build", "--release"]),
    ]
    results = []
    for name, command in steps:
        result = run_step(name, command)
        results.append(result)

    passed = len(results) == len(steps) and all(step["exit_code"] == 0 for step in results)
    release_passed = any(
        step["name"] == "release" and step["exit_code"] == 0 for step in results
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
        "release": release_metadata() if release_passed else None,
    }
    SUMMARY.write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    emit({"event": "validation_complete", "status": summary["status"], "summary": str(SUMMARY), "release": summary["release"]})
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
