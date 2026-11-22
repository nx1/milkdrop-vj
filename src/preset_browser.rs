//! Background Preset Scanner, Disk Cache (`preset_cache.json`), and Fast Filter (`2A-4`).
//!
//! Why this runs on a background thread:
//! - Scanning hundreds or thousands of `.milk` files synchronously on the main thread would
//!   freeze startup and stall the 60 FPS OpenGL render loop.
//! - Instead, `PresetScannerHandle::spawn()` walks `presets\` on a worker thread, reuses
//!   cached complexity metadata from `preset_cache.json` whenever `(path, size, mtime)` matches,
//!   parses only new or modified `.milk` files, and streams non-blocking progress updates over
//!   an `mpsc::Receiver` to the UI thread.
//!
//! Rust concepts explained for Python developers:
//! - **`std::sync::mpsc::channel()`:** A thread-safe FIFO queue (`tx.send()` on the background
//!   scanner thread, `rx.try_recv()` on the UI thread) — like Python's `queue.Queue`.
//! - **`UNIX_EPOCH` mtime seconds:** Used with `file_size` to detect changed files without
//!   re-reading every `.milk` file on every launch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::playlists::{CollectionFilter, PlaylistStore};
use crate::preset_meta::{estimate_complexity_from_file, ComplexityTier, PresetComplexity};

/// Current JSON schema version for `preset_cache.json`.
pub const PRESET_CACHE_SCHEMA_VERSION: u32 = 1;

/// Estimated CPU/draw load filter shown in the Preset Browser toolbar (`Weight:` pills).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WeightFilter {
    #[default]
    All,
    Light,
    Medium,
    Heavy,
}

impl WeightFilter {
    pub fn matches(self, tier: ComplexityTier) -> bool {
        match self {
            Self::All => true,
            Self::Light => tier == ComplexityTier::Light,
            Self::Medium => tier == ComplexityTier::Medium,
            Self::Heavy => tier == ComplexityTier::Heavy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Light => "Light",
            Self::Medium => "Med",
            Self::Heavy => "Heavy",
        }
    }
}

/// One discovered `.milk` preset with its folder label and calibrated static complexity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetEntry {
    pub path: PathBuf,
    pub rel_path: String,
    pub display_name: String,
    pub folder: String,
    /// Pre-lowercased `"<name> <folder>"` string for sub-millisecond search across 10,000+ presets.
    pub search_haystack: String,
    pub complexity: PresetComplexity,
}

/// One cached preset record stored in `preset_cache.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPresetMeta {
    pub file_size: u64,
    pub modified_secs: u64,
    pub enabled_shape_instances: u32,
}

/// Top-level `preset_cache.json` structure on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetCacheFile {
    pub schema_version: u32,
    pub entries: BTreeMap<String, CachedPresetMeta>,
}

impl Default for PresetCacheFile {
    fn default() -> Self {
        Self {
            schema_version: PRESET_CACHE_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl PresetCacheFile {
    /// Loads `preset_cache.json` if present and valid; otherwise returns an empty cache.
    pub fn load_or_empty(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(cache) if cache.schema_version == PRESET_CACHE_SCHEMA_VERSION => cache,
            _ => Self::default(),
        }
    }

    /// Saves `preset_cache.json` atomically via a temporary file so interrupted writes never corrupt it.
    pub fn save_atomic(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create cache dir {}: {e}", parent.display()))?;
            }
        }
        // Compact JSON keeps a 10,000-preset cache 3x smaller and faster to parse on warm launch.
        let json = serde_json::to_string(self)
            .map_err(|e| format!("Failed to serialize preset cache: {e}"))?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &json)
            .map_err(|e| format!("Failed to write {}: {e}", tmp_path.display()))?;
        if let Err(rename_err) = fs::rename(&tmp_path, path) {
            // Fallback on Windows if destination is temporarily locked.
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
}

/// Progress or completion message sent from the background scanner thread to the UI thread.
#[derive(Debug, Clone)]
pub enum ScanUpdate {
    Progress {
        scanned: usize,
        total: usize,
        cache_hits: usize,
        parsed: usize,
    },
    Finished(ScanReport),
}

/// Final result produced when a background library scan finishes.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub entries: Vec<PresetEntry>,
    pub folders: Vec<String>,
    pub total: usize,
    pub cache_hits: usize,
    pub parsed: usize,
    pub elapsed_ms: f64,
    pub cache_saved: bool,
    pub error: Option<String>,
}

/// Current status of the Preset Library shown in the browser toolbar.
#[derive(Debug, Clone)]
pub struct ScanStatus {
    pub scanning: bool,
    pub scanned: usize,
    pub total: usize,
    pub cache_hits: usize,
    pub parsed: usize,
    pub elapsed_ms: f64,
    pub last_error: Option<String>,
}

impl Default for ScanStatus {
    fn default() -> Self {
        Self {
            scanning: false,
            scanned: 0,
            total: 0,
            cache_hits: 0,
            parsed: 0,
            elapsed_ms: 0.0,
            last_error: None,
        }
    }
}

/// Non-blocking handle to a running background preset scan thread.
pub struct PresetScannerHandle {
    rx: Receiver<ScanUpdate>,
}

impl PresetScannerHandle {
    /// Spawns a background thread (`preset-scanner`) that walks `root_dir` and updates `cache_path`.
    pub fn spawn(root_dir: PathBuf, cache_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("preset-scanner".to_string())
            .spawn(move || {
                let report = scan_presets_blocking(&root_dir, &cache_path, |scanned, total, hits, parsed| {
                    let _ = tx.send(ScanUpdate::Progress {
                        scanned,
                        total,
                        cache_hits: hits,
                        parsed,
                    });
                });
                let _ = tx.send(ScanUpdate::Finished(report));
            });
        Self { rx }
    }

    /// Drains all currently queued updates without blocking the render loop.
    pub fn poll_updates(&self) -> Vec<ScanUpdate> {
        let mut updates = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            updates.push(msg);
        }
        updates
    }
}

/// Discovers only the top-level starter `.milk` presets immediately so Deck A & Deck B can boot
/// with zero startup delay while the full library scans in the background.
pub fn discover_immediate_starter_presets(root_dir: &Path) -> Result<Vec<PresetEntry>, String> {
    let entries = fs::read_dir(root_dir)
        .map_err(|e| format!("Failed to read presets folder {}: {e}", root_dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|res| res.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_scannable_milk_file(p))
        .collect();
    paths.sort();

    if paths.is_empty() {
        return Err(format!("No starter .milk presets found in {}", root_dir.display()));
    }

    Ok(paths
        .into_iter()
        .map(|path| {
            let rel_path = normalize_rel_path(root_dir, &path);
            let display_name = file_stem_lossy(&path);
            let folder = derive_folder_label(&rel_path);
            let search_haystack = format!("{} {}", display_name.to_lowercase(), folder.to_lowercase());
            let complexity = estimate_complexity_from_file(&path);
            PresetEntry {
                path,
                rel_path,
                display_name,
                folder,
                search_haystack,
                complexity,
            }
        })
        .collect())
}

/// Performs a full recursive preset scan using `cache_path`, invoking `on_progress` periodically.
pub fn scan_presets_blocking<F>(root_dir: &Path, cache_path: &Path, mut on_progress: F) -> ScanReport
where
    F: FnMut(usize, usize, usize, usize),
{
    let t0 = Instant::now();
    let old_cache = PresetCacheFile::load_or_empty(cache_path);
    let cache_existed = cache_path.exists();

    let mut discovered: Vec<PathBuf> = Vec::new();
    if let Err(err) = collect_milk_files_recursive(root_dir, &mut discovered) {
        return ScanReport {
            entries: Vec::new(),
            folders: Vec::new(),
            total: 0,
            cache_hits: 0,
            parsed: 0,
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
            cache_saved: false,
            error: Some(err),
        };
    }

    let total = discovered.len();
    on_progress(0, total, 0, 0);

    let mut new_cache = PresetCacheFile::default();
    let mut checkpoint_cache = old_cache.clone();
    let mut entries = Vec::with_capacity(total);
    let mut folder_set = BTreeSet::new();
    let mut cache_hits = 0usize;
    let mut parsed = 0usize;

    for (idx, path) in discovered.into_iter().enumerate() {
        let rel_path = normalize_rel_path(root_dir, &path);
        let display_name = file_stem_lossy(&path);
        let folder = derive_folder_label(&rel_path);

        let (file_size, modified_secs) = file_fingerprint(&path);
        let mut was_miss = false;
        let complexity = match old_cache.entries.get(&rel_path) {
            Some(cached)
                if cached.file_size == file_size && cached.modified_secs == modified_secs =>
            {
                cache_hits += 1;
                PresetComplexity {
                    enabled_shape_instances: cached.enabled_shape_instances,
                    tier: ComplexityTier::from_enabled_instances(cached.enabled_shape_instances),
                }
            }
            _ => {
                parsed += 1;
                was_miss = true;
                estimate_complexity_from_file(&path)
            }
        };

        let meta = CachedPresetMeta {
            file_size,
            modified_secs,
            enabled_shape_instances: complexity.enabled_shape_instances,
        };
        new_cache.entries.insert(rel_path.clone(), meta.clone());
        if was_miss {
            checkpoint_cache.entries.insert(rel_path.clone(), meta);
            // Checkpoint every 1,000 newly parsed presets so closing early during a large
            // first-time cold scan never loses cache progress.
            if parsed % 1000 == 0 {
                let _ = checkpoint_cache.save_atomic(cache_path);
            }
        }
        folder_set.insert(folder.clone());
        let search_haystack = format!("{} {}", display_name.to_lowercase(), folder.to_lowercase());
        entries.push(PresetEntry {
            path,
            rel_path,
            display_name,
            folder,
            search_haystack,
            complexity,
        });

        let scanned = idx + 1;
        if scanned % 128 == 0 || scanned == total {
            on_progress(scanned, total, cache_hits, parsed);
        }
    }

    // Deterministic sorting: Starter presets first, then folders alphabetically, then preset name.
    entries.sort_by(|a, b| {
        let a_starter = a.folder == "Starter";
        let b_starter = b.folder == "Starter";
        b_starter
            .cmp(&a_starter)
            .then_with(|| a.folder.to_lowercase().cmp(&b.folder.to_lowercase()))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
    });

    let mut folders: Vec<String> = folder_set.into_iter().collect();
    folders.sort_by(|a, b| {
        let a_starter = a == "Starter";
        let b_starter = b == "Starter";
        b_starter
            .cmp(&a_starter)
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });

    let cache_changed = !cache_existed || new_cache.entries != old_cache.entries;
    let mut save_err = None;
    let mut cache_saved = false;
    if cache_changed {
        match new_cache.save_atomic(cache_path) {
            Ok(()) => cache_saved = true,
            Err(e) => save_err = Some(e),
        }
    }

    ScanReport {
        entries,
        folders,
        total,
        cache_hits,
        parsed,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        cache_saved,
        error: save_err,
    }
}

/// Filters `entries` by collection (`All`, `Favorites`, or a named playlist in its curated order),
/// case-insensitive multi-term search query, `WeightFilter`, and optional folder.
/// Returns matching indices into `entries`.
pub fn filter_preset_indices(
    entries: &[PresetEntry],
    search_query: &str,
    weight_filter: WeightFilter,
    folder_filter: Option<&str>,
    collection_filter: &CollectionFilter,
    playlists: &PlaylistStore,
) -> Vec<usize> {
    let terms: Vec<String> = search_query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();

    let matches_common = |entry: &PresetEntry| -> bool {
        if !weight_filter.matches(entry.complexity.tier) {
            return false;
        }
        if let Some(wanted_folder) = folder_filter {
            if entry.folder != wanted_folder {
                return false;
            }
        }
        for term in &terms {
            if !entry.search_haystack.contains(term) {
                return false;
            }
        }
        true
    };

    match collection_filter {
        CollectionFilter::All => entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| matches_common(entry).then_some(idx))
            .collect(),
        CollectionFilter::Favorites => entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                (playlists.is_favorite(&entry.rel_path) && matches_common(entry)).then_some(idx)
            })
            .collect(),
        CollectionFilter::Playlist(playlist_name) => {
            let Some(pl) = playlists.get_playlist(playlist_name) else {
                return Vec::new();
            };
            // Preserve the user's exact curated playlist order!
            let by_rel: HashMap<&str, usize> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.rel_path.as_str(), i))
                .collect();
            pl.presets
                .iter()
                .filter_map(|rel| {
                    let &idx = by_rel.get(rel.as_str())?;
                    matches_common(&entries[idx]).then_some(idx)
                })
                .collect()
        }
    }
}

fn collect_milk_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let rd = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read folder {}: {e}", dir.display()))?;
    let mut children: Vec<PathBuf> = rd.filter_map(|res| res.ok().map(|e| e.path())).collect();
    children.sort();

    for path in children {
        if path.is_dir() {
            // Skip hidden folders such as `.git`.
            let is_hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false);
            if !is_hidden {
                collect_milk_files_recursive(&path, out)?;
            }
        } else if is_scannable_milk_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_scannable_milk_file(path: &Path) -> bool {
    let is_milk = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("milk"))
        .unwrap_or(false);
    let is_hidden_test = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('_'))
        .unwrap_or(false);
    is_milk && !is_hidden_test
}

fn normalize_rel_path(root: &Path, full: &Path) -> String {
    let rel = full.strip_prefix(root).unwrap_or(full);
    rel.to_string_lossy().replace('\\', "/")
}

fn file_stem_lossy(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unnamed>")
        .to_string()
}

fn derive_folder_label(rel_path: &str) -> String {
    let Some((dir_part, _file_name)) = rel_path.rsplit_once('/') else {
        return "Starter".to_string();
    };
    let trimmed = dir_part.strip_prefix("library/").unwrap_or(dir_part);
    if trimmed.is_empty() {
        "Library".to_string()
    } else {
        trimmed.to_string()
    }
}

fn file_fingerprint(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::metadata(path) else {
        return (0, 0);
    };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (size, mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn make_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("vj_browser_test_{tag}_{nanos}"));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn scanner_caches_complexity_and_skips_underscore_files() {
        let root = make_temp_dir("cache");
        let cache_path = root.join("preset_cache.json");
        let presets_dir = root.join("presets");
        let fractal_dir = presets_dir.join("library").join("Fractal").join("Trees");
        fs::create_dir_all(&fractal_dir).unwrap();

        // Starter light preset
        fs::write(
            presets_dir.join("01_Starter_Glow.milk"),
            "shapecode_0_enabled=1\nshapecode_0_num_inst=16\n",
        )
        .unwrap();
        // Hidden test file (must be skipped)
        fs::write(presets_dir.join("_bad_shader_test.milk"), "broken").unwrap();
        // Library heavy preset
        fs::write(
            fractal_dir.join("Forest_Heavy.milk"),
            "shapecode_0_enabled=1\nshapecode_0_num_inst=900\n",
        )
        .unwrap();
        // Library medium preset
        fs::write(
            fractal_dir.join("Canopy_Med.milk"),
            "shapecode_0_enabled=1\nshapecode_0_num_inst=250\n",
        )
        .unwrap();

        // Cold scan: 0 hits, 3 parsed, cache saved.
        let cold = scan_presets_blocking(&presets_dir, &cache_path, |_, _, _, _| {});
        assert!(cold.error.is_none());
        assert_eq!(cold.total, 3);
        assert_eq!(cold.cache_hits, 0);
        assert_eq!(cold.parsed, 3);
        assert!(cold.cache_saved);
        assert!(cache_path.exists());
        assert_eq!(cold.folders, vec!["Starter".to_string(), "Fractal/Trees".to_string()]);
        assert_eq!(cold.entries[0].folder, "Starter");

        // Warm scan: 3 hits, 0 parsed, cache not rewritten unnecessarily.
        let warm = scan_presets_blocking(&presets_dir, &cache_path, |_, _, _, _| {});
        assert!(warm.error.is_none());
        assert_eq!(warm.total, 3);
        assert_eq!(warm.cache_hits, 3);
        assert_eq!(warm.parsed, 0);
        assert!(!warm.cache_saved);

        // Filter tests: search + weight + folder + favorites + playlist curated order.
        let mut playlists = PlaylistStore::default();
        let heavy_only = filter_preset_indices(
            &warm.entries,
            "",
            WeightFilter::Heavy,
            None,
            &CollectionFilter::All,
            &playlists,
        );
        assert_eq!(heavy_only.len(), 1);
        assert_eq!(warm.entries[heavy_only[0]].display_name, "Forest_Heavy");

        let search_multi = filter_preset_indices(
            &warm.entries,
            "fractal canopy",
            WeightFilter::All,
            None,
            &CollectionFilter::All,
            &playlists,
        );
        assert_eq!(search_multi.len(), 1);
        assert_eq!(warm.entries[search_multi[0]].display_name, "Canopy_Med");

        let folder_starter = filter_preset_indices(
            &warm.entries,
            "",
            WeightFilter::All,
            Some("Starter"),
            &CollectionFilter::All,
            &playlists,
        );
        assert_eq!(folder_starter.len(), 1);
        assert_eq!(warm.entries[folder_starter[0]].display_name, "01_Starter_Glow");

        // Favorites filter & playlist order preservation.
        let heavy_rel = warm.entries[heavy_only[0]].rel_path.clone();
        let starter_rel = warm.entries[folder_starter[0]].rel_path.clone();
        playlists.toggle_favorite(&heavy_rel);
        let favs = filter_preset_indices(
            &warm.entries,
            "",
            WeightFilter::All,
            None,
            &CollectionFilter::Favorites,
            &playlists,
        );
        assert_eq!(favs.len(), 1);
        assert_eq!(warm.entries[favs[0]].display_name, "Forest_Heavy");

        playlists.create_playlist("Set 1").unwrap();
        // Add heavy first, then starter -> playlist view must preserve [Forest_Heavy, 01_Starter_Glow]!
        playlists.toggle_in_playlist("Set 1", &heavy_rel);
        playlists.toggle_in_playlist("Set 1", &starter_rel);
        let pl_view = filter_preset_indices(
            &warm.entries,
            "",
            WeightFilter::All,
            None,
            &CollectionFilter::Playlist("Set 1".to_string()),
            &playlists,
        );
        assert_eq!(pl_view.len(), 2);
        assert_eq!(warm.entries[pl_view[0]].display_name, "Forest_Heavy");
        assert_eq!(warm.entries[pl_view[1]].display_name, "01_Starter_Glow");

        let _ = fs::remove_dir_all(&root);
    }
}
