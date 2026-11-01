#!/usr/bin/env python3
"""Check the workspace dependency boundaries and export evidence (#166)."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "artifacts" / "state" / "issue-166" / "workspace.json"


def inspect(metadata: dict) -> dict:
    packages = {package["id"]: package for package in metadata["packages"]}
    members = {packages[key]["name"]: key for key in metadata["workspace_members"]}
    graph = {
        node["id"]: sorted({
            dependency["pkg"] for dependency in node["deps"]
            if any(kind["kind"] is None for kind in dependency["dep_kinds"])
        })
        for node in metadata["resolve"]["nodes"]
    }

    def names(ids):
        return {packages[key]["name"] for key in ids}

    def reachable(package):
        visited = set()
        pending = list(graph[members[package]])
        while pending:
            dependency = pending.pop()
            if dependency not in visited:
                visited.add(dependency)
                pending.extend(graph[dependency])
        return names(visited)

    violations = []
    for package in ("asterfiles", "asterfiles-domain", "asterfiles-platform-windows",
                    "asterfiles-webdav", "asterfiles-ui"):
        if package not in members:
            violations.append(f"Missing workspace member: {package}")
    if "asterfiles-domain" in members:
        forbidden = reachable("asterfiles-domain") & {"windows", "windows-sys", "slint"}
        if forbidden:
            violations.append(f"Domain depends on forbidden packages: {sorted(forbidden)}")
    if "asterfiles" in members:
        forbidden = names(graph[members["asterfiles"]]) & {"windows", "windows-sys", "windows-core"}
        if forbidden:
            violations.append(f"Application directly depends on Windows bindings: {sorted(forbidden)}")
    if "asterfiles-ui" in members:
        forbidden = reachable("asterfiles-ui") & {
            "asterfiles-domain", "asterfiles-platform-windows", "asterfiles-webdav"
        }
        if forbidden:
            violations.append(f"UI depends on application services: {sorted(forbidden)}")
    return {
        "issue": 166,
        "status": "failed" if violations else "passed",
        "violations": violations,
        "workspace_members": sorted(members),
        "packages": {key: {"name": value["name"], "version": value["version"]}
                     for key, value in sorted(packages.items())},
        "normal_dependencies": dict(sorted(graph.items())),
    }


def main() -> int:
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT,
        stdout=subprocess.PIPE, text=True, encoding="utf-8", check=True,
    )
    report = inspect(json.loads(result.stdout))
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"event": "workspace_architecture", "status": report["status"],
                      "artifact": str(OUTPUT), "violations": report["violations"]}), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
