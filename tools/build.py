#!/usr/bin/env python3
"""Project build lifecycle and bounded local cache (#140)."""
from __future__ import annotations

import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TARGET = ROOT / "target"
CACHE_ROOT = ROOT / ".cache"
KACHE = CACHE_ROOT / "tools" / "kache.exe"
STORE = CACHE_ROOT / "kache"
EVIDENCE = ROOT / "artifacts" / "verify"
STAMP = CACHE_ROOT / "build-inputs.json"
KACHE_VERSION = "0.27.0"
KACHE_SHA256 = "bda91971a843b386d9862679f73e21840acb2736264bd636e293b4a886c1f476"
TARGET_LIMIT = 8 * 1024**3
TOTAL_LIMIT = 13 * 1024**3


def environment() -> dict[str, str]:
    env = os.environ.copy()
    env.update(KACHE_CONFIG=str(ROOT / ".kache.toml"), KACHE_CACHE_DIR=str(STORE),
               KACHE_RUNTIME_DIR=str(CACHE_ROOT / "runtime"), CARGO_INCREMENTAL="0")
    return env


def run(command: list[str], capture: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(command, cwd=ROOT, env=environment(), check=True,
                          text=True, encoding="utf-8", errors="replace",
                          stdout=subprocess.PIPE if capture else None)


def safe_tree(path: Path) -> Path:
    absolute = path.absolute()
    if absolute not in (TARGET, STORE, CACHE_ROOT / "runtime"):
        raise RuntimeError(f"Not a managed build directory: {path}")
    # Compare expanded paths. GitHub runners store the profile as RUNNER~1.
    if not absolute.resolve().is_relative_to(ROOT.resolve()) or absolute.is_symlink() or absolute.is_junction():
        raise RuntimeError(f"Refusing redirected build directory: {path}")
    for directory, children, _ in os.walk(absolute):
        for name in children:
            child = Path(directory) / name
            if child.is_symlink() or child.is_junction():
                raise RuntimeError(f"Refusing redirected child: {child}")
    return absolute


def assert_idle() -> None:
    if os.name == "nt":
        result = run(["pwsh", "-NoProfile", "-Command",
                      "@(Get-Process cargo,rustc -ErrorAction SilentlyContinue).Count"], True)
        if int(result.stdout.strip()):
            raise RuntimeError("Cargo/rustc is running; no cache maintenance was performed.")


def measure(path: Path) -> dict[str, int]:
    result = {"files": 0, "logical_bytes": 0, "unique_bytes": 0, "allocated_bytes": 0}
    seen = set()
    disk_size = None
    if os.name == "nt":
        import msvcrt
        class FileStandardInfo(ctypes.Structure):
            _fields_ = [("allocation", ctypes.c_longlong), ("end", ctypes.c_longlong),
                        ("links", ctypes.c_ulong), ("delete_pending", ctypes.c_ubyte),
                        ("directory", ctypes.c_ubyte)]
        disk_size = ctypes.WinDLL("kernel32", use_last_error=True).GetFileInformationByHandleEx
        disk_size.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p, ctypes.c_ulong]
        disk_size.restype = ctypes.c_int
    if path.is_symlink() or path.is_junction():
        raise RuntimeError(f"Cannot measure redirected root: {path}")
    # Fresh CI checkouts and first local runs have no target yet.
    if not path.exists():
        return result
    for directory, children, files in os.walk(path, onerror=lambda error: (_ for _ in ()).throw(error)):
        for child in children:
            candidate = Path(directory) / child
            if candidate.is_symlink() or candidate.is_junction():
                raise RuntimeError(f"Cannot measure redirected directory: {candidate}")
        for name in files:
            file = Path(directory) / name
            if file.is_symlink() or file.is_junction():
                raise RuntimeError(f"Cannot measure redirected file: {file}")
            stat = file.stat()
            result["files"] += 1
            result["logical_bytes"] += stat.st_size
            identity = (stat.st_dev, stat.st_ino)
            if identity in seen:
                continue
            seen.add(identity)
            result["unique_bytes"] += stat.st_size
            if disk_size:
                info = FileStandardInfo()
                with file.open("rb") as stream:
                    handle = msvcrt.get_osfhandle(stream.fileno())
                    if not disk_size(handle, 1, ctypes.byref(info), ctypes.sizeof(info)):
                        raise ctypes.WinError(ctypes.get_last_error())
                allocated = info.allocation
            else:
                allocated = stat.st_blocks * 512
            result["allocated_bytes"] += allocated
    return result


def report(name: str) -> dict:
    result = {"issue": 140, "target": measure(TARGET), "cache": measure(CACHE_ROOT),
              "target_limit_bytes": TARGET_LIMIT, "total_limit_bytes": TOTAL_LIMIT}
    result["total_allocated_bytes"] = result["target"]["allocated_bytes"] + result["cache"]["allocated_bytes"]
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    (EVIDENCE / f"build-cache-{name}.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"event": "build_storage", "phase": name, **result}), flush=True)
    return result


def clean_target(preserve_outputs: bool = True) -> None:
    target = safe_tree(TARGET)
    assert_idle()
    # Keep the last runnable app even when dependencies are retired under pressure.
    with tempfile.TemporaryDirectory(prefix="asterfiles-build-") as directory:
        staging = Path(directory)
        outputs = []
        if preserve_outputs:
            for profile in ("debug", "release"):
                for suffix in ("exe", "pdb"):
                    source = target / profile / f"asterfiles.{suffix}"
                    if source.is_file():
                        relative = source.relative_to(target)
                        saved = staging / relative
                        saved.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(source, saved)
                        outputs.append(relative)
        if target.exists():
            shutil.rmtree(target)
        for relative in outputs:
            destination = target / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(staging / relative, destination)


def build_inputs() -> dict[str, str]:
    return {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
            for name in ("rust-toolchain.toml", "Cargo.lock", "Cargo.toml", ".cargo/config.toml", ".kache.toml")}


def gc() -> None:
    if KACHE.is_file():
        # Explicit GC starts a daemon in kache 0.27; retire our private runtime afterwards.
        try:
            output = run([str(KACHE), "gc", "--json"], True).stdout
            EVIDENCE.mkdir(parents=True, exist_ok=True)
            (EVIDENCE / "kache-gc.json").write_text(output, encoding="utf-8")
        finally:
            run([str(KACHE), "daemon", "stop"], True)


def prepare() -> None:
    assert_idle()
    if not KACHE.is_file():
        raise RuntimeError("Run python tools/build.py setup before building.")
    version = run([str(KACHE), "--version"], True).stdout.strip()
    if version != f"kache {KACHE_VERSION}":
        raise RuntimeError(f"Unexpected cache tool: {version}; run setup.")
    if os.environ.get("CARGO_TARGET_DIR") or os.environ.get("RUSTC_WRAPPER"):
        raise RuntimeError("Remove CARGO_TARGET_DIR/RUSTC_WRAPPER overrides to use the repository build policy.")
    before = report("before")
    current = build_inputs()
    previous = json.loads(STAMP.read_text(encoding="utf-8")) if STAMP.exists() else None
    if previous != current or before["target"]["allocated_bytes"] > TARGET_LIMIT:
        print("Retiring obsolete build outputs; reusable compiler cache is retained.", flush=True)
        clean_target()
    gc()
    CACHE_ROOT.mkdir(parents=True, exist_ok=True)
    STAMP.write_text(json.dumps(current, indent=2) + "\n", encoding="utf-8")


def finish() -> None:
    assert_idle()
    gc()
    usage = report("after-build")
    if usage["target"]["allocated_bytes"] > TARGET_LIMIT:
        clean_target()
        usage = report("after-target-clean")
    if usage["total_allocated_bytes"] > TOTAL_LIMIT:
        # kache's blob budget excludes index/log/runtime bytes; enforce the aggregate too.
        store = safe_tree(STORE)
        if store.exists():
            shutil.rmtree(store)
        runtime = safe_tree(CACHE_ROOT / "runtime")
        if runtime.exists():
            shutil.rmtree(runtime)
        usage = report("after-cache-clean")
    if usage["total_allocated_bytes"] > TOTAL_LIMIT:
        raise RuntimeError("Build storage remains above the aggregate budget; inspect build-cache evidence.")
    report("final")


def setup() -> None:
    KACHE.parent.mkdir(parents=True, exist_ok=True)
    if not KACHE.is_file() or hashlib.sha256(KACHE.read_bytes()).hexdigest() != KACHE_SHA256:
        url = f"https://github.com/kunobi-ninja/kache/releases/download/v{KACHE_VERSION}/kache-x86_64-pc-windows-msvc.exe"
        temporary = KACHE.with_suffix(".download")
        try:
            urllib.request.urlretrieve(url, temporary)
            if hashlib.sha256(temporary.read_bytes()).hexdigest() != KACHE_SHA256:
                raise RuntimeError("kache download checksum mismatch")
            temporary.replace(KACHE)
        finally:
            temporary.unlink(missing_ok=True)
    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]
    run(["rustup", "toolchain", "install", toolchain["channel"], "--profile", "minimal",
         "--component", ",".join(toolchain["components"])])
    run([str(KACHE), "--version"])


def main(arguments: list[str] | None = None) -> int:
    args = sys.argv[1:] if arguments is None else arguments
    operation = args[0] if args else "build"
    try:
        if operation == "setup":
            setup()
        elif operation == "status":
            report("status")
        elif operation == "prepare":
            prepare()
        elif operation == "finish":
            finish()
        elif operation == "clean":
            report("before-clean")
            clean_target()
            finish()
        elif operation in {"build", "test", "check", "clippy", "run"}:
            prepare()
            try:
                run(["cargo", *(args or ["build", "--locked"])])
            except (OSError, subprocess.CalledProcessError):
                try:
                    finish()
                except Exception as cleanup_error:
                    print(f"Build cleanup also failed: {cleanup_error}", file=sys.stderr)
                raise
            else:
                finish()
        else:
            raise RuntimeError("Use setup/status/clean or a Cargo build/test/check/clippy/run command.")
        return 0
    except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError) as error:
        print(f"Build policy failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())