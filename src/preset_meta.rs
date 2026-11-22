//! Static `.milk` preset complexity estimation (calibrated from controlled laptop runs).
//!
//! Verified directly in `libprojectM 4.1.8` (`CustomShape.cpp` lines 80–133):
//! - Disabled shapes (`shapecode_N_enabled=0`) return immediately and perform zero draw work.
//! - Enabled shapes (`shapecode_N_enabled=1`) loop `shapecode_N_num_inst` times per frame.
//!
//! Thresholds calibrated from our measured Dell Precision 7740 runs (provisional, laptop-limited):
//! - **Light (`0..=100` enabled shape instances):** Fast presets (`2–8 ms`, locked `60 FPS`).
//! - **Medium (`101..=500`):** Solo under `16.67 ms` (`5–9 ms`), watch budget if paired.
//! - **Heavy (`> 500`):** Likely to exceed `16.67 ms` on one deck alone (`22–26 ms`).
//!
//! Always labeled in the UI as an **estimated CPU/draw load**, not a guarantee.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Calibrated estimated CPU/draw load tier for a `.milk` preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplexityTier {
    Light,
    Medium,
    Heavy,
}

impl ComplexityTier {
    pub fn from_enabled_instances(enabled_num_inst: u32) -> Self {
        if enabled_num_inst <= 100 {
            Self::Light
        } else if enabled_num_inst <= 500 {
            Self::Medium
        } else {
            Self::Heavy
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "LIGHT",
            Self::Medium => "MEDIUM",
            Self::Heavy => "HEAVY",
        }
    }
}

/// Static complexity summary parsed from a `.milk` text file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetComplexity {
    pub enabled_shape_instances: u32,
    pub tier: ComplexityTier,
}

/// Parses `.milk` text and sums `shapecode_N_num_inst` across shapes where `shapecode_N_enabled=1`.
pub fn estimate_complexity_from_text(contents: &str) -> PresetComplexity {
    let mut enabled: HashMap<u32, bool> = HashMap::new();
    let mut num_inst: HashMap<u32, u32> = HashMap::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("shapecode_") else {
            continue;
        };
        let Some((idx_str, suffix)) = rest.split_once('_') else {
            continue;
        };
        let Ok(shape_idx) = idx_str.parse::<u32>() else {
            continue;
        };

        if let Some(val_str) = suffix.strip_prefix("enabled=") {
            let is_on = val_str.trim() == "1" || val_str.trim().eq_ignore_ascii_case("true");
            enabled.insert(shape_idx, is_on);
        } else if let Some(val_str) = suffix.strip_prefix("num_inst=") {
            // MilkDrop presets usually write integer values, occasionally floats like `64.0`.
            if let Ok(v) = val_str.trim().parse::<f64>() {
                num_inst.insert(shape_idx, v.max(0.0).round() as u32);
            }
        }
    }

    let mut total_enabled = 0u32;
    for (idx, is_on) in enabled {
        if is_on {
            // In libprojectM CustomShape.hpp, default m_instances is 1 when enabled=1 and num_inst is omitted.
            let count = num_inst.get(&idx).copied().unwrap_or(1);
            total_enabled = total_enabled.saturating_add(count);
        }
    }

    PresetComplexity {
        enabled_shape_instances: total_enabled,
        tier: ComplexityTier::from_enabled_instances(total_enabled),
    }
}

/// Reads a `.milk` file from disk and estimates its static complexity.
pub fn estimate_complexity_from_file(path: &Path) -> PresetComplexity {
    match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            estimate_complexity_from_text(&text)
        }
        Err(_) => PresetComplexity {
            enabled_shape_instances: 0,
            tier: ComplexityTier::Light,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_shapes_are_excluded_from_complexity_estimate() {
        let sample = "\
shapecode_0_enabled=1
shapecode_0_num_inst=78
shapecode_1_enabled=0
shapecode_1_num_inst=1024
shapecode_2_enabled=1
shapecode_2_num_inst=200
";
        let est = estimate_complexity_from_text(sample);
        assert_eq!(est.enabled_shape_instances, 278);
        assert_eq!(est.tier, ComplexityTier::Medium);
    }

    #[test]
    fn tier_thresholds_match_calibrated_ranges() {
        assert_eq!(ComplexityTier::from_enabled_instances(0), ComplexityTier::Light);
        assert_eq!(ComplexityTier::from_enabled_instances(100), ComplexityTier::Light);
        assert_eq!(ComplexityTier::from_enabled_instances(101), ComplexityTier::Medium);
        assert_eq!(ComplexityTier::from_enabled_instances(500), ComplexityTier::Medium);
        assert_eq!(ComplexityTier::from_enabled_instances(501), ComplexityTier::Heavy);
        assert_eq!(ComplexityTier::from_enabled_instances(3584), ComplexityTier::Heavy);
    }
}
