//! Shared library modules for `milkdrop-vj`.
//!
//! Putting reusable modules (`audio`, `deck`, `test_audio`) in `src/lib.rs` keeps
//! our C library links (`projectM-4`, `glew32`) in one place and lets both the main
//! visualizer (`src/main.rs`) and console tools (`src/bin/audio_test.rs`) share code.

pub mod audio;
pub mod control;
pub mod deck;
pub mod hot_banks;
pub mod playlists;
pub mod preset_browser;
pub mod preset_meta;
pub mod test_audio;
