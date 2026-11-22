//! Favorites and Curated Playlists (`playlists.json`) — Stage `2A-4b`.
//!
//! Stores user-curated favorites and named playlists (like Traktor crates) locally in
//! `playlists.json` (gitignored). Presets are referenced by their normalized relative path
//! under `presets/` (`PresetEntry::rel_path`) so favorites and playlists stay valid across
//! rescans and library updates.
//!
//! Rust concepts explained for Python developers:
//! - **`HashSet<String>`:** Like a Python `set[str]` — gives O(1) `is_favorite()` checks
//!   during every 60 FPS UI frame without scanning a list.
//! - **Atomic save (`.tmp` + `rename`):** Writes JSON to a temporary file first and renames
//!   it into place so a crash or power cut never leaves a half-written `playlists.json`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Current JSON schema version for `playlists.json`.
pub const PLAYLISTS_SCHEMA_VERSION: u32 = 1;

/// Which collection view is currently active in the Preset Browser.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CollectionFilter {
    #[default]
    All,
    Favorites,
    Playlist(String),
}

/// One user-created playlist (ordered list of relative preset paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    #[serde(default)]
    pub presets: Vec<String>,
}

/// Top-level `playlists.json` schema saved on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistsFile {
    pub schema_version: u32,
    #[serde(default)]
    pub favorites: Vec<String>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
}

impl Default for PlaylistsFile {
    fn default() -> Self {
        Self {
            schema_version: PLAYLISTS_SCHEMA_VERSION,
            favorites: Vec::new(),
            playlists: Vec::new(),
        }
    }
}

/// In-memory manager for Favorites and Playlists with O(1) favorite lookups.
#[derive(Debug, Clone, Default)]
pub struct PlaylistStore {
    data: PlaylistsFile,
    favorite_set: HashSet<String>,
}

impl PlaylistStore {
    /// Loads `playlists.json` from `path`, or returns an empty store if the file does not exist yet.
    pub fn load_or_empty(path: &Path) -> Result<(Self, bool), String> {
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let text = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let data: PlaylistsFile = serde_json::from_str(&text)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        if data.schema_version != PLAYLISTS_SCHEMA_VERSION {
            return Err(format!(
                "Unsupported schema_version {} in {} (expected {})",
                data.schema_version,
                path.display(),
                PLAYLISTS_SCHEMA_VERSION
            ));
        }
        let favorite_set: HashSet<String> = data.favorites.iter().cloned().collect();
        Ok((Self { data, favorite_set }, true))
    }

    /// Saves `playlists.json` cleanly as pretty-printed JSON using an atomic temp-file swap.
    pub fn save_atomic(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create folder {}: {e}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(&self.data)
            .map_err(|e| format!("Failed to serialize playlists.json: {e}"))?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &json)
            .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
        if let Err(rename_err) = fs::rename(&tmp_path, path) {
            fs::write(path, &json).map_err(|e| {
                format!(
                    "Failed to save {} (rename: {rename_err}; direct write: {e})",
                    path.display()
                )
            })?;
            let _ = fs::remove_file(&tmp_path);
        }
        Ok(())
    }

    /// Returns `true` if `rel_path` is starred as a Favorite (O(1)).
    pub fn is_favorite(&self, rel_path: &str) -> bool {
        self.favorite_set.contains(rel_path)
    }

    /// Toggles `rel_path` in Favorites and returns the new state (`true` = now favorited).
    pub fn toggle_favorite(&mut self, rel_path: &str) -> bool {
        if self.favorite_set.remove(rel_path) {
            self.data.favorites.retain(|p| p != rel_path);
            false
        } else {
            self.favorite_set.insert(rel_path.to_string());
            self.data.favorites.push(rel_path.to_string());
            true
        }
    }

    pub fn favorite_count(&self) -> usize {
        self.data.favorites.len()
    }

    pub fn playlists(&self) -> &[Playlist] {
        &self.data.playlists
    }

    pub fn get_playlist(&self, name: &str) -> Option<&Playlist> {
        self.data
            .playlists
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Creates a new empty playlist, rejecting blank or duplicate names.
    pub fn create_playlist(&mut self, raw_name: &str) -> Result<String, String> {
        let name = raw_name.trim();
        if name.is_empty() {
            return Err("Playlist name cannot be empty.".to_string());
        }
        if self
            .data
            .playlists
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name))
        {
            return Err(format!("A playlist named `{name}` already exists."));
        }
        let owned = name.to_string();
        self.data.playlists.push(Playlist {
            name: owned.clone(),
            presets: Vec::new(),
        });
        Ok(owned)
    }

    /// Renames an existing playlist, returning the old and new names on success.
    pub fn rename_playlist(&mut self, idx: usize, raw_new_name: &str) -> Result<(String, String), String> {
        let new_name = raw_new_name.trim();
        if new_name.is_empty() {
            return Err("Playlist name cannot be empty.".to_string());
        }
        if self
            .data
            .playlists
            .iter()
            .enumerate()
            .any(|(i, p)| i != idx && p.name.eq_ignore_ascii_case(new_name))
        {
            return Err(format!("A playlist named `{new_name}` already exists."));
        }
        let Some(slot) = self.data.playlists.get_mut(idx) else {
            return Err("Playlist not found.".to_string());
        };
        let old_name = slot.name.clone();
        slot.name = new_name.to_string();
        Ok((old_name, slot.name.clone()))
    }

    /// Deletes a playlist by index and returns its removed name.
    pub fn delete_playlist(&mut self, idx: usize) -> Option<String> {
        if idx < self.data.playlists.len() {
            Some(self.data.playlists.remove(idx).name)
        } else {
            None
        }
    }

    /// Returns `true` if `rel_path` is inside the named playlist.
    pub fn playlist_contains(&self, playlist_name: &str, rel_path: &str) -> bool {
        self.get_playlist(playlist_name)
            .map(|pl| pl.presets.iter().any(|p| p == rel_path))
            .unwrap_or(false)
    }

    /// Adds or removes `rel_path` in the named playlist; returns `true` if now present.
    pub fn toggle_in_playlist(&mut self, playlist_name: &str, rel_path: &str) -> bool {
        let Some(pl) = self
            .data
            .playlists
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(playlist_name))
        else {
            return false;
        };
        if let Some(pos) = pl.presets.iter().position(|p| p == rel_path) {
            pl.presets.remove(pos);
            false
        } else {
            pl.presets.push(rel_path.to_string());
            true
        }
    }

    /// Removes `rel_path` from the named playlist; returns `true` if it was removed.
    pub fn remove_from_playlist(&mut self, playlist_name: &str, rel_path: &str) -> bool {
        let Some(pl) = self
            .data
            .playlists
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(playlist_name))
        else {
            return false;
        };
        let before = pl.presets.len();
        pl.presets.retain(|p| p != rel_path);
        pl.presets.len() != before
    }

    /// Moves `rel_path` up (`delta = -1`) or down (`delta = 1`) inside the named playlist.
    pub fn move_in_playlist(&mut self, playlist_name: &str, rel_path: &str, delta: isize) -> bool {
        let Some(pl) = self
            .data
            .playlists
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(playlist_name))
        else {
            return false;
        };
        let Some(pos) = pl.presets.iter().position(|p| p == rel_path) else {
            return false;
        };
        let target = pos as isize + delta;
        if target < 0 || target as usize >= pl.presets.len() {
            return false;
        }
        pl.presets.swap(pos, target as usize);
        true
    }

    /// Counts how many custom playlists contain `rel_path` (for compact row badges).
    pub fn playlist_membership_count(&self, rel_path: &str) -> usize {
        self.data
            .playlists
            .iter()
            .filter(|pl| pl.presets.iter().any(|p| p == rel_path))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn favorites_and_playlists_round_trip_and_reorder() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vj_playlists_test_{nanos}.json"));

        let mut store = PlaylistStore::default();
        assert!(!store.is_favorite("library/A/one.milk"));
        assert!(store.toggle_favorite("library/A/one.milk"));
        assert!(store.is_favorite("library/A/one.milk"));
        assert_eq!(store.favorite_count(), 1);

        // Create playlist and reject case-insensitive duplicates.
        assert_eq!(store.create_playlist("  Peak Drop ").unwrap(), "Peak Drop");
        assert!(store.create_playlist("peak drop").is_err());

        // Add two presets and reorder them.
        assert!(store.toggle_in_playlist("Peak Drop", "library/A/one.milk"));
        assert!(store.toggle_in_playlist("Peak Drop", "library/B/two.milk"));
        assert!(store.playlist_contains("Peak Drop", "library/A/one.milk"));
        assert_eq!(store.playlist_membership_count("library/A/one.milk"), 1);

        assert!(store.move_in_playlist("Peak Drop", "library/B/two.milk", -1));
        assert_eq!(
            store.get_playlist("Peak Drop").unwrap().presets,
            vec![
                "library/B/two.milk".to_string(),
                "library/A/one.milk".to_string()
            ]
        );

        // Save and reload cleanly.
        store.save_atomic(&path).unwrap();
        let (reloaded, existed) = PlaylistStore::load_or_empty(&path).unwrap();
        assert!(existed);
        assert!(reloaded.is_favorite("library/A/one.milk"));
        assert_eq!(
            reloaded.get_playlist("Peak Drop").unwrap().presets,
            vec![
                "library/B/two.milk".to_string(),
                "library/A/one.milk".to_string()
            ]
        );

        let _ = fs::remove_file(&path);
    }
}
