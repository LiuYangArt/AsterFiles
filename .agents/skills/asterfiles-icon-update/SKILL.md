---
name: asterfiles-icon-update
description: Update AsterFiles application icons from assets/app-icon.png, including the Slint window/taskbar image and the multi-size PNG-encoded Windows ICO embedded in the Debug executable. Use when the project app icon PNG changes or the Windows EXE icon must be refreshed.
---

# AsterFiles Icon Update

Use `assets/app-icon.png` as the single source of truth. The Slint window and taskbar already reference it directly; do not copy it elsewhere or change those bindings unless the project structure has actually changed.

## Update

From the repository root, run:

```powershell
python .agents/skills/asterfiles-icon-update/scripts/update_icon.py
cargo build
```

The script must regenerate `assets/windows/asterfiles.ico` with these exact sizes: 16, 20, 24, 32, 40, 48, 64, 96, 128, and 256 pixels. Every ICO entry remains a 32-bit PNG to avoid Windows BMP transparency differences. It also checks that the source is square, at least 256 px, has an alpha channel, and has transparent corners.

Do not run a Release build unless the user explicitly requests it. Do not edit `assets/app-icon.png`; it is user-provided input.

## Verify and report

Treat success as all of the following:

- the update script exits successfully and reports all 10 PNG entries with transparent corners;
- `cargo build` succeeds, proving the ICO was embedded through `build.rs` and `assets/windows/asterfiles.rc`;
- `target/debug/asterfiles.exe` has a fresh modification time;
- report its SHA-256 using `Get-FileHash -Algorithm SHA256 target/debug/asterfiles.exe`.

If the desktop shortcut still shows the old image after these checks, identify Windows icon caching as a display issue and recommend recreating the shortcut. Do not clear system-wide icon caches without explicit permission.

Preserve unrelated working-tree changes. The normal expected tracked changes are the user-edited `assets/app-icon.png` and the generated `assets/windows/asterfiles.ico`.