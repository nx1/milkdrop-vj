# MilkDrop VJ

A live, human-in-the-loop **Video DJ (VJ)** instrument for Windows powered by **`libprojectM` 4.1.8** and written in **Rust**.

Mix two independent MilkDrop visual decks live like DJing music in **Traktor**—with a dark-venue control console, keyboard-first controls, real-time Windows audio capture, curated playlists, and a unified control bus designed for USB MIDI controllers.

---

## What Works Today

- **Dual Independent MilkDrop Decks (Deck A & Deck B):** Two isolated `libprojectM 4.1.8` OpenGL 3.3 Core instances rendering simultaneously with zero visual bleed.
- **Live GPU Mixer & Master Preview:** Real-time crossfader (`0.0` to `1.0`, double-click to center), **`ON-AIR` / `MIXED` / `CUE`** deck indicators, and 5 blend modes:
  - `Crossfade`, `Additive`, `Multiply`, `Difference`, and `Luma Key`
- **Emergency Blackout:** Smooth `0.4 s` fade-to-black panic toggle (`Backspace`) on the Master output.
- **Live Windows System Audio (WASAPI Loopback):** Captures music playing out of Windows speakers/headphones/USB audio interfaces via a lock-free ring buffer, with automatic silence handling and a built-in synthetic test signal (`--audio-source test`).
- **Keyboard-First Controls & Unified Control Bus:** Every live control reads and writes normalized values (`0.0` to `1.0`) through `mappings.json`. Remap any key in the app (**`Keys`** button), including UK/European ISO `\` keyboard support.
- **Load Safety Guard:** Prevents accidentally overwriting a live (`ON-AIR` or `MIXED`) deck with a single stray keypress or click (requires a 2-click **`CONFIRM?`** on mouse, or **`Ctrl+A` / `Ctrl+B`** on keyboard).
- **10,000-Preset Browser, Favorites, & Curated Playlists:**
  - Non-blocking background scanner + local `preset_cache.json` disk cache (loads ~9,800 presets in ~1.6 s once cached).
  - Live multi-word search (`/`), estimated load filter (**`Weight:`** `Light` / `Med` / `Heavy`), and **`Folder:`** filter.
  - One-click **Favorites (`*` / `FAV`)** and ordered **Curated Playlists** saved to `playlists.json` (with `< Prev` / `Next >` stepping through your playlist order).
- **Live Safety Telemetry:** Non-blocking `GL_TIME_ELAPSED` GPU render timers per deck, asynchronous `32x18` PBO blank-deck detection (warns if a broken preset stays black for 2+ seconds), and preset-load stall logging.

---

## Roadmap & Current Status

| Milestone | Scope | Status |
| :--- | :--- | :--- |
| **Milestone 1** | Dual `libprojectM 4.1.8` OpenGL engine + WASAPI Loopback audio + lock-free ring buffer | **Complete** |
| **Milestone 2 (`2A-1` – `2A-4b`)** | Control Bus, dark-venue `egui` console, GPU mixer (5 blend modes + Blackout), remappable keys, 10k-preset browser, Favorites & Playlists | **Complete** |
| **Milestone 2 (`2A-5`)** | Top bar: live audio device dropdown (unplug-safe restart), Live/Test source switch, runtime `720p` / `1080p` deck toggle | **Next** |
| **Milestone 2 (`2B`)** | Borderless fullscreen Projector Output window on a second monitor | Planned |
| **Milestone 3** | Per-deck 3-band Visual EQ + Kill switches, Manual Beat Pulse (`Space`), Tap Tempo (`T`), 2 GPU Effect Racks, Hot-Preset Banks (`1`–`8`) | Planned |
| **Milestone 4** | Line-In / USB audio interface input, USB MIDI (`midir`) + MIDI Learn, Spout2 output | Planned |

For full architecture notes, benchmarks, and UI specifications, read [docs/project_research_and_design.md](docs/project_research_and_design.md).

---

## Quick Start on Windows

### 1. Install Prerequisites (One-Time Setup)

You need a 64-bit Windows 10/11 machine with an OpenGL 3.3+ GPU (dedicated NVIDIA or AMD GPU strongly recommended) and these tools:

1. **Git for Windows:** https://git-scm.com/download/win
2. **Visual Studio 2022 Build Tools** (with the **Desktop development with C++** workload): https://visualstudio.microsoft.com/visual-cpp-build-tools/
3. **Rust (`rustup`):** https://rustup.rs (choose the default `x86_64-pc-windows-msvc` toolchain)
4. **LLVM (for `libclang`, used by Rust `bindgen`):** Install from https://github.com/llvm/llvm-project/releases (or `winget install LLVM.LLVM`), then set your user environment variable:
   ```powershell
   [Environment]::SetEnvironmentVariable("LIBCLANG_PATH", "C:\Program Files\LLVM\bin", "User")
   ```
5. **`vcpkg` (C++ package manager for `libprojectM` 4.1.8):**
   ```powershell
   git clone https://github.com/microsoft/vcpkg.git C:\vcpkg
   C:\vcpkg\bootstrap-vcpkg.bat
   [Environment]::SetEnvironmentVariable("VCPKG_ROOT", "C:\vcpkg", "User")
   ```
   *(Open a fresh PowerShell window after setting environment variables.)*

### 2. Clone the Repository & Install `libprojectM`

```powershell
git clone <YOUR_REPO_URL> C:\dev\vj
cd C:\dev\vj

# Builds and installs libprojectM 4.1.8 + GLEW into .\vcpkg_installed\x64-windows\
C:\vcpkg\vcpkg.exe install --triplet=x64-windows
```

### 3. Add MilkDrop Presets (`.milk` Files)

Preset files are **not** stored in this Git repository (they are gitignored). The easiest way to get started is the open-source **Cream of the Crop** collection (~9,795 curated presets):

```powershell
git clone --depth 1 https://github.com/projectM-visualizer/presets-cream-of-the-crop.git C:\dev\vj\presets\library
Remove-Item -Recurse -Force C:\dev\vj\presets\library\.git
```

*(You can also drop any individual `.milk` files directly into `presets\` or subfolders under `presets\`.)*

### 4. Test, Build (Release), and Run

Always run the **Release** build for smooth 60 FPS visuals:

```powershell
# Run unit tests (verifies audio ring buffer, Control Bus, complexity badges, cache, playlists, mixer)
cargo test

# Build optimized Release executable (copies projectM-4.dll and glew32.dll automatically)
cargo build --release --bin milkdrop-vj

# Launch with live Windows system audio (WASAPI Loopback)
.\target\release\milkdrop-vj.exe
```

#### Useful CLI Flags

| Flag | Example | What It Does |
| :--- | :--- | :--- |
| `--audio-source` | `--audio-source test` | Use built-in synthetic beat audio instead of live Windows audio (`live` is default) |
| `--audio-device` | `--audio-device "USB"` | Pick a Windows output device by 1-based index or name substring |
| `--deck-size` | `--deck-size 1920x1080` | Internal render resolution per deck (default `1280x720`) |
| `--auto-exit-secs` | `--auto-exit-secs 10` | Automatically exit cleanly after `N` seconds (useful for quick sanity checks) |
| `--log` | `--log logs\my_run.log` | Write session telemetry and OpenGL error checks to a custom log path |
| `--measure-gpu` | `--measure-gpu` | Diagnostic `glFinish()` timing (keep off during live shows) |

---

## Controls & Keyboard Shortcuts

All live shortcuts can be remapped inside the app via the top-bar **`Keys`** button (saved to local `mappings.json`).

### Performance Mode (Default)

| Key | Action |
| :--- | :--- |
| **`[`** / **`\`** / **`]`** | Crossfader: Cut Deck A (`100% A`) / Center (`50/50`) / Cut Deck B (`100% B`) |
| **`←`** / **`→`** | Nudge crossfader left / right (`5%` steps; hold to slide smoothly) |
| **`M`** | Cycle GPU blend mode (`Crossfade` → `Additive` → `Multiply` → `Difference` → `Luma Key`) |
| **`Backspace`** | Toggle **Emergency Blackout** (`0.4 s` smooth fade to/from black) |
| **`↑`** / **`↓`** | Select previous / next preset row in the filtered browser list |
| **`Enter`** | Load selected preset into whichever deck is off-air (`CUE`, `<= 5%` weight). Hold **`Shift`** for Hard Cut |
| **`A`** / **`B`** | Load selected preset into Deck A / Deck B (blocked if live; use **`Ctrl+A`** / **`Ctrl+B`** to force load; add **`Shift`** for Hard Cut) |
| **`/`** | Focus the Search box (**Typing Mode** — pauses all live performance hotkeys with an amber banner) |
| **`Esc`** | Exit Typing Mode or close modals (second `Esc` clears search; `Esc` never quits the app) |

### Load Safety Rules

- **`CUE`** (`<= 5%` crossfader weight): Loads immediately on one click or keypress.
- **`ON-AIR`** (`>= 95%`) or **`MIXED`** (`5%–95%`): Protected. Clicking `[Load A]`, `[Load B]`, `< Prev`, or `Next >` arms a yellow **`CONFIRM?`** state for 3 seconds; click again to confirm.

---

## Project Structure

```text
.
├── AGENTS.md                        # Universal AI coding agent handbook (read first if using an LLM!)
├── CLAUDE.md / GEMINI.md            # Entry pointers for Claude Code & Gemini CLI
├── .github/copilot-instructions.md  # Entry pointer for GitHub Copilot
├── .agents/skills/                  # Modular task playbooks (build/verify, OpenGL, Control Bus/UI)
├── docs/
│   └── project_research_and_design.md  # Architecture, benchmarks, approved UI spec, & roadmap
├── build.rs                         # Links libprojectM + GLEW via vcpkg, runs bindgen, copies DLLs
├── vcpkg.json                       # Declares C++ dependency (projectm)
└── src/
    ├── main.rs                      # App event loop, keyboard router, audio/deck/mixer orchestration
    ├── lib.rs                       # Shared library exports for binaries & unit tests
    ├── ui.rs                        # Dark-venue egui 0.37 console, Keys modal, Playlists modal
    ├── egui_renderer.rs             # Zero-reallocation OpenGL 3.3 batched renderer (avoids NVIDIA WGL leak)
    ├── render_deck.rs               # Per-deck WGL pbuffer context, GL 5.3 fence sync, GPU timers, blank PBO
    ├── mixer.rs                     # GPU Master FBO, 5 blend shaders, DeckAirState, 0.4s blackout fade
    ├── deck.rs                      # Safe Rust wrapper around libprojectM C FFI + PCM chunking
    ├── gl_state.rs                  # Saves/restores OpenGL state around libprojectM calls
    ├── gl_log.rs                    # Per-frame OpenGL error logger & telemetry log writer
    ├── metrics.rs                   # Frame timing statistics & Windows process memory sampler
    ├── control/mod.rs               # Unified Control Bus (0.0..=1.0) & mappings.json persistence
    ├── preset_browser.rs            # Background scanner thread, preset_cache.json, fast filter
    ├── preset_meta.rs               # Static .milk complexity estimator (enabled shapecode num_inst)
    ├── playlists.rs                 # Favorites & curated playlists (playlists.json)
    ├── audio/                       # AudioSource trait, WASAPI Loopback, lock-free SPSC ring buffer
    ├── test_audio.rs                # Synthetic beat & harmonic audio generator (--audio-source test)
    └── bin/audio_test.rs            # Standalone console audio meter utility
```

**Local files kept out of Git (`.gitignore`):** `target/`, `vcpkg_installed/`, `presets/`, `logs/`, `mappings.json`, `preset_cache.json`, and `playlists.json`.

---

## Contributing With an AI Assistant

If you are contributing using **Antigravity, Claude Code, Codex, Cursor, Windsurf, GitHub Copilot, or Gemini CLI**, your assistant should automatically pick up the project instructions from:

- **[AGENTS.md](AGENTS.md)** — core architecture, OpenGL & NVIDIA driver invariants, coding rules, and current milestone status.
- **[.agents/skills/](.agents/skills/)** — step-by-step playbooks for building/verifying, touching OpenGL multi-deck code, and adding new controls through the Control Bus.

Always run `cargo test` and verify `Total GL errors: 0` in the Release log before opening a pull request.

---

## Credits & Licensing Notes

- Visual engine: [**libprojectM 4.x**](https://github.com/projectM-visualizer/projectm) (LGPL-2.1, linked dynamically via `projectM-4.dll` on Windows).
- Preset collection referenced in setup: [**presets-cream-of-the-crop**](https://github.com/projectM-visualizer/presets-cream-of-the-crop) (check its `LICENSE.md` for public/commercial show guidance).
- Windows `vcpkg` + `bindgen` and OpenGL state-isolation lessons inspired by [**OpenDrop-VJ**](https://github.com/kushiemoon-dev/OpenDrop-VJ) (MIT).
