//! Unified Control Bus (`0.0..=1.0`) and `mappings.json` configuration.
//!
//! Why we build this BEFORE any UI widgets or keyboard shortcuts:
//! Every live control in the app — whether moved by an `egui` widget, a remappable
//! keyboard shortcut (`2A-3`), or a future USB MIDI controller (Milestone 4) —
//! reads and writes through this single `ControlBus`. That means we never have to
//! rewrite the UI when adding keyboard remapping or MIDI Learn.
//!
//! Rust concepts explained for Python developers:
//! - **`BTreeMap<String, f32>`:** Like a Python `dict[str, float]`, except keys stay
//!   sorted alphabetically so logs and debug dumps are deterministic.
//! - **`#[derive(Serialize, Deserialize)]`:** Tells `serde` to generate JSON reader
//!   and writer code automatically at compile time (like Python `dataclasses` + `json`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical Control Bus parameter names shared by UI widgets, keyboard, and MIDI.
pub mod param {
    pub const MIXER_CROSSFADER: &str = "mixer.crossfader";
    pub const MIXER_BLEND_MODE: &str = "mixer.blend_mode";
    pub const MASTER_BLACKOUT: &str = "master.blackout";
    pub const DECK_A_TRANSITION_SMOOTH: &str = "deck_a.transition_smooth";
    pub const DECK_B_TRANSITION_SMOOTH: &str = "deck_b.transition_smooth";

    // One-shot trigger targets (queued as actions rather than stored as continuous values).
    pub const DECK_A_PREV_PRESET: &str = "deck_a.prev_preset";
    pub const DECK_A_NEXT_PRESET: &str = "deck_a.next_preset";
    pub const DECK_B_PREV_PRESET: &str = "deck_b.prev_preset";
    pub const DECK_B_NEXT_PRESET: &str = "deck_b.next_preset";
    pub const DECK_A_TEST_BLANK: &str = "deck_a.test_blank";
    pub const BROWSER_LOAD_OFF_AIR: &str = "browser.load_off_air";
    pub const DECK_A_LOAD_SELECTED: &str = "deck_a.load_selected";
    pub const DECK_B_LOAD_SELECTED: &str = "deck_b.load_selected";
}

/// Number of GPU blend modes supported by Milestone 2 (`2A-2`).
pub const BLEND_MODE_COUNT: usize = 5;

/// How a keyboard key or MIDI input modifies a Control Bus target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingMode {
    /// Fires a one-shot action on press.
    Trigger,
    /// Flips a parameter between `0.0` and `1.0` on each press.
    Toggle,
    /// Sets `value` while held, returns to `0.0` on release.
    Momentary,
    /// Jumps a parameter directly to `value`.
    Set,
    /// Adds `step` (positive or negative), clamped to `0.0..=1.0`.
    Nudge,
    /// Advances a discrete parameter across `steps` evenly spaced states in `0.0..=1.0`, wrapping around.
    Cycle,
    /// Maps a continuous MIDI CC (`0..=127`) linearly onto `min..=max`.
    Continuous,
}

/// One keyboard binding stored in `mappings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyboardMapping {
    /// Key name (for example `"BracketLeft"`, `"ArrowRight"`, `"M"`, `"Backspace"`, `"A"`).
    pub key: String,
    /// Required modifiers (for example `[]`, `["Shift"]`, `["Ctrl"]`).
    #[serde(default)]
    pub modifiers: Vec<String>,
    /// Target Control Bus parameter or trigger name (for example `"mixer.crossfader"`).
    pub target: String,
    /// How pressing/releasing this key updates `target`.
    pub mode: MappingMode,
    /// Target value for `set`, `momentary`, or `trigger` modes (normalized `0.0..=1.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f32>,
    /// Signed step for `nudge` mode (for example `-0.05` or `+0.05`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f32>,
    /// Number of discrete states for `cycle` mode (for example `5` blend modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<usize>,
    /// For preset load triggers: `true` forces a hard cut (`smooth_transition = false`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub hard_cut: bool,
    /// For preset load triggers: `true` bypasses on-air load protection.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_on_air: bool,
}

/// One MIDI binding stored in `mappings.json` (parsed and preserved now; driven by `midir` in Milestone 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiMapping {
    pub device: String,
    pub channel: u8,
    pub kind: String,
    pub number: u8,
    pub target: String,
    pub mode: MappingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f32>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Top-level `mappings.json` schema (Section 5.2 of the design document).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingsConfig {
    pub schema_version: u32,
    pub keyboard: Vec<KeyboardMapping>,
    #[serde(default)]
    pub midi: Vec<MidiMapping>,
}

impl Default for MappingsConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            keyboard: vec![
                KeyboardMapping {
                    key: "BracketLeft".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Set,
                    value: Some(0.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "Backslash".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Set,
                    value: Some(0.5),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "BracketRight".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Set,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "ArrowLeft".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Nudge,
                    value: None,
                    step: Some(-0.05),
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "ArrowRight".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Nudge,
                    value: None,
                    step: Some(0.05),
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "M".to_string(),
                    modifiers: vec![],
                    target: param::MIXER_BLEND_MODE.to_string(),
                    mode: MappingMode::Cycle,
                    value: None,
                    step: None,
                    steps: Some(BLEND_MODE_COUNT),
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "Backspace".to_string(),
                    modifiers: vec![],
                    target: param::MASTER_BLACKOUT.to_string(),
                    mode: MappingMode::Toggle,
                    value: None,
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "Enter".to_string(),
                    modifiers: vec![],
                    target: param::BROWSER_LOAD_OFF_AIR.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "Enter".to_string(),
                    modifiers: vec!["Shift".to_string()],
                    target: param::BROWSER_LOAD_OFF_AIR.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: true,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "A".to_string(),
                    modifiers: vec![],
                    target: param::DECK_A_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "A".to_string(),
                    modifiers: vec!["Shift".to_string()],
                    target: param::DECK_A_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: true,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "A".to_string(),
                    modifiers: vec!["Ctrl".to_string()],
                    target: param::DECK_A_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: true,
                },
                KeyboardMapping {
                    key: "B".to_string(),
                    modifiers: vec![],
                    target: param::DECK_B_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "B".to_string(),
                    modifiers: vec!["Shift".to_string()],
                    target: param::DECK_B_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: true,
                    force_on_air: false,
                },
                KeyboardMapping {
                    key: "B".to_string(),
                    modifiers: vec!["Ctrl".to_string()],
                    target: param::DECK_B_LOAD_SELECTED.to_string(),
                    mode: MappingMode::Trigger,
                    value: Some(1.0),
                    step: None,
                    steps: None,
                    hard_cut: false,
                    force_on_air: true,
                },
            ],
            midi: vec![
                MidiMapping {
                    device: "*".to_string(),
                    channel: 1,
                    kind: "cc".to_string(),
                    number: 1,
                    target: param::MIXER_CROSSFADER.to_string(),
                    mode: MappingMode::Continuous,
                    min: Some(0.0),
                    max: Some(1.0),
                    value: None,
                },
                MidiMapping {
                    device: "*".to_string(),
                    channel: 1,
                    kind: "note".to_string(),
                    number: 36,
                    target: param::MASTER_BLACKOUT.to_string(),
                    mode: MappingMode::Toggle,
                    min: None,
                    max: None,
                    value: Some(1.0),
                },
            ],
        }
    }
}

impl MappingsConfig {
    /// Loads `mappings.json` from `path`, or creates a default file if it does not exist yet.
    pub fn load_or_create_default(path: &Path) -> Result<(Self, bool), String> {
        if path.exists() {
            let text = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
            let cfg: Self = serde_json::from_str(&text)
                .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
            if cfg.schema_version != 1 {
                return Err(format!(
                    "Unsupported schema_version {} in {} (expected 1)",
                    cfg.schema_version,
                    path.display()
                ));
            }
            Ok((cfg, false))
        } else {
            let cfg = Self::default();
            cfg.save(path)?;
            Ok((cfg, true))
        }
    }

    /// Saves this mapping configuration as pretty-printed JSON.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create folder {}: {e}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize mappings.json: {e}"))?;
        fs::write(path, json)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(())
    }
}

/// One queued one-shot action emitted onto the Control Bus (by a UI button, key, or MIDI pad).
#[derive(Debug, Clone, PartialEq)]
pub struct ControlTrigger {
    pub target: String,
    pub value: f32,
    pub hard_cut: bool,
    pub force_on_air: bool,
}

/// The central parameter store (`0.0..=1.0`) and action queue shared by UI, keyboard, and MIDI.
#[derive(Debug, Clone)]
pub struct ControlBus {
    params: BTreeMap<String, f32>,
    triggers: Vec<ControlTrigger>,
    pub mappings: MappingsConfig,
}

impl ControlBus {
    /// Creates a new `ControlBus` initialized with safe Milestone 2 default values.
    pub fn new(mappings: MappingsConfig) -> Self {
        let mut params = BTreeMap::new();
        // Start full Deck A (0.0) so Deck B is off-air (CUE) by default.
        params.insert(param::MIXER_CROSSFADER.to_string(), 0.0);
        // Index 0 = Crossfade (default blend mode).
        params.insert(param::MIXER_BLEND_MODE.to_string(), 0.0);
        // Blackout OFF at startup.
        params.insert(param::MASTER_BLACKOUT.to_string(), 0.0);
        // Default preset transition: Smooth (1.5s) = 1.0 (0.0 = Hard Cut).
        params.insert(param::DECK_A_TRANSITION_SMOOTH.to_string(), 1.0);
        params.insert(param::DECK_B_TRANSITION_SMOOTH.to_string(), 1.0);

        Self {
            params,
            triggers: Vec::new(),
            mappings,
        }
    }

    /// Reads a normalized parameter value (`0.0..=1.0`), returning `0.0` if unset.
    pub fn get(&self, target: &str) -> f32 {
        self.params.get(target).copied().unwrap_or(0.0)
    }

    /// Reads a boolean parameter (`>= 0.5` is `true`).
    pub fn get_bool(&self, target: &str) -> bool {
        self.get(target) >= 0.5
    }

    /// Sets a parameter value, clamped strictly to `0.0..=1.0`.
    pub fn set(&mut self, target: &str, value: f32) {
        let clamped = if value.is_nan() {
            0.0
        } else {
            value.clamp(0.0, 1.0)
        };
        self.params.insert(target.to_string(), clamped);
    }

    /// Flips a parameter between `0.0` and `1.0`.
    pub fn toggle(&mut self, target: &str) -> bool {
        let next = if self.get_bool(target) { 0.0 } else { 1.0 };
        self.set(target, next);
        next >= 0.5
    }

    /// Adds a signed delta to a parameter, clamped to `0.0..=1.0`.
    pub fn nudge(&mut self, target: &str, step: f32) -> f32 {
        let next = (self.get(target) + step).clamp(0.0, 1.0);
        self.set(target, next);
        next
    }

    /// Advances a discrete parameter with `steps` states (`0..steps`) evenly across `0.0..=1.0`.
    pub fn cycle(&mut self, target: &str, steps: usize) -> usize {
        if steps <= 1 {
            self.set(target, 0.0);
            return 0;
        }
        let max_idx = (steps - 1) as f32;
        let current_idx = (self.get(target) * max_idx).round() as usize;
        let next_idx = (current_idx + 1) % steps;
        self.set(target, next_idx as f32 / max_idx);
        next_idx
    }

    /// Converts a discrete parameter in `0.0..=1.0` back into an integer index `0..steps`.
    pub fn get_discrete_index(&self, target: &str, steps: usize) -> usize {
        if steps <= 1 {
            return 0;
        }
        let max_idx = (steps - 1) as f32;
        ((self.get(target) * max_idx).round() as usize).min(steps - 1)
    }

    /// Queues a one-shot action trigger (for example a preset load button or key press).
    pub fn trigger(&mut self, target: &str, hard_cut: bool, force_on_air: bool) {
        self.triggers.push(ControlTrigger {
            target: target.to_string(),
            value: 1.0,
            hard_cut,
            force_on_air,
        });
    }

    /// Drains all queued triggers for the current frame so the engine can execute them once.
    pub fn drain_triggers(&mut self) -> Vec<ControlTrigger> {
        std::mem::take(&mut self.triggers)
    }

    /// Applies one `KeyboardMapping` event (`pressed = true` on key down, `false` on key up).
    pub fn apply_keyboard_mapping(&mut self, mapping: &KeyboardMapping, pressed: bool) {
        match mapping.mode {
            MappingMode::Trigger => {
                if pressed {
                    self.triggers.push(ControlTrigger {
                        target: mapping.target.clone(),
                        value: mapping.value.unwrap_or(1.0).clamp(0.0, 1.0),
                        hard_cut: mapping.hard_cut,
                        force_on_air: mapping.force_on_air,
                    });
                }
            }
            MappingMode::Toggle => {
                if pressed {
                    self.toggle(&mapping.target);
                }
            }
            MappingMode::Momentary => {
                if pressed {
                    self.set(&mapping.target, mapping.value.unwrap_or(1.0));
                } else {
                    self.set(&mapping.target, 0.0);
                }
            }
            MappingMode::Set => {
                if pressed {
                    self.set(&mapping.target, mapping.value.unwrap_or(0.0));
                }
            }
            MappingMode::Nudge => {
                if pressed {
                    self.nudge(&mapping.target, mapping.step.unwrap_or(0.05));
                }
            }
            MappingMode::Cycle => {
                if pressed {
                    self.cycle(&mapping.target, mapping.steps.unwrap_or(2));
                }
            }
            MappingMode::Continuous => {
                if pressed {
                    self.set(&mapping.target, mapping.value.unwrap_or(0.0));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_clamps_and_updates_modes_cleanly() {
        let mut bus = ControlBus::new(MappingsConfig::default());

        // Default crossfader starts at 0.0 (Deck A full, Deck B cue).
        assert_eq!(bus.get(param::MIXER_CROSSFADER), 0.0);
        assert!(bus.get_bool(param::DECK_A_TRANSITION_SMOOTH));

        // Set clamps out-of-range inputs safely.
        bus.set(param::MIXER_CROSSFADER, 1.8);
        assert_eq!(bus.get(param::MIXER_CROSSFADER), 1.0);
        bus.set(param::MIXER_CROSSFADER, -0.4);
        assert_eq!(bus.get(param::MIXER_CROSSFADER), 0.0);

        // Nudge moves in 0.05 increments and clamps cleanly.
        bus.nudge(param::MIXER_CROSSFADER, 0.05);
        assert!((bus.get(param::MIXER_CROSSFADER) - 0.05).abs() < 1e-6);
        bus.nudge(param::MIXER_CROSSFADER, -0.20);
        assert_eq!(bus.get(param::MIXER_CROSSFADER), 0.0);

        // Toggle flips blackout on and off.
        assert!(!bus.get_bool(param::MASTER_BLACKOUT));
        assert!(bus.toggle(param::MASTER_BLACKOUT));
        assert!(bus.get_bool(param::MASTER_BLACKOUT));
        assert!(!bus.toggle(param::MASTER_BLACKOUT));

        // Cycle wraps smoothly across 5 blend modes (0..4 -> 0).
        for expected in [1, 2, 3, 4, 0] {
            assert_eq!(bus.cycle(param::MIXER_BLEND_MODE, BLEND_MODE_COUNT), expected);
            assert_eq!(
                bus.get_discrete_index(param::MIXER_BLEND_MODE, BLEND_MODE_COUNT),
                expected
            );
        }

        // Momentary sets on press and clears on release.
        let momentary = KeyboardMapping {
            key: "Z".to_string(),
            modifiers: vec![],
            target: "test.momentary".to_string(),
            mode: MappingMode::Momentary,
            value: Some(0.8),
            step: None,
            steps: None,
            hard_cut: false,
            force_on_air: false,
        };
        bus.apply_keyboard_mapping(&momentary, true);
        assert!((bus.get("test.momentary") - 0.8).abs() < 1e-6);
        bus.apply_keyboard_mapping(&momentary, false);
        assert_eq!(bus.get("test.momentary"), 0.0);

        // Trigger queues and drains cleanly.
        bus.trigger(param::DECK_A_NEXT_PRESET, true, false);
        let drained = bus.drain_triggers();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].target, param::DECK_A_NEXT_PRESET);
        assert!(drained[0].hard_cut);
        assert!(bus.drain_triggers().is_empty());
    }

    #[test]
    fn mappings_json_round_trips() {
        let cfg = MappingsConfig::default();
        let json = serde_json::to_string_pretty(&cfg).expect("serialize default mappings");
        let parsed: MappingsConfig =
            serde_json::from_str(&json).expect("deserialize default mappings");
        assert_eq!(cfg, parsed);
        assert_eq!(parsed.schema_version, 1);
        assert!(!parsed.keyboard.is_empty());
        assert!(!parsed.midi.is_empty());
    }
}
