//! Persistent Hot-Preset Banks (`hot_banks.json`) for Milestone 3 Stage `3D`.
//!
//! Stores 4 instant-recall slots for Deck A (`1`–`4`) and 4 slots for Deck B (`5`–`8`).
//! Each slot references a preset by its portable relative path under `presets/`
//! (matching `playlists.json`) plus its cached display name so pads render immediately
//! at startup before the background library scanner finishes.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Number of Hot-Preset Bank pads per deck (`1`–`4` on Deck A, `5`–`8` on Deck B).
pub const HOT_BANK_SLOTS_PER_DECK: usize = 4;

/// A single assigned Hot-Preset Bank pad.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotBankSlot {
    /// Forward-slash relative path under `presets/` (e.g. `"library/Geiss - Spiral.milk"`).
    pub rel_path: String,
    /// Human-readable display name shown on the pad button.
    pub display_name: String,
}

/// Persistent `hot_banks.json` store for Deck A (`0..4`) and Deck B (`0..4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotBankStore {
    pub schema_version: u32,
    pub deck_a: [Option<HotBankSlot>; HOT_BANK_SLOTS_PER_DECK],
    pub deck_b: [Option<HotBankSlot>; HOT_BANK_SLOTS_PER_DECK],
}

impl Default for HotBankStore {
    fn default() -> Self {
        Self {
            schema_version: 1,
            deck_a: [None, None, None, None],
            deck_b: [None, None, None, None],
        }
    }
}

impl HotBankStore {
    /// Loads `hot_banks.json` if it exists; otherwise returns an empty bank store.
    /// Returns `(store, loaded_from_disk)`.
    pub fn load_or_default(path: &Path) -> Result<(Self, bool), String> {
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let text = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let store: Self = serde_json::from_str(&text)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        if store.schema_version != 1 {
            return Err(format!(
                "Unsupported schema_version {} in {} (expected 1)",
                store.schema_version,
                path.display()
            ));
        }
        Ok((store, true))
    }

    /// Saves `hot_banks.json` as pretty-printed JSON.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create folder {}: {e}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize hot_banks.json: {e}"))?;
        fs::write(path, json)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
        Ok(())
    }

    /// Returns the assigned slot (`0..4`) for Deck A (`is_deck_a = true`) or Deck B (`false`).
    pub fn get(&self, is_deck_a: bool, slot: usize) -> Option<&HotBankSlot> {
        let bank = if is_deck_a { &self.deck_a } else { &self.deck_b };
        bank.get(slot).and_then(|s| s.as_ref())
    }

    /// Assigns a preset to the given deck and slot (`0..4`).
    pub fn assign(
        &mut self,
        is_deck_a: bool,
        slot: usize,
        rel_path: String,
        display_name: String,
    ) -> bool {
        let bank = if is_deck_a {
            &mut self.deck_a
        } else {
            &mut self.deck_b
        };
        if let Some(entry) = bank.get_mut(slot) {
            *entry = Some(HotBankSlot {
                rel_path,
                display_name,
            });
            true
        } else {
            false
        }
    }

    /// Clears a slot (`0..4`) back to empty (`None`).
    pub fn clear(&mut self, is_deck_a: bool, slot: usize) -> bool {
        let bank = if is_deck_a {
            &mut self.deck_a
        } else {
            &mut self.deck_b
        };
        if let Some(entry) = bank.get_mut(slot) {
            let had = entry.is_some();
            *entry = None;
            had
        } else {
            false
        }
    }

    /// Counts how many total pads across Deck A and Deck B currently have presets saved.
    pub fn assigned_count(&self) -> usize {
        self.deck_a.iter().filter(|s| s.is_some()).count()
            + self.deck_b.iter().filter(|s| s.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn hot_banks_assign_clear_and_json_round_trip() {
        let mut store = HotBankStore::default();
        assert_eq!(store.assigned_count(), 0);
        assert!(store.assign(
            true,
            0,
            "library/Geiss - Spiral.milk".to_string(),
            "Geiss - Spiral".to_string(),
        ));
        assert!(store.assign(
            false,
            3,
            "library/Rovastar - Solar.milk".to_string(),
            "Rovastar - Solar".to_string(),
        ));
        assert!(!store.assign(true, 4, "bad.milk".to_string(), "Bad".to_string()));
        assert_eq!(store.assigned_count(), 2);

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("milkdrop_vj_hot_banks_{stamp}.json"));
        store.save(&tmp).expect("save hot_banks.json");

        let (loaded, from_disk) = HotBankStore::load_or_default(&tmp).expect("load hot_banks.json");
        let _ = fs::remove_file(&tmp);
        assert!(from_disk);
        assert_eq!(loaded, store);
        assert_eq!(
            loaded.get(true, 0).map(|s| s.display_name.as_str()),
            Some("Geiss - Spiral")
        );
        assert_eq!(
            loaded.get(false, 3).map(|s| s.rel_path.as_str()),
            Some("library/Rovastar - Solar.milk")
        );

        let mut cleared = loaded;
        assert!(cleared.clear(true, 0));
        assert!(!cleared.clear(true, 0));
        assert_eq!(cleared.assigned_count(), 1);
    }
}
