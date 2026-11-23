# GitHub Copilot Instructions (`milkdrop-vj`)

Read [AGENTS.md](../AGENTS.md) as the single source of truth for architecture, Windows/OpenGL/NVIDIA invariants, Control Bus rules, and verification steps.

Key non-negotiables when proposing edits:
1. Keep all live controls routed through `ControlBus` (`src/control/mod.rs`) and `mappings.json`.
2. Never replace `src/egui_renderer.rs` with `egui_glow` (avoids an NVIDIA WGL `glBufferData(STREAM_DRAW)` memory leak) and never remove `default_fonts` from `egui 0.37`.
3. Preserve OpenGL 3.3 Core Section 5.3 fence sync (`glFenceSync` + `glFlush` on deck contexts; GPU-only `glWaitSync` on main/projector contexts) and never add `glFinish()` or synchronous full-frame `glReadPixels()` to the live loop.
4. Keep UI layout DPI-aware in logical `egui` points (`1280x720 pt` minimum), preserve 2-click Load Safety on `ON-AIR`/`MIXED` decks, and pause performance hotkeys in Typing Mode or modals.
5. Run `cargo test` and verify `Total GL errors: 0` in a Release build.

For detailed playbooks, see `.agents/skills/` and `docs/project_research_and_design.md`.
