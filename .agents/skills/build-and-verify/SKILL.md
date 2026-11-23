---
name: build-and-verify
description: Build, test, and verify milkdrop-vj on Windows using vcpkg, cargo test, Release builds, and automated log inspection. Use before committing or after any code change.
---

# Build and Verify (`milkdrop-vj`)

Follow this playbook whenever building, testing, or verifying changes to `milkdrop-vj` on Windows.

## 1. Environment Variables (PowerShell)

Fresh non-interactive shells on Windows sometimes miss user environment variables. Restore them before running `cargo` or `git`:

```powershell
$env:Path = [System.Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [System.Environment]::GetEnvironmentVariable('Path','User')
$env:LIBCLANG_PATH = [System.Environment]::GetEnvironmentVariable('LIBCLANG_PATH','User')
$env:VCPKG_ROOT = [System.Environment]::GetEnvironmentVariable('VCPKG_ROOT','User')
```

If `vcpkg_installed\x64-windows\include\projectM-4\projectM.h` is missing, install `libprojectM` once:

```powershell
& "$env:VCPKG_ROOT\vcpkg.exe" install --triplet=x64-windows
```

## 2. Unit Tests & Release Build

Never run `cargo` in parallel with file edits. Finish all edits first, then run:

```powershell
cargo test
cargo build --release --bin milkdrop-vj
```

Requirements:
- All unit tests pass (`audio`, `control`, `deck`, `preset_meta`, `preset_browser`, `playlists`, `mixer`).
- Zero compiler warnings.
- `build.rs` automatically copies `projectM-4.dll` and `glew32.dll` next to the compiled executable.

## 3. Automated 6-Second Release Sanity Check

Before asking a human to test on their desktop, run a short synthetic-audio Release verification:

```powershell
.\target\release\milkdrop-vj.exe --audio-source test --auto-exit-secs 6 --log logs\_sanity.log
Get-Content logs\_sanity.log
Remove-Item logs\_sanity.log -ErrorAction SilentlyContinue
```

## 4. Log Audit Checklist (Must Pass)

Verify every item in the log output:
1. **`GL_RENDERER`:** Confirm the dedicated GPU (not Intel integrated graphics unless unavoidable).
2. **`Total GL errors: 0`:** Any non-zero OpenGL error is a blocker — fix it before proceeding.
3. **FPS & Deck GPU Timers:** ~`60 FPS` on light presets (`1280x720` default), `GL_TIME_ELAPSED` showing realistic `0.5–3.0 ms` per light deck (not `10–15 ms` VSync-polluted wall time).
4. **Memory Stability:** WorkingSet and PrivateCommit should flatten quickly after initial shader/UI warmup (watch for steady upward drift across 5-second log lines).
5. **Clean Exit:** Process exits with code `0` and prints `closed cleanly`.

## 5. Performance Measurement Rules

- Always benchmark **Release** builds (`target\release\milkdrop-vj.exe`), never Debug.
- Keep developer smoke tests to `<= 60 seconds` and default `1280x720` per deck on constrained laptops.
- Label laptop benchmark figures as *provisional, laptop-limited*.
