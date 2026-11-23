---
name: control-bus-and-ui
description: Conventions for adding parameters, keyboard shortcuts, UI controls, layout widgets, and safety guards in milkdrop-vj. Use before editing src/control/mod.rs, src/ui.rs, or keyboard/UI routing in src/main.rs.
---

# Control Bus, Keyboard, & Dark-Venue UI Playbook

Follow these conventions whenever adding or modifying controls, shortcuts, or UI panels.

## 1. Control Bus First (`src/control/mod.rs`)

Every live performance control (crossfader, blend mode, blackout, future Visual EQ, Effect Racks, Beat Pulse, Hot-Preset Banks, and MIDI) MUST flow through `ControlBus`:
- Add a canonical dot-separated constant in `control::param` (e.g., `"mixer.crossfader"`, `"deck_a.eq.bass"`).
- Store continuous and boolean state normalized in **`0.0..=1.0`** (`>= 0.5` = `true`).
- Discrete selectors (like 5 blend modes) map evenly across `0.0..=1.0` via `get_discrete_index()` / `cycle()`.
- One-shot actions use `bus.trigger(target, hard_cut, force_on_air)` and are drained once per frame in `ConsoleState::process_bus_triggers()`.
- Any default keyboard shortcut goes into `MappingsConfig::default()` (`mappings.json` schema v1) with a plain-language `action_description()` so the in-app **`Keys`** modal can display and remap it automatically.

## 2. Keyboard & UK ISO Compatibility (`src/main.rs`)

- Route shortcuts from `winit`'s `PhysicalKey::Code(KeyCode::...)` (plus `event.logical_key` where regional layouts differ) so `Shift` and `Ctrl` combinations work cleanly on Windows.
- **UK / European ISO layout rule:** Keep `KeyCode::IntlBackslash => "Backslash"` and map `KeyCode::Backslash` to `"Hash"` when `logical_key` is `#` / `~` (else `"Backslash"`).
- Allow OS key-repeat **only** for `MappingMode::Nudge` (smooth fader sliding); ignore `event.repeat` for `Toggle`, `Trigger`, `Cycle`, and `Set`.
- **Pause live hotkeys** whenever:
  1. Search box has focus (Typing Mode — show the `30 pt` amber `KEYS PAUSED` banner).
  2. Any modal (`Keys`, `Playlists`, etc.) is open.
- `Esc` exits Typing Mode / closes modals (second `Esc` clears search) and **never** exits the app.

## 3. Dark-Venue Layout & DPI Scaling (`src/ui.rs`)

- Keep `egui = { version = "0.37.0", features = ["default_fonts"] }` enabled.
- Use `egui` logical points (`pt`), never raw pixels.
- Respect Section 13 vertical budgets so nothing clips at **`1280x720 pt`** (minimum) or **`1920x1080 pt`** (design):
  - Top Bar: `56 pt`
  - Status Strip: `28 pt`
  - Visual Stage: `(window_h * 0.40).clamp(306.0, 432.0)` with exact `16:9` previews derived from available height (`stage_h - 126.0`)
  - Preset Browser: `CentralPanel` taking all remaining vertical height, using virtualized `ScrollArea::show_rows` (never loop all 9,800+ rows eagerly!)
- Prefer guaranteed crisp ASCII/text badges (`*`, `FAV`, `LIGHT`, `ON-AIR`, `CONFIRM?`) over obscure Unicode symbols that may render as squares in bundled fonts.

## 4. Load Safety & Destructive Guards

- Air states derived strictly from `mixer.crossfader`:
  - `ON-AIR`: deck weight `>= 0.95`
  - `MIXED`: `0.05 < w < 0.95`
  - `CUE`: `w <= 0.05` (safe off-air deck)
- Any mouse load button (`Load A`, `Load B`, `< Prev`, `Next >`) on a protected deck (`w > 0.05`) MUST use `guarded_load_button` (2-click 3-second yellow `CONFIRM?`).
- Any destructive curation action (like deleting a playlist) also uses a 2-click `CONFIRM?` guard.
- Never show unimplemented future controls as active; grey out upcoming stage placeholders with their stage tag or omit Milestone 3/4 controls until scheduled.
