# AGENTS.md — AI Coding Assistant Handbook (`milkdrop-vj`)

Read this file before editing code in this repository. It captures the project's architecture, hard-won Windows/OpenGL invariants, testing rules, and roadmap so any developer or AI assistant can contribute safely without re-introducing subtle bugs.

For full background, design specs, and benchmarks, see [docs/project_research_and_design.md](docs/project_research_and_design.md).

---

## 1. Project Goal & Current State

`milkdrop-vj` is a human-in-the-loop **Video DJ (VJ)** desktop instrument for Windows written in **Rust**, powered by **`libprojectM` 4.1.8** (MilkDrop-compatible visualizer), designed to feel like **Traktor** (keyboard/mouse now, USB MIDI in Milestone 4).

### Current Milestone Progress
- **Milestone 1 (Complete):** Dual isolated `libprojectM 4.1.8` OpenGL 3.3 Core decks + native Windows WASAPI Loopback audio capture via a lock-free SPSC ring buffer.
- **Milestone 2 (In Progress):**
  - `2A-1` **Done:** Control Bus (`0.0..=1.0`) + `mappings.json`, dark-venue `egui 0.37` console, custom zero-leak GL renderer (`src/egui_renderer.rs`), OpenGL 5.3 fence sync, non-blocking `GL_TIME_ELAPSED` deck timers, async `32x18` PBO blank-deck detector, `[LOAD-STALL]` logging.
  - `2A-2` **Done:** `GpuMixer` FBO (`src/mixer.rs`), Master preview, 5 blend modes (`Crossfade`, `Additive`, `Multiply`, `Difference`, `Luma Key`), crossfader + double-click center, `ON-AIR`/`MIXED`/`CUE` badges, 2-click Load Safety, `0.4 s` smooth Emergency Blackout (`Backspace`).
  - `2A-3` **Done:** Remappable keyboard routing via Control Bus + Focus Model (`/` Typing Mode, `Esc`, `↑`/`↓`, `Enter`, `A`/`B`/`Shift`/`Ctrl`) + in-app `Keys` remap modal + UK ISO `KeyCode::IntlBackslash` fix.
  - `2A-4` & `2A-4b` **Done:** Background `preset-scanner` thread + `preset_cache.json` (with 1,000-file checkpoints), live Search / `Weight:` / `Folder:` filters, virtualized `ScrollArea::show_rows` (handles 9,800+ presets cleanly), Favorites (`*` / `FAV`) & ordered Curated Playlists (`playlists.json`).
  - **`2A-5` (Next):** Top bar — live audio device dropdown (non-blocking background capture-thread restart, unplug-safe), `Live WASAPI` ↔ `Test Audio` switch, and runtime `1280x720` ↔ `1920x1080` deck resolution toggle.
  - **`2B` (After `2A-5`):** Borderless fullscreen Projector Output window on a second monitor.
- **Milestone 3 (Planned):** Per-deck 3-band Visual EQ + Kill switches, Manual Beat Pulse (`Space`), Tap Tempo (`T`), 2 GPU Effect Racks, Hot-Preset Banks (`1`–`8`).
- **Milestone 4 (Planned):** Line-In / USB audio interface input (`AudioSource` backend #2), USB MIDI (`midir`) + MIDI Learn, Spout2 output (`VideoOutput` trait).

---

## 2. Hard Engineering Invariants (Do Not Break)

These rules exist because each one solved a real, measured bug on Windows / OpenGL / NVIDIA drivers:

1. **Multi-Deck OpenGL 3.3 Core Pattern 1 + Section 5.3 Sync (`src/render_deck.rs`, `src/gl_state.rs`):**
   - Deck A and Deck B each run in their own OpenGL 3.3 Core WGL pbuffer context shared with the main window context (pbuffer config MUST use `.compatible_with_native_window(raw_window_handle)`).
   - Call `init_gl_loader_for_projectm()` (`glewInit()`) once after creating the main OpenGL context and before any `Deck::new()`.
   - Save and restore OpenGL state tightly around `projectm_opengl_render_frame()`.
   - After copying a deck's pbuffer to its shared `GL_TEXTURE_2D`, insert **`glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0)` + `glFlush()`** on the deck context. Before sampling that texture in the main or projector context, call GPU-only **`glWaitSync(fence, 0, GL_TIMEOUT_IGNORED)`**. Always retire signaled fences on the deck context that created them.
2. **Do NOT Re-Introduce `egui_glow` (`src/egui_renderer.rs`):**
   - `egui_glow::Painter::paint_mesh()` calls `glBufferData(..., GL_STREAM_DRAW)` ~25×/frame, which triggers a `+2.3 GiB/hr` driver memory leak across WGL shared contexts on NVIDIA (`596.58`).
   - Keep using our custom zero-reallocation batched `glBufferSubData` renderer (`src/egui_renderer.rs`) paired directly with `egui = { version = "0.37.0", features = ["default_fonts"] }` and `egui-winit 0.37.0`. (Never drop the `default_fonts` feature in `egui 0.37`, or all glyphs render as squares.)
3. **Unified Control Bus First (`src/control/mod.rs`):**
   - Every live performance control (UI widgets, keyboard shortcuts, future MIDI) MUST read and write normalized values (`0.0..=1.0`) or triggers through `ControlBus` and `mappings.json`. Never wire shortcut logic directly to widgets bypassing the bus.
4. **Never Stall the 60 FPS Render Loop:**
   - Never call `glFinish()` in normal live mode (allowed only behind diagnostic `--measure-gpu`). Use the non-blocking 3-slot `GL_TIME_ELAPSED` query ring in `src/render_deck.rs`.
   - Never call full-resolution synchronous `glReadPixels()`. Blank-deck detection MUST remain downscaled (`32x18`) asynchronous PBO readback (~2×/sec).
   - Keep disk scanning (`src/preset_browser.rs`) and audio capture / device restarts (`src/audio/capture_thread.rs`) off the main OpenGL thread.
   - Always chunk PCM fed into `libprojectM` to `<= 576` samples per channel per call (`projectm_pcm_get_max_samples()`).
5. **Single-VSync Swap-Interval Rule (for Stage `2B` Dual Windows):**
   - In a single-threaded OpenGL loop, only **one** window may use `SwapInterval::Wait(1)` (otherwise two consecutive VSync waits halve frame rate to `30 FPS`).
   - When the Projector window is open: `Wait(1)` on Projector Output, `DontWait` on Control Console. When only the Control Console is open: `Wait(1)` on Control Console.
6. **UI Layout, Keyboard, & Safety Contracts (`src/ui.rs`, `src/main.rs`):**
   - All UI sizes are `egui` logical points (`pt`), never raw pixels. Support minimum `1280x720 pt` and design `1920x1080 pt` without cutting off controls (Section 13 of design doc).
   - Keep UK/European ISO keyboard support (`KeyCode::IntlBackslash` -> `"Backslash"`, UK `#` -> `"Hash"`).
   - Keep Load Safety on `ON-AIR` (`w >= 0.95`) and `MIXED` (`0.05 < w < 0.95`) decks: 2-click 3-second `CONFIRM?` on mouse; `Ctrl` override on keyboard; `Shift` = Hard Cut.
   - Pause all live performance hotkeys whenever Search Typing Mode or any modal (`Keys`, `Playlists`) is open. `Esc` exits modes/modals and NEVER quits the app.

---

## 3. Repository & Working Conventions

- **Plain Language (ISO 24495-1):** Write comments, docs, commit messages, and user explanations in clear, concise plain English. Explain non-obvious Rust or OpenGL concepts plainly.
- **Keep Local Data Out of Git:** Never commit `target/`, `vcpkg_installed/`, `presets/`, `logs/`, `mappings.json`, `preset_cache.json`, or `playlists.json`.
- **Measure Performance in Release Only:** Never compare Debug timings. When running on a constrained laptop, default to `1280x720` per deck and short `<= 60s` test runs, and label laptop benchmarks as *provisional, laptop-limited*.
- **No Silent Workarounds:** If a build, OpenGL check, or test fails, report the exact error rather than hiding it.

---

## 4. Quick Verification Commands (Windows PowerShell)

```powershell
# 1. Unit tests (must pass with 0 warnings)
cargo test

# 2. Optimized Release build (copies projectM-4.dll and glew32.dll via build.rs)
cargo build --release --bin milkdrop-vj

# 3. Short 6-second Release sanity check (verifies GL errors = 0, FPS, memory, clean exit)
.\target\release\milkdrop-vj.exe --audio-source test --auto-exit-secs 6 --log logs\_sanity.log
Get-Content logs\_sanity.log
Remove-Item logs\_sanity.log
```

---

## 5. Modular Skills (`.agents/skills/`)

Load the relevant skill file before making changes in that area:
- [`.agents/skills/build-and-verify/SKILL.md`](.agents/skills/build-and-verify/SKILL.md) — Windows environment setup, `vcpkg`, `cargo test`, Release verification, and log audit checklist.
- [`.agents/skills/opengl-multi-deck/SKILL.md`](.agents/skills/opengl-multi-deck/SKILL.md) — Shared WGL contexts, Section 5.3 fence sync, `egui_renderer.rs` leak invariant, GPU timers, and Stage `2B` dual-window VSync rules.
- [`.agents/skills/control-bus-and-ui/SKILL.md`](.agents/skills/control-bus-and-ui/SKILL.md) — Adding parameters/triggers to `ControlBus` & `mappings.json`, Section 13 layout math, Focus Model, UK ISO keys, and Load Safety.
