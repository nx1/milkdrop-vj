//! Dark-Venue Traktor-Style `egui 0.37` Console UI (Milestone 2 — Stage `2A-4`).
//!
//! Layout specification: Section 13 of `docs/project_research_and_design.md`.
//! - All sizes are in `egui` logical points (`pt`), automatically scaled by Windows DPI.
//! - Design size: `1920x1080 pt`; minimum supported window size: `1280x720 pt`.
//! - Preview sizes follow available stage height (`(window_h * 0.40).clamp(306, 432)`)
//!   keeping exact `16:9` aspect ratio, and the Preset Browser fills all remaining height.
//! - Every working control (crossfader, cuts, blend modes, blackout, deck load buttons,
//!   browser row Load A/B buttons, transition mode, and remappable keyboard bindings)
//!   reads and writes through the unified `ControlBus` (`0.0..=1.0`) and `mappings.json`.
//! - Controls arriving in `2A-5` and `2B` remain greyed out until implemented.

use std::path::Path;
use std::time::{Duration, Instant};

use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, ScrollArea,
    Stroke, TextStyle, TextureId, Vec2,
};

use crate::mixer::{BlendMode, DeckAirState};
use crate::render_deck::BlankDeckStatus;
use milkdrop_vj::control::{param, ControlBus, MappingMode, BLEND_MODE_COUNT};
use milkdrop_vj::preset_browser::{PresetEntry, ScanStatus, WeightFilter};
use milkdrop_vj::preset_meta::{ComplexityTier, PresetComplexity};

/// Thresholds for per-deck rolling GPU render time (`ms`) warnings (provisional, laptop-limited).
pub const DECK_MS_WARN: f64 = 6.0;
pub const DECK_MS_ALERT: f64 = 10.0;

/// Thresholds for highlighting the Status Strip's `Last load` stall readout (Risk 8).
pub const LOAD_STALL_WARN_MS: f64 = 16.7;
pub const LOAD_STALL_ALERT_MS: f64 = 33.3;

/// Seconds a protected on-air/mixed load button stays armed (`[CONFIRM?]`) before disarming.
const LOAD_CONFIRM_WINDOW: Duration = Duration::from_secs(3);
/// Duration for inline status notices (such as Load Safety guard messages).
const TOAST_DURATION: Duration = Duration::from_millis(3500);

// High-contrast dark-venue palette.
const BG_WINDOW: Color32 = Color32::from_rgb(14, 16, 20); // #0E1014
const BG_PANEL: Color32 = Color32::from_rgb(23, 27, 34); // #171B22
const BG_CARD: Color32 = Color32::from_rgb(28, 33, 42); // #1C212A
const BORDER_SUBTLE: Color32 = Color32::from_rgb(52, 60, 74);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(242, 244, 248);
const TEXT_MUTED: Color32 = Color32::from_rgb(156, 166, 184);

const ACCENT_DECK_A: Color32 = Color32::from_rgb(34, 211, 238); // Cyan
const ACCENT_DECK_B: Color32 = Color32::from_rgb(245, 158, 11); // Amber
const COLOR_OK: Color32 = Color32::from_rgb(52, 211, 153); // Green
const COLOR_WARN: Color32 = Color32::from_rgb(250, 204, 21); // Yellow
const COLOR_ALERT: Color32 = Color32::from_rgb(239, 68, 68); // Red

/// Tracks two-step `[CONFIRM?]` arming for protected on-air/mixed load buttons (Load Safety).
#[derive(Default)]
pub struct LoadGuardState {
    armed_target: Option<String>,
    armed_at: Option<Instant>,
}

impl LoadGuardState {
    pub fn expire_if_needed(&mut self) {
        if let Some(t) = self.armed_at {
            if t.elapsed() >= LOAD_CONFIRM_WINDOW {
                self.armed_target = None;
                self.armed_at = None;
            }
        }
    }

    pub fn is_armed(&self, target: &str) -> bool {
        self.armed_target.as_deref() == Some(target)
            && self
                .armed_at
                .map(|t| t.elapsed() < LOAD_CONFIRM_WINDOW)
                .unwrap_or(false)
    }

    pub fn click_load(&mut self, target: &str, air_state: DeckAirState) -> bool {
        if !air_state.is_protected() {
            self.disarm();
            return true;
        }
        if self.is_armed(target) {
            self.disarm();
            true
        } else {
            self.armed_target = Some(target.to_string());
            self.armed_at = Some(Instant::now());
            false
        }
    }

    pub fn disarm(&mut self) {
        self.armed_target = None;
        self.armed_at = None;
    }
}

/// Interactive focus, selection, filter, and key-remapping modal state shared between `main.rs` and `ui.rs`.
#[derive(Default)]
pub struct UiInteractionState {
    /// Highlighted row index inside the currently filtered Preset Browser list (`Up`/`Down` or click).
    pub selected_preset_idx: usize,
    /// True after Up/Down moved selection so the virtualized scroll area scrolls the row into view.
    pub scroll_to_selected: bool,
    /// Live search query text (case-insensitive multi-term match against preset name and folder).
    pub search_query: String,
    /// Active estimated-load (`Weight:`) filter pill (`All`, `Light`, `Med`, `Heavy`).
    pub weight_filter: WeightFilter,
    /// Optional folder filter (`None` = All folders).
    pub folder_filter: Option<String>,
    /// Set when the user clicks `Rescan` in the Preset Browser toolbar.
    pub request_rescan: bool,
    /// True while the Search `TextEdit` has keyboard focus (pauses performance shortcuts).
    pub search_focused: bool,
    /// Set by `/` key to focus the Search box on the next UI frame.
    pub request_focus_search: bool,
    /// Set by `Esc` or `Enter` in Typing Mode to return focus cleanly to performance keys.
    pub request_blur_search: bool,
    /// True while the `Keyboard Mappings (mappings.json)` modal is open.
    pub keys_modal_open: bool,
    /// Index in `bus.mappings.keyboard` currently capturing a replacement key press.
    pub remapping_idx: Option<usize>,
    /// Status text shown inside the Keys modal (save confirmation or conflict warning).
    pub remap_feedback: Option<String>,
    /// Set when the user clicks `Reset Defaults` in the Keys modal.
    pub request_reset_defaults: bool,
    /// Temporary inline warning/notice shown in the Status Strip (e.g. Load Safety messages).
    toast: Option<(String, Instant)>,
}

impl UiInteractionState {
    /// Returns `true` whenever live performance keyboard shortcuts must be paused.
    #[allow(dead_code)]
    pub fn performance_keys_paused(&self) -> bool {
        self.search_focused || self.keys_modal_open
    }

    pub fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn active_toast(&self) -> Option<&str> {
        let (msg, at) = self.toast.as_ref()?;
        if at.elapsed() < TOAST_DURATION {
            Some(msg.as_str())
        } else {
            None
        }
    }
}

/// High-level audio link state shown prominently in the Top Bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBadgeState {
    Live,
    Test,
    Silence,
    DeviceLost,
}

/// Summary of the most recent preset load for the Status Strip.
#[derive(Debug, Clone)]
pub struct LastLoadSummary {
    pub deck_label: String,
    pub preset_name: String,
    pub load_ms: f64,
    pub error: Option<String>,
}

/// Read-only per-deck snapshot passed into `draw_console_ui()` each frame.
pub struct DeckUiSnapshot<'a> {
    pub texture_id: TextureId,
    pub preset_name: &'a str,
    pub preset_path: &'a Path,
    pub complexity: PresetComplexity,
    pub render_ms_avg: f64,
    pub blank: BlankDeckStatus,
}

/// All live readouts needed to draw one frame of the `2A-4` Control Console.
pub struct ConsoleUiState<'a> {
    pub audio_source_label: &'a str,
    pub audio_device_name: &'a str,
    pub audio_sample_rate: u32,
    pub audio_peak_db: f32,
    pub audio_state: AudioBadgeState,
    pub deck_res_label: &'a str,
    pub gpu_label: &'a str,
    pub gpu_is_intel: bool,
    pub global_fps: f64,
    pub gl_errors: u64,
    pub blank_check_avg_us: f64,
    pub mappings_status: &'a str,
    pub last_load: Option<&'a LastLoadSummary>,
    pub master_texture_id: TextureId,
    pub blackout_alpha: f32,
    pub deck_a: DeckUiSnapshot<'a>,
    pub deck_b: DeckUiSnapshot<'a>,
    pub all_presets: &'a [PresetEntry],
    pub filtered_indices: &'a [usize],
    pub folders: &'a [String],
    pub scan_status: &'a ScanStatus,
}

/// Applies the high-contrast dark-venue palette and larger readable typography once at startup.
pub fn apply_dark_venue_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = BG_WINDOW;
    visuals.panel_fill = BG_PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(10, 12, 16);
    visuals.faint_bg_color = BG_CARD;
    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 45, 58);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(52, 62, 80);
    visuals.widgets.active.bg_fill = Color32::from_rgb(66, 80, 102);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.global_style()).clone();
    style.text_styles.insert(TextStyle::Body, FontId::proportional(15.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.5));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(20.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(13.0));
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    ctx.set_global_style(style);
}

/// Draws the entire Milestone 2 (`2A-3`) console inside `ui` (`egui 0.37` root `Ui`).
pub fn draw_console_ui(
    ui: &mut egui::Ui,
    state: &ConsoleUiState<'_>,
    bus: &mut ControlBus,
    load_guard: &mut LoadGuardState,
    interact: &mut UiInteractionState,
) {
    load_guard.expire_if_needed();

    let window_h = ui.ctx().content_rect().height().max(720.0);
    let stage_h = (window_h * 0.40).clamp(306.0, 432.0);
    let shift_held = ui.input(|i| i.modifiers.shift);

    // Derive ON-AIR / MIXED / CUE live states strictly from Control Bus crossfader position.
    let xfade = bus.get(param::MIXER_CROSSFADER);
    let air_a = DeckAirState::from_weight(1.0 - xfade);
    let air_b = DeckAirState::from_weight(xfade);

    // 1. TOP BAR — fixed 56 pt height.
    egui::Panel::top("top_bar")
        .exact_size(56.0)
        .frame(
            Frame::new()
                .fill(BG_PANEL)
                .inner_margin(Margin::symmetric(12, 8))
                .stroke(Stroke::new(1.0, BORDER_SUBTLE)),
        )
        .show(ui, |ui| {
            draw_top_bar(ui, state, interact);
        });

    // 2. BOTTOM STATUS STRIP — fixed 28 pt height.
    egui::Panel::bottom("status_strip")
        .exact_size(28.0)
        .frame(
            Frame::new()
                .fill(BG_WINDOW)
                .inner_margin(Margin::symmetric(12, 4))
                .stroke(Stroke::new(1.0, BORDER_SUBTLE)),
        )
        .show(ui, |ui| {
            draw_status_strip(ui, state, interact);
        });

    // 3. VISUAL STAGE — 3 columns (Deck A | Live Master Mixer | Deck B).
    egui::Panel::top("visual_stage")
        .exact_size(stage_h)
        .frame(
            Frame::new()
                .fill(BG_WINDOW)
                .inner_margin(Margin::symmetric(10, 6)),
        )
        .show(ui, |ui| {
            let preview_h_from_stage = (stage_h - 126.0).max(140.0);
            ui.columns(3, |cols| {
                draw_deck_column(
                    &mut cols[0],
                    "DECK A",
                    ACCENT_DECK_A,
                    air_a,
                    &state.deck_a,
                    preview_h_from_stage,
                    bus,
                    load_guard,
                    param::DECK_A_PREV_PRESET,
                    param::DECK_A_NEXT_PRESET,
                    param::DECK_A_TRANSITION_SMOOTH,
                    Some(param::DECK_A_TEST_BLANK),
                    shift_held,
                );
                draw_master_mixer_column(
                    &mut cols[1],
                    state.master_texture_id,
                    state.blackout_alpha,
                    preview_h_from_stage,
                    bus,
                );
                draw_deck_column(
                    &mut cols[2],
                    "DECK B",
                    ACCENT_DECK_B,
                    air_b,
                    &state.deck_b,
                    preview_h_from_stage,
                    bus,
                    load_guard,
                    param::DECK_B_PREV_PRESET,
                    param::DECK_B_NEXT_PRESET,
                    param::DECK_B_TRANSITION_SMOOTH,
                    None,
                    shift_held,
                );
            });
        });

    // 4. CONDITIONAL 30 pt AMBER TYPING BANNER — shown ONLY while Search box has focus.
    if interact.search_focused {
        egui::Panel::top("typing_mode_banner")
            .exact_size(30.0)
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(180, 110, 12))
                    .inner_margin(Margin::symmetric(12, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new(
                            "KEYS PAUSED — TYPING IN SEARCH   (Press Esc to resume live keys; Enter exits search first; Up/Down still select list rows)",
                        )
                        .font(FontId::proportional(14.5))
                        .strong()
                        .color(Color32::from_rgb(16, 16, 16)),
                    );
                });
            });
    }

    // 5. PRESET BROWSER — CentralPanel fills 100% of remaining vertical space.
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(BG_PANEL)
                .inner_margin(Margin::symmetric(12, 8))
                .stroke(Stroke::new(1.0, BORDER_SUBTLE)),
        )
        .show(ui, |ui| {
            draw_browser_panel(
                ui,
                state,
                air_a,
                air_b,
                shift_held,
                bus,
                load_guard,
                interact,
            );
        });

    // 6. IN-APP REMAPPABLE KEYS MODAL (`mappings.json`).
    if interact.keys_modal_open {
        draw_keys_modal(ui.ctx(), bus, interact);
    }
}

fn draw_top_bar(
    ui: &mut egui::Ui,
    state: &ConsoleUiState<'_>,
    interact: &mut UiInteractionState,
) {
    ui.horizontal_centered(|ui| {
        ui.add_enabled(
            false,
            egui::Button::new(format!("{} v (2A-5)", state.audio_source_label)),
        );

        let dev_short = truncate_chars(state.audio_device_name, 26);
        ui.add_enabled(false, egui::Button::new(format!("{dev_short} v (2A-5)")));

        draw_peak_meter(ui, state.audio_peak_db);
        draw_audio_status_pill(ui, state.audio_state);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (fps_bg, fps_fg) = if state.global_fps >= 55.0 {
                (Color32::from_rgb(16, 62, 42), COLOR_OK)
            } else if state.global_fps >= 45.0 {
                (Color32::from_rgb(76, 58, 12), COLOR_WARN)
            } else {
                (Color32::from_rgb(120, 24, 24), Color32::WHITE)
            };
            Frame::new()
                .fill(fps_bg)
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(10, 4))
                .stroke(Stroke::new(1.0, fps_fg))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{:5.1} FPS", state.global_fps))
                            .font(FontId::monospace(20.0))
                            .strong()
                            .color(fps_fg),
                    );
                });

            let (gpu_bg, gpu_fg) = if state.gpu_is_intel {
                (COLOR_ALERT, Color32::WHITE)
            } else {
                (BG_CARD, COLOR_OK)
            };
            Frame::new()
                .fill(gpu_bg)
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::symmetric(8, 4))
                .stroke(Stroke::new(1.0, gpu_fg))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(state.gpu_label)
                            .font(FontId::proportional(14.0))
                            .strong()
                            .color(gpu_fg),
                    );
                });

            // LIVE Stage 2A-3 Keys remapping modal toggle.
            let keys_btn = if interact.keys_modal_open {
                egui::Button::new(RichText::new("Keys").strong().color(Color32::BLACK))
                    .fill(ACCENT_DECK_A)
            } else {
                egui::Button::new("Keys")
            };
            if ui
                .add(keys_btn)
                .on_hover_text("View and remap keyboard shortcuts (saved to mappings.json)")
                .clicked()
            {
                interact.keys_modal_open = !interact.keys_modal_open;
                interact.remapping_idx = None;
            }

            ui.add_enabled(false, egui::Button::new("Proj: OFF (2B)"));
            ui.add_enabled(
                false,
                egui::Button::new(format!("Res: {} (2A-5)", state.deck_res_label)),
            );
        });
    });
}

fn draw_peak_meter(ui: &mut egui::Ui, peak_db: f32) {
    let desired = Vec2::new(96.0, 22.0);
    let (rect, _resp) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Color32::from_rgb(16, 20, 26));
    painter.rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, BORDER_SUBTLE),
        egui::StrokeKind::Inside,
    );

    let norm = ((peak_db + 60.0) / 60.0).clamp(0.0, 1.0);
    if norm > 0.01 {
        let fill_w = (rect.width() - 4.0) * norm;
        let fill_rect = Rect::from_min_size(
            egui::pos2(rect.min.x + 2.0, rect.min.y + 2.0),
            Vec2::new(fill_w, rect.height() - 4.0),
        );
        let color = if peak_db > -3.0 {
            COLOR_ALERT
        } else if peak_db > -12.0 {
            COLOR_WARN
        } else {
            COLOR_OK
        };
        painter.rect_filled(fill_rect, 2.0, color);
    }

    let db_text = if peak_db <= -90.0 {
        "-inf".to_string()
    } else {
        format!("{peak_db:+.0}dB")
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        db_text,
        FontId::monospace(12.0),
        Color32::WHITE,
    );
}

fn draw_audio_status_pill(ui: &mut egui::Ui, status: AudioBadgeState) {
    let (label, bg, fg, font_size) = match status {
        AudioBadgeState::Live => ("LIVE", Color32::from_rgb(16, 70, 44), COLOR_OK, 16.0),
        AudioBadgeState::Test => ("TEST", Color32::from_rgb(14, 60, 74), ACCENT_DECK_A, 16.0),
        AudioBadgeState::Silence => ("SILENCE", Color32::from_rgb(88, 62, 12), COLOR_WARN, 16.0),
        AudioBadgeState::DeviceLost => ("! DEVICE LOST !", COLOR_ALERT, Color32::WHITE, 18.0),
    };
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(10, 4))
        .stroke(Stroke::new(1.5, fg))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .font(FontId::proportional(font_size))
                    .strong()
                    .color(fg),
            );
        });
}

fn draw_air_badge(ui: &mut egui::Ui, air_state: DeckAirState, deck_accent: Color32) {
    let (bg, fg) = match air_state {
        DeckAirState::OnAir => (deck_accent.gamma_multiply(0.32), deck_accent),
        DeckAirState::Mixed => (Color32::from_rgb(88, 62, 12), COLOR_WARN),
        DeckAirState::Cue => (Color32::from_rgb(36, 42, 54), TEXT_MUTED),
    };
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(7, 2))
        .stroke(Stroke::new(1.0, fg))
        .show(ui, |ui| {
            ui.label(
                RichText::new(air_state.label())
                    .font(FontId::monospace(12.5))
                    .strong()
                    .color(fg),
            );
        });
}

#[allow(clippy::too_many_arguments)]
fn draw_deck_column(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    air_state: DeckAirState,
    snap: &DeckUiSnapshot<'_>,
    preview_h_from_stage: f32,
    bus: &mut ControlBus,
    load_guard: &mut LoadGuardState,
    prev_target: &str,
    next_target: &str,
    smooth_param: &str,
    test_blank_target: Option<&str>,
    shift_held: bool,
) {
    let border_color = match air_state {
        DeckAirState::OnAir => accent,
        DeckAirState::Mixed => COLOR_WARN.gamma_multiply(0.8),
        DeckAirState::Cue => BORDER_SUBTLE,
    };

    Frame::new()
        .fill(BG_CARD)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(8))
        .stroke(Stroke::new(1.5, border_color))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(FontId::proportional(18.5))
                        .strong()
                        .color(accent),
                );
                draw_air_badge(ui, air_state, accent);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (ms_color, tag) = if snap.render_ms_avg >= DECK_MS_ALERT {
                        (COLOR_ALERT, " OVER")
                    } else if snap.render_ms_avg >= DECK_MS_WARN {
                        (COLOR_WARN, " SLOW")
                    } else {
                        (TEXT_PRIMARY, "")
                    };
                    ui.label(
                        RichText::new(format!("{:4.1} ms{tag}", snap.render_ms_avg))
                            .font(FontId::monospace(15.0))
                            .strong()
                            .color(ms_color),
                    )
                    .on_hover_text(
                        "Deck GPU render time (non-blocking GL_TIME_ELAPSED, rolling avg).\n\
                         Green/White < 6.0 ms | Yellow 6.0-9.9 ms (SLOW) | Red >= 10.0 ms (OVER BUDGET).",
                    );
                });
            });

            let col_inner_w = ui.available_width().max(160.0);
            let preview_h = preview_h_from_stage.min(col_inner_w * 9.0 / 16.0);
            let preview_w = preview_h * 16.0 / 9.0;
            let preview_size = Vec2::new(preview_w, preview_h);

            ui.vertical_centered(|ui| {
                let img = egui::Image::new((snap.texture_id, preview_size))
                    .uv(Rect::from_min_max(egui::pos2(0.0, 1.0), egui::pos2(1.0, 0.0)));
                ui.add(img);
            });

            ui.horizontal(|ui| {
                let (badge_bg, badge_fg) = complexity_colors(snap.complexity.tier);
                Frame::new()
                    .fill(badge_bg)
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(6, 2))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} ({})",
                                snap.complexity.tier.label(),
                                snap.complexity.enabled_shape_instances
                            ))
                            .font(FontId::monospace(12.0))
                            .strong()
                            .color(badge_fg),
                        )
                        .on_hover_text(
                            "Estimated CPU/draw load (sum of enabled shapecode num_inst).\n\
                             Light <= 100 | Medium 101-500 | Heavy > 500.\n\
                             Estimate only, not a guarantee.",
                        );
                    });

                let short_name = truncate_chars(snap.preset_name, 32);
                ui.label(
                    RichText::new(short_name)
                        .font(FontId::proportional(14.0))
                        .color(TEXT_PRIMARY),
                )
                .on_hover_text(snap.preset_name);
            });

            if snap.blank.is_blank_warning {
                Frame::new()
                    .fill(COLOR_ALERT)
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(8, 2))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "! BLANK DECK (luma {:.1}/255 for {:.1}s)",
                                snap.blank.avg_luma, snap.blank.dark_seconds
                            ))
                            .font(FontId::proportional(13.0))
                            .strong()
                            .color(Color32::WHITE),
                        );
                    });
            } else {
                let luma_text = if snap.blank.has_sample {
                    format!("Luma: {:.0}/255 (ok)", snap.blank.avg_luma)
                } else {
                    "Luma: sampling...".to_string()
                };
                ui.label(
                    RichText::new(luma_text)
                        .font(FontId::monospace(12.0))
                        .color(TEXT_MUTED),
                );
            }

            ui.horizontal(|ui| {
                if guarded_load_button(
                    ui,
                    "< Prev",
                    prev_target,
                    air_state,
                    load_guard,
                    "Load previous preset (2-click confirm if ON-AIR/MIXED; Shift = hard cut)",
                ) {
                    bus.trigger(prev_target, shift_held, true);
                }
                if guarded_load_button(
                    ui,
                    "Next >",
                    next_target,
                    air_state,
                    load_guard,
                    "Load next preset (2-click confirm if ON-AIR/MIXED; Shift = hard cut)",
                ) {
                    bus.trigger(next_target, shift_held, true);
                }

                let is_smooth = bus.get_bool(smooth_param);
                let mode_label = if is_smooth {
                    "Smooth 1.5s"
                } else {
                    "Hard Cut"
                };
                if ui
                    .button(mode_label)
                    .on_hover_text("Default preset transition mode (stored in Control Bus)")
                    .clicked()
                {
                    bus.toggle(smooth_param);
                }

                if let Some(blank_target) = test_blank_target {
                    if guarded_load_button(
                        ui,
                        "Test Blank",
                        blank_target,
                        air_state,
                        load_guard,
                        "Load broken-shader preset to test the 2-second blank-deck warning",
                    ) {
                        bus.trigger(blank_target, false, true);
                    }
                }
            });
        });
}

fn guarded_load_button(
    ui: &mut egui::Ui,
    normal_label: &str,
    target_key: &str,
    air_state: DeckAirState,
    load_guard: &mut LoadGuardState,
    hover_text: &str,
) -> bool {
    let armed = load_guard.is_armed(target_key);
    let btn = if armed {
        egui::Button::new(
            RichText::new("CONFIRM?")
                .strong()
                .color(Color32::from_rgb(20, 20, 20)),
        )
        .fill(COLOR_WARN)
    } else {
        egui::Button::new(normal_label)
    };
    if ui.add(btn).on_hover_text(hover_text).clicked() {
        load_guard.click_load(target_key, air_state)
    } else {
        false
    }
}

/// Live Center Column: Master Preview + 5 Blend Modes + Crossfader + Blackout with dynamic key hints.
fn draw_master_mixer_column(
    ui: &mut egui::Ui,
    master_tex_id: TextureId,
    blackout_alpha: f32,
    preview_h_from_stage: f32,
    bus: &mut ControlBus,
) {
    let blackout_target = bus.get_bool(param::MASTER_BLACKOUT);
    let is_darkening = blackout_target || blackout_alpha > 0.01;
    let border_color = if blackout_target {
        COLOR_ALERT
    } else {
        COLOR_OK.gamma_multiply(0.65)
    };

    // Read dynamic key hints from `bus.mappings` so remaps reflect on-screen immediately.
    let hint_blend = bus
        .mappings
        .hint_for_target(param::MIXER_BLEND_MODE, MappingMode::Cycle);
    let hint_cut_a = bus.mappings.hint_for_set(param::MIXER_CROSSFADER, 0.0);
    let hint_center = bus.mappings.hint_for_set(param::MIXER_CROSSFADER, 0.5);
    let hint_cut_b = bus.mappings.hint_for_set(param::MIXER_CROSSFADER, 1.0);
    let hint_blackout = bus
        .mappings
        .hint_for_target(param::MASTER_BLACKOUT, MappingMode::Toggle);

    Frame::new()
        .fill(BG_CARD)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(8))
        .stroke(Stroke::new(1.5, border_color))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("MASTER OUTPUT")
                        .font(FontId::proportional(18.5))
                        .strong()
                        .color(TEXT_PRIMARY),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (pill_text, pill_bg, pill_fg) = if blackout_target {
                        ("BLACKOUT", COLOR_ALERT, Color32::WHITE)
                    } else if is_darkening {
                        ("FADING...", Color32::from_rgb(100, 40, 20), COLOR_WARN)
                    } else {
                        ("LIVE", Color32::from_rgb(16, 62, 42), COLOR_OK)
                    };
                    Frame::new()
                        .fill(pill_bg)
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(Margin::symmetric(8, 2))
                        .stroke(Stroke::new(1.0, pill_fg))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(pill_text)
                                    .font(FontId::monospace(12.5))
                                    .strong()
                                    .color(pill_fg),
                            );
                        });
                });
            });

            let col_inner_w = ui.available_width().max(160.0);
            let preview_h = preview_h_from_stage.min(col_inner_w * 9.0 / 16.0);
            let preview_w = preview_h * 16.0 / 9.0;
            let preview_size = Vec2::new(preview_w, preview_h);

            ui.vertical_centered(|ui| {
                let img = egui::Image::new((master_tex_id, preview_size))
                    .uv(Rect::from_min_max(egui::pos2(0.0, 1.0), egui::pos2(1.0, 0.0)));
                ui.add(img);
            });

            let mode_idx = bus.get_discrete_index(param::MIXER_BLEND_MODE, BLEND_MODE_COUNT);
            let current_mode = BlendMode::from_index(mode_idx);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Blend [{hint_blend}]:"))
                        .font(FontId::proportional(14.0))
                        .color(TEXT_MUTED),
                );
                egui::ComboBox::from_id_salt("master_blend_mode")
                    .selected_text(RichText::new(current_mode.label()).strong())
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for mode in BlendMode::ALL {
                            let selected = mode == current_mode;
                            if ui
                                .selectable_label(selected, mode.label())
                                .on_hover_text(mode.description())
                                .clicked()
                            {
                                let norm = mode.as_index() as f32 / (BLEND_MODE_COUNT - 1) as f32;
                                bus.set(param::MIXER_BLEND_MODE, norm);
                            }
                        }
                    });

                let xfade = bus.get(param::MIXER_CROSSFADER);
                let pct_b = (xfade * 100.0).round() as u32;
                let pct_a = 100_u32.saturating_sub(pct_b);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("A {pct_a:>3}% / {pct_b:>3}% B"))
                            .font(FontId::monospace(12.5))
                            .color(TEXT_MUTED),
                    );
                });
            });

            let mut xfade = bus.get(param::MIXER_CROSSFADER);
            ui.horizontal(|ui| {
                ui.label(RichText::new("A").strong().color(ACCENT_DECK_A));
                let slider_w = (ui.available_width() - 34.0).max(80.0);
                ui.spacing_mut().slider_width = slider_w;
                let resp = ui
                    .add(egui::Slider::new(&mut xfade, 0.0..=1.0).show_value(false))
                    .on_hover_text("Crossfader (double-click to center; Left/Right arrows nudge 5%)");
                if resp.double_clicked() {
                    bus.set(param::MIXER_CROSSFADER, 0.5);
                } else if resp.changed() {
                    bus.set(param::MIXER_CROSSFADER, xfade);
                }
                ui.label(RichText::new("B").strong().color(ACCENT_DECK_B));
            });

            ui.horizontal(|ui| {
                if ui
                    .button(format!("Cut A [{hint_cut_a}]"))
                    .on_hover_text("Jump crossfader to 100% Deck A")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 0.0);
                }
                if ui
                    .button(format!("Center [{hint_center}]"))
                    .on_hover_text("Center crossfader (50% A / 50% B)")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 0.5);
                }
                if ui
                    .button(format!("Cut B [{hint_cut_b}]"))
                    .on_hover_text("Jump crossfader to 100% Deck B")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 1.0);
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let blk_label = if blackout_target {
                        format!("BLACKOUT ON [{hint_blackout}]")
                    } else {
                        format!("BLACKOUT [{hint_blackout}]")
                    };
                    let blk_btn = if blackout_target {
                        egui::Button::new(RichText::new(blk_label).strong().color(Color32::WHITE))
                            .fill(COLOR_ALERT)
                    } else {
                        egui::Button::new(
                            RichText::new(blk_label)
                                .strong()
                                .color(Color32::from_rgb(252, 165, 165)),
                        )
                        .fill(Color32::from_rgb(76, 22, 26))
                    };
                    if ui
                        .add(blk_btn)
                        .on_hover_text("Emergency Blackout: smooth 0.4s fade-to-black on Master")
                        .clicked()
                    {
                        bus.toggle(param::MASTER_BLACKOUT);
                    }
                });
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn draw_browser_panel(
    ui: &mut egui::Ui,
    state: &ConsoleUiState<'_>,
    air_a: DeckAirState,
    air_b: DeckAirState,
    shift_held: bool,
    bus: &mut ControlBus,
    load_guard: &mut LoadGuardState,
    interact: &mut UiInteractionState,
) {
    let total_presets = state.all_presets.len();
    let filtered_len = state.filtered_indices.len();

    // Header row: title + key hints + live scan progress / count + Rescan button.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("PRESET BROWSER")
                .font(FontId::proportional(17.0))
                .strong()
                .color(TEXT_PRIMARY),
        );
        ui.label(
            RichText::new(
                "Up/Down select | Enter loads CUE deck | A/B target deck (Ctrl forces on-air; Shift hard-cuts)",
            )
            .font(FontId::proportional(13.0))
            .color(TEXT_MUTED),
        );

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let rescan_enabled = !state.scan_status.scanning;
            if ui
                .add_enabled(rescan_enabled, egui::Button::new("Rescan"))
                .on_hover_text("Re-scan presets\\ on a background thread and refresh preset_cache.json")
                .clicked()
            {
                interact.request_rescan = true;
            }

            if state.scan_status.scanning {
                let text = if state.scan_status.total > 0 {
                    format!(
                        "Scanning {}/{}...",
                        state.scan_status.scanned, state.scan_status.total
                    )
                } else {
                    "Scanning presets...".to_string()
                };
                Frame::new()
                    .fill(Color32::from_rgb(76, 58, 12))
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(8, 2))
                    .stroke(Stroke::new(1.0, COLOR_WARN))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(text)
                                .font(FontId::monospace(12.5))
                                .strong()
                                .color(COLOR_WARN),
                        );
                    });
            } else {
                let summary_text = format!("Showing {filtered_len} / {total_presets}");
                let hover_detail = format!(
                    "Library scan complete in {:.1} ms (provisional, laptop-limited)\n\
                     Cache hits: {} | Newly parsed: {}\n\
                     Cache file: preset_cache.json",
                    state.scan_status.elapsed_ms,
                    state.scan_status.cache_hits,
                    state.scan_status.parsed
                );
                Frame::new()
                    .fill(BG_CARD)
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(8, 2))
                    .stroke(Stroke::new(1.0, BORDER_SUBTLE))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(summary_text)
                                .font(FontId::monospace(12.5))
                                .strong()
                                .color(ACCENT_DECK_A),
                        )
                        .on_hover_text(hover_detail);
                    });
            }
        });
    });

    // Filter toolbar: live Search box + Weight pills + Folder dropdown.
    ui.horizontal(|ui| {
        ui.label("Search [/]:");

        let border_stroke = if interact.search_focused {
            Stroke::new(2.0, ACCENT_DECK_A)
        } else {
            Stroke::new(1.0, BORDER_SUBTLE)
        };
        Frame::new()
            .stroke(border_stroke)
            .corner_radius(CornerRadius::same(4))
            .inner_margin(Margin::same(1))
            .show(ui, |ui| {
                let search_id = ui.make_persistent_id("preset_search_box");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut interact.search_query)
                        .id(search_id)
                        .desired_width(240.0)
                        .hint_text("Type name or folder (e.g. fractal)..."),
                );
                if interact.request_focus_search {
                    resp.request_focus();
                    interact.request_focus_search = false;
                }
                if interact.request_blur_search {
                    resp.surrender_focus();
                    interact.request_blur_search = false;
                }
                interact.search_focused = resp.has_focus();
            });

        if !interact.search_query.is_empty() && ui.button("Clear").clicked() {
            interact.search_query.clear();
        }

        ui.separator();
        ui.label(
            RichText::new("Weight:")
                .color(TEXT_MUTED)
                .font(FontId::proportional(14.0)),
        )
        .on_hover_text("Filter by estimated CPU/draw load (enabled shape instances; estimate only).");

        for (filter_opt, active_bg, active_fg) in [
            (WeightFilter::All, ACCENT_DECK_A, Color32::BLACK),
            (WeightFilter::Light, COLOR_OK, Color32::BLACK),
            (WeightFilter::Medium, COLOR_WARN, Color32::BLACK),
            (WeightFilter::Heavy, COLOR_ALERT, Color32::WHITE),
        ] {
            let is_active = interact.weight_filter == filter_opt;
            let btn = if is_active {
                egui::Button::new(
                    RichText::new(filter_opt.label())
                        .strong()
                        .color(active_fg),
                )
                .fill(active_bg)
            } else {
                egui::Button::new(filter_opt.label())
            };
            if ui.add(btn).clicked() {
                interact.weight_filter = filter_opt;
            }
        }

        ui.separator();
        ui.label(
            RichText::new("Folder:")
                .color(TEXT_MUTED)
                .font(FontId::proportional(14.0)),
        );
        let selected_folder_text = interact
            .folder_filter
            .as_deref()
            .unwrap_or("All Folders");
        egui::ComboBox::from_id_salt("browser_folder_filter")
            .selected_text(RichText::new(truncate_chars(selected_folder_text, 24)).strong())
            .width(190.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(interact.folder_filter.is_none(), "All Folders")
                    .clicked()
                {
                    interact.folder_filter = None;
                }
                for folder in state.folders {
                    let is_sel = interact.folder_filter.as_deref() == Some(folder.as_str());
                    if ui.selectable_label(is_sel, folder).clicked() {
                        interact.folder_filter = Some(folder.clone());
                    }
                }
            });

        let has_any_filter = !interact.search_query.is_empty()
            || interact.weight_filter != WeightFilter::All
            || interact.folder_filter.is_some();
        if has_any_filter && ui.button("Reset Filters").clicked() {
            interact.search_query.clear();
            interact.weight_filter = WeightFilter::All;
            interact.folder_filter = None;
        }
    });

    ui.separator();

    let scroll_to = interact.scroll_to_selected;
    interact.scroll_to_selected = false;

    if filtered_len == 0 {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("No presets match the current search / weight / folder filter.")
                    .font(FontId::proportional(15.5))
                    .color(TEXT_MUTED),
            );
            ui.add_space(6.0);
            if ui.button("Reset Filters").clicked() {
                interact.search_query.clear();
                interact.weight_filter = WeightFilter::All;
                interact.folder_filter = None;
            }
        });
        return;
    }

    // Virtualized scroll list (`show_rows`): renders ONLY visible rows (O(1) for 300 or 10,000 presets).
    let row_height = 34.0;
    ScrollArea::vertical()
        .id_salt("preset_browser_scroll")
        .auto_shrink([false, false])
        .show_rows(ui, row_height, filtered_len, |ui, row_range| {
            let row_stride = row_height + ui.spacing().item_spacing.y;
            if scroll_to {
                let sel = interact.selected_preset_idx.min(filtered_len - 1);
                let list_top_y = ui.cursor().top() - (row_range.start as f32 * row_stride);
                let target_y = list_top_y + (sel as f32 * row_stride);
                let target_rect = Rect::from_min_size(
                    egui::pos2(ui.cursor().left(), target_y),
                    Vec2::new(ui.available_width().max(10.0), row_height),
                );
                ui.scroll_to_rect(target_rect, None);
            }

            for vis_idx in row_range {
                let entry_idx = state.filtered_indices[vis_idx];
                let row = &state.all_presets[entry_idx];
                let is_selected = vis_idx == interact.selected_preset_idx;
                let on_deck_a = row.path == state.deck_a.preset_path;
                let on_deck_b = row.path == state.deck_b.preset_path;

                let row_bg = if is_selected {
                    Color32::from_rgb(24, 54, 68)
                } else if vis_idx % 2 == 0 {
                    BG_CARD
                } else {
                    Color32::from_rgb(20, 24, 31)
                };
                let row_stroke = if is_selected {
                    Stroke::new(1.5, ACCENT_DECK_A)
                } else {
                    Stroke::NONE
                };

                let row_w = ui.available_width().max(200.0);
                let frame_resp = ui.allocate_ui_with_layout(
                    Vec2::new(row_w, row_height),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        Frame::new()
                            .fill(row_bg)
                            .stroke(row_stroke)
                            .corner_radius(CornerRadius::same(3))
                            .inner_margin(Margin::symmetric(8, 3))
                            .show(ui, |ui| {
                                ui.set_min_width((row_w - 18.0).max(160.0));
                                ui.set_min_height((row_height - 8.0).max(20.0));
                                ui.horizontal_centered(|ui| {
                                    let marker = if is_selected { ">" } else { " " };
                                    ui.label(
                                        RichText::new(marker)
                                            .font(FontId::monospace(15.0))
                                            .strong()
                                            .color(ACCENT_DECK_A),
                                    );

                                    let (badge_bg, badge_fg) =
                                        complexity_colors(row.complexity.tier);
                                    Frame::new()
                                        .fill(badge_bg)
                                        .corner_radius(CornerRadius::same(4))
                                        .inner_margin(Margin::symmetric(6, 1))
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{:<6} {:>4}",
                                                    row.complexity.tier.label(),
                                                    row.complexity.enabled_shape_instances
                                                ))
                                                .font(FontId::monospace(12.0))
                                                .strong()
                                                .color(badge_fg),
                                            )
                                            .on_hover_text(
                                                "Estimated CPU/draw load (enabled shape instances; estimate only).",
                                            );
                                        });

                                    if on_deck_a {
                                        Frame::new()
                                            .fill(ACCENT_DECK_A.gamma_multiply(0.28))
                                            .stroke(Stroke::new(1.0, ACCENT_DECK_A))
                                            .corner_radius(CornerRadius::same(3))
                                            .inner_margin(Margin::symmetric(4, 1))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new("A")
                                                        .font(FontId::monospace(11.5))
                                                        .strong()
                                                        .color(ACCENT_DECK_A),
                                                );
                                            });
                                    }
                                    if on_deck_b {
                                        Frame::new()
                                            .fill(ACCENT_DECK_B.gamma_multiply(0.28))
                                            .stroke(Stroke::new(1.0, ACCENT_DECK_B))
                                            .corner_radius(CornerRadius::same(3))
                                            .inner_margin(Margin::symmetric(4, 1))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new("B")
                                                        .font(FontId::monospace(11.5))
                                                        .strong()
                                                        .color(ACCENT_DECK_B),
                                                );
                                            });
                                    }

                                    let name_short = truncate_chars(&row.display_name, 56);
                                    ui.label(
                                        RichText::new(name_short)
                                            .font(FontId::proportional(14.5))
                                            .strong()
                                            .color(TEXT_PRIMARY),
                                    )
                                    .on_hover_text(&row.display_name);

                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let key_b = format!("row_load_b_{entry_idx}");
                                        if guarded_load_button(
                                            ui,
                                            "Load B",
                                            &key_b,
                                            air_b,
                                            load_guard,
                                            "Load preset into Deck B (2-click confirm if ON-AIR/MIXED; Shift+Click = hard cut)",
                                        ) {
                                            interact.selected_preset_idx = vis_idx;
                                            bus.trigger(
                                                param::DECK_B_LOAD_SELECTED,
                                                shift_held,
                                                true,
                                            );
                                        }

                                        let key_a = format!("row_load_a_{entry_idx}");
                                        if guarded_load_button(
                                            ui,
                                            "Load A",
                                            &key_a,
                                            air_a,
                                            load_guard,
                                            "Load preset into Deck A (2-click confirm if ON-AIR/MIXED; Shift+Click = hard cut)",
                                        ) {
                                            interact.selected_preset_idx = vis_idx;
                                            bus.trigger(
                                                param::DECK_A_LOAD_SELECTED,
                                                shift_held,
                                                true,
                                            );
                                        }

                                        ui.label(
                                            RichText::new(truncate_chars(&row.folder, 24))
                                                .font(FontId::proportional(13.0))
                                                .color(TEXT_MUTED),
                                        )
                                        .on_hover_text(&row.folder);
                                    });
                                });
                            });
                    },
                );

                if frame_resp.response.interact(egui::Sense::click()).clicked() {
                    interact.selected_preset_idx = vis_idx;
                }
            }
        });
}

fn draw_keys_modal(
    ctx: &egui::Context,
    bus: &ControlBus,
    interact: &mut UiInteractionState,
) {
    let mut open = interact.keys_modal_open;
    egui::Window::new("Keyboard Mappings (mappings.json)")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(680.0)
        .default_height(460.0)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "Performance keys are paused while this window is open. Click Remap, then press your new key combo (Esc cancels).",
                )
                .font(FontId::proportional(13.5))
                .color(TEXT_MUTED),
            );
            ui.label(
                RichText::new(
                    "Fixed UI navigation keys:  / = Focus Search  |  Esc = Exit Search / Close  |  Up/Down = Select Preset Row",
                )
                .font(FontId::monospace(12.5))
                .color(ACCENT_DECK_A),
            );
            ui.separator();

            ScrollArea::vertical()
                .max_height(310.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (idx, binding) in bus.mappings.keyboard.iter().enumerate() {
                        let is_capturing = interact.remapping_idx == Some(idx);
                        let row_bg = if is_capturing {
                            Color32::from_rgb(24, 64, 78)
                        } else if idx % 2 == 0 {
                            BG_CARD
                        } else {
                            Color32::from_rgb(20, 24, 31)
                        };
                        Frame::new()
                            .fill(row_bg)
                            .inner_margin(Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(binding.action_description())
                                            .font(FontId::proportional(14.5))
                                            .color(TEXT_PRIMARY),
                                    );

                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if is_capturing {
                                            let btn = egui::Button::new(
                                                RichText::new("Press key... (Esc)")
                                                    .strong()
                                                    .color(Color32::BLACK),
                                            )
                                            .fill(ACCENT_DECK_A);
                                            if ui.add(btn).clicked() {
                                                interact.remapping_idx = None;
                                            }
                                        } else if ui.button("Remap").clicked() {
                                            interact.remapping_idx = Some(idx);
                                            interact.remap_feedback = Some(format!(
                                                "Listening for new key for `{}`...",
                                                binding.action_description()
                                            ));
                                        }

                                        Frame::new()
                                            .fill(Color32::from_rgb(16, 20, 28))
                                            .stroke(Stroke::new(1.0, BORDER_SUBTLE))
                                            .corner_radius(CornerRadius::same(4))
                                            .inner_margin(Margin::symmetric(8, 2))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new(binding.short_key_badge())
                                                        .font(FontId::monospace(13.5))
                                                        .strong()
                                                        .color(COLOR_WARN),
                                                );
                                            });
                                    });
                                });
                            });
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Reset Defaults").clicked() {
                    interact.request_reset_defaults = true;
                    interact.remapping_idx = None;
                }
                if let Some(msg) = &interact.remap_feedback {
                    ui.label(
                        RichText::new(msg)
                            .font(FontId::proportional(13.5))
                            .color(COLOR_WARN),
                    );
                }
            });
        });

    interact.keys_modal_open = open;
    if !open {
        interact.remapping_idx = None;
    }
}

fn draw_status_strip(
    ui: &mut egui::Ui,
    state: &ConsoleUiState<'_>,
    interact: &UiInteractionState,
) {
    ui.horizontal_centered(|ui| {
        ui.label(
            RichText::new(format!("Audio: {} Hz", state.audio_sample_rate))
                .font(FontId::monospace(13.0))
                .color(TEXT_MUTED),
        );
        ui.separator();

        // Active Load Safety / remap toast takes priority so guard feedback is impossible to miss.
        if let Some(toast_msg) = interact.active_toast() {
            ui.label(
                RichText::new(format!("! {toast_msg}"))
                    .font(FontId::proportional(13.5))
                    .strong()
                    .color(COLOR_WARN),
            );
        } else if let Some(last) = state.last_load {
            if let Some(err) = &last.error {
                ui.label(
                    RichText::new(format!(
                        "Load error ({}): {}",
                        last.deck_label,
                        truncate_chars(err, 52)
                    ))
                    .font(FontId::monospace(13.0))
                    .strong()
                    .color(COLOR_ALERT),
                );
            } else {
                let (color, stall_tag) = if last.load_ms >= LOAD_STALL_ALERT_MS {
                    (COLOR_ALERT, " [STALL >=2fr]")
                } else if last.load_ms >= LOAD_STALL_WARN_MS {
                    (COLOR_WARN, " [STALL >1fr]")
                } else {
                    (TEXT_PRIMARY, "")
                };
                ui.label(
                    RichText::new(format!(
                        "Last load [{}]: {} ({:.1} ms){stall_tag}",
                        last.deck_label,
                        truncate_chars(&last.preset_name, 26),
                        last.load_ms
                    ))
                    .font(FontId::monospace(13.0))
                    .strong()
                    .color(color),
                );
            }
        } else {
            ui.label(
                RichText::new("Last load: —")
                    .font(FontId::monospace(13.0))
                    .color(TEXT_MUTED),
            );
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (keys_text, keys_color) = if interact.search_focused {
                ("KEYS PAUSED (Typing)", COLOR_WARN)
            } else if interact.keys_modal_open {
                ("KEYS PAUSED (Remap)", ACCENT_DECK_A)
            } else {
                ("Keys: ACTIVE (/ Search)", COLOR_OK)
            };
            ui.label(
                RichText::new(keys_text)
                    .font(FontId::monospace(12.5))
                    .strong()
                    .color(keys_color),
            );
            ui.separator();
            ui.label(
                RichText::new(format!("Blank PBO: {:.0} us", state.blank_check_avg_us))
                    .font(FontId::monospace(12.5))
                    .color(TEXT_MUTED),
            );
            ui.separator();
            let gl_color = if state.gl_errors == 0 {
                COLOR_OK
            } else {
                COLOR_ALERT
            };
            ui.label(
                RichText::new(format!("GL Errors: {}", state.gl_errors))
                    .font(FontId::monospace(13.0))
                    .strong()
                    .color(gl_color),
            );
            ui.separator();
            ui.label(
                RichText::new(state.mappings_status)
                    .font(FontId::monospace(12.0))
                    .color(TEXT_MUTED),
            );
        });
    });
}

fn complexity_colors(tier: ComplexityTier) -> (Color32, Color32) {
    match tier {
        ComplexityTier::Light => (Color32::from_rgb(16, 62, 42), COLOR_OK),
        ComplexityTier::Medium => (Color32::from_rgb(76, 58, 12), COLOR_WARN),
        ComplexityTier::Heavy => (Color32::from_rgb(108, 24, 24), Color32::WHITE),
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut iter = s.chars();
    let collected: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{collected}...")
    } else {
        collected
    }
}
