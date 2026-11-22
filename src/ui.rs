//! Dark-Venue Traktor-Style `egui 0.37` Console UI (Milestone 2 — Stage `2A-2`).
//!
//! Layout specification: Section 13 of `docs/project_research_and_design.md`.
//! - All sizes are in `egui` logical points (`pt`), automatically scaled by Windows DPI.
//! - Design size: `1920x1080 pt`; minimum supported window size: `1280x720 pt`.
//! - Preview sizes follow available stage height (`(window_h * 0.40).clamp(306, 432)`)
//!   keeping exact `16:9` aspect ratio, and the Preset Browser fills all remaining height.
//! - Every working control (crossfader, cuts, blend modes, blackout, deck load buttons,
//!   transition mode) reads and writes through the unified `ControlBus` (`0.0..=1.0`).
//! - Controls arriving in `2A-3`..`2B` remain greyed out until implemented.

use std::time::{Duration, Instant};

use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, ScrollArea,
    Stroke, TextStyle, TextureId, Vec2,
};

use crate::mixer::{BlendMode, DeckAirState};
use crate::render_deck::BlankDeckStatus;
use milkdrop_vj::control::{param, ControlBus, BLEND_MODE_COUNT};
use milkdrop_vj::preset_meta::{ComplexityTier, PresetComplexity};

/// Thresholds for per-deck rolling GPU render time (`ms`) warnings (provisional, laptop-limited).
/// Based on sharing one `16.67 ms` 60 Hz frame budget across Deck A + Deck B + Mixer/UI (~2 ms).
pub const DECK_MS_WARN: f64 = 6.0;
pub const DECK_MS_ALERT: f64 = 10.0;

/// Thresholds for highlighting the Status Strip's `Last load` stall readout (Risk 8).
/// `16.7 ms` = >1 dropped 60 FPS frame; `33.3 ms` = >=2 dropped frames.
pub const LOAD_STALL_WARN_MS: f64 = 16.7;
pub const LOAD_STALL_ALERT_MS: f64 = 33.3;

/// Seconds a protected on-air/mixed load button stays armed (`[CONFIRM?]`) before disarming.
const LOAD_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

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
    /// Clears expired arming state (> 3 seconds old).
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

    /// Handles clicking a load button on a deck with air state `air_state`.
    /// Returns `true` if the load should execute immediately, or `false` if this first click
    /// merely armed the 3-second confirmation guard.
    pub fn click_load(&mut self, target: &str, air_state: DeckAirState) -> bool {
        if !air_state.is_protected() {
            // Deck is in CUE (<= 5% weight): safe to load in one click.
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

/// One row shown in the `2A-1`/`2A-2` read-only Preset Browser preview (activated in `2A-4`).
#[derive(Debug, Clone)]
pub struct StarterPresetRow {
    pub name: String,
    pub folder: String,
    pub complexity: PresetComplexity,
}

/// Read-only per-deck snapshot passed into `draw_console_ui()` each frame.
pub struct DeckUiSnapshot<'a> {
    pub texture_id: TextureId,
    pub preset_name: &'a str,
    pub complexity: PresetComplexity,
    pub render_ms_avg: f64,
    pub blank: BlankDeckStatus,
}

/// All live readouts needed to draw one frame of the `2A-2` Control Console.
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
    pub starter_presets: &'a [StarterPresetRow],
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

/// Draws the entire Milestone 2 (`2A-2`) console inside `ui` (`egui 0.37` root `Ui`).
pub fn draw_console_ui(
    ui: &mut egui::Ui,
    state: &ConsoleUiState<'_>,
    bus: &mut ControlBus,
    load_guard: &mut LoadGuardState,
) {
    load_guard.expire_if_needed();

    let window_h = ui.ctx().content_rect().height().max(720.0);
    // Exact Section 13 vertical budget:
    // At 720 pt -> stage = 306 pt (preview 320x180); at 1080 pt -> stage = 432 pt (preview 544x306).
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
            draw_top_bar(ui, state);
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
            draw_status_strip(ui, state);
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

    // 4. PRESET BROWSER — CentralPanel fills 100% of remaining vertical space.
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(BG_PANEL)
                .inner_margin(Margin::symmetric(12, 8))
                .stroke(Stroke::new(1.0, BORDER_SUBTLE)),
        )
        .show(ui, |ui| {
            draw_browser_placeholder(ui, state.starter_presets);
        });
}

fn draw_top_bar(ui: &mut egui::Ui, state: &ConsoleUiState<'_>) {
    ui.horizontal_centered(|ui| {
        // Left cluster: greyed-out (2A-5) audio controls + LIVE meter + large audio status badge.
        ui.add_enabled(
            false,
            egui::Button::new(format!("{} v (2A-5)", state.audio_source_label)),
        );

        let dev_short = truncate_chars(state.audio_device_name, 26);
        ui.add_enabled(false, egui::Button::new(format!("{dev_short} v (2A-5)")));

        draw_peak_meter(ui, state.audio_peak_db);
        draw_audio_status_pill(ui, state.audio_state);

        // Right cluster: stage placeholders + GPU + large global FPS badge.
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

            ui.add_enabled(false, egui::Button::new("Keys (2A-3)"));
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
            // 1. Header row (~28 pt): Deck title + ON-AIR/MIXED/CUE badge + rolling GPU render time.
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

            // 2. Live 16:9 OpenGL preview sized strictly from available stage height & column width.
            let col_inner_w = ui.available_width().max(160.0);
            let preview_h = preview_h_from_stage.min(col_inner_w * 9.0 / 16.0);
            let preview_w = preview_h * 16.0 / 9.0;
            let preview_size = Vec2::new(preview_w, preview_h);

            ui.vertical_centered(|ui| {
                // Flip V coordinate (`1.0 -> 0.0`) because OpenGL textures have origin at bottom-left.
                let img = egui::Image::new((snap.texture_id, preview_size))
                    .uv(Rect::from_min_max(egui::pos2(0.0, 1.0), egui::pos2(1.0, 0.0)));
                ui.add(img);
            });

            // 3. Preset name + calibrated static complexity badge (~24 pt).
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

            // 4. Blank-deck warning slot (~20 pt).
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

            // 5. Working deck controls — wired through the Control Bus with 2-click Load Safety
            //    whenever this deck is protected (ON-AIR or MIXED, weight > 5%).
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

/// Renders a load button with Section 13.3 Load Safety:
/// - `CUE` deck (`w <= 5%`): fires on first click.
/// - `ON-AIR` / `MIXED` deck (`w > 5%`): first click turns bright amber `[CONFIRM?]` for 3s;
///   second click executes the load.
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

/// Live Stage `2A-2` Center Column: Master Preview + 5 Blend Modes + Crossfader + Blackout.
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

    Frame::new()
        .fill(BG_CARD)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(8))
        .stroke(Stroke::new(1.5, border_color))
        .show(ui, |ui| {
            // 1. Header (~28 pt): MASTER OUTPUT + LIVE / BLACKOUT pill.
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

            // 2. Live 16:9 Master Output texture preview (exact same height rule as Deck A & B).
            let col_inner_w = ui.available_width().max(160.0);
            let preview_h = preview_h_from_stage.min(col_inner_w * 9.0 / 16.0);
            let preview_w = preview_h * 16.0 / 9.0;
            let preview_size = Vec2::new(preview_w, preview_h);

            ui.vertical_centered(|ui| {
                let img = egui::Image::new((master_tex_id, preview_size))
                    .uv(Rect::from_min_max(egui::pos2(0.0, 1.0), egui::pos2(1.0, 0.0)));
                ui.add(img);
            });

            // 3. Blend Mode dropdown (`2A-2`, wired to Control Bus `mixer.blend_mode`).
            let mode_idx = bus.get_discrete_index(param::MIXER_BLEND_MODE, BLEND_MODE_COUNT);
            let current_mode = BlendMode::from_index(mode_idx);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Blend [M]:")
                        .font(FontId::proportional(14.0))
                        .color(TEXT_MUTED),
                );
                egui::ComboBox::from_id_salt("master_blend_mode")
                    .selected_text(RichText::new(current_mode.label()).strong())
                    .width(135.0)
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

            // 4. Wide Crossfader Slider (double-click snaps to 0.5 Center).
            let mut xfade = bus.get(param::MIXER_CROSSFADER);
            ui.horizontal(|ui| {
                ui.label(RichText::new("A").strong().color(ACCENT_DECK_A));
                let slider_w = (ui.available_width() - 34.0).max(80.0);
                ui.spacing_mut().slider_width = slider_w;
                let resp = ui
                    .add(egui::Slider::new(&mut xfade, 0.0..=1.0).show_value(false))
                    .on_hover_text("Crossfader (double-click to center; keys [ \\ ] <- -> in 2A-3)");
                if resp.double_clicked() {
                    bus.set(param::MIXER_CROSSFADER, 0.5);
                } else if resp.changed() {
                    bus.set(param::MIXER_CROSSFADER, xfade);
                }
                ui.label(RichText::new("B").strong().color(ACCENT_DECK_B));
            });

            // 5. Cut buttons (`Cut A`, `Center`, `Cut B`) + Emergency Blackout toggle (`0.4s` fade).
            ui.horizontal(|ui| {
                if ui
                    .button("Cut A [[]")
                    .on_hover_text("Jump crossfader to 100% Deck A")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 0.0);
                }
                if ui
                    .button("Center [\\]")
                    .on_hover_text("Center crossfader (50% A / 50% B)")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 0.5);
                }
                if ui
                    .button("Cut B []]")
                    .on_hover_text("Jump crossfader to 100% Deck B")
                    .clicked()
                {
                    bus.set(param::MIXER_CROSSFADER, 1.0);
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let blk_btn = if blackout_target {
                        egui::Button::new(
                            RichText::new("BLACKOUT ON")
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(COLOR_ALERT)
                    } else {
                        egui::Button::new(
                            RichText::new("BLACKOUT")
                                .strong()
                                .color(Color32::from_rgb(252, 165, 165)),
                        )
                        .fill(Color32::from_rgb(76, 22, 26))
                    };
                    if ui
                        .add(blk_btn)
                        .on_hover_text("Emergency Blackout: smooth 0.4s fade-to-black on Master (Backspace in 2A-3)")
                        .clicked()
                    {
                        bus.toggle(param::MASTER_BLACKOUT);
                    }
                });
            });
        });
}

fn draw_browser_placeholder(ui: &mut egui::Ui, presets: &[StarterPresetRow]) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("PRESET BROWSER")
                .font(FontId::proportional(17.0))
                .strong()
                .color(TEXT_MUTED),
        );
        ui.label(
            RichText::new("(Full search, background scan & cache arrive in Stage 2A-4)")
                .font(FontId::proportional(13.5))
                .color(TEXT_MUTED),
        );
    });

    ui.add_enabled_ui(false, |ui| {
        let mut search_dummy = String::new();
        ui.horizontal(|ui| {
            ui.label("Search [/]:");
            ui.add(
                egui::TextEdit::singleline(&mut search_dummy)
                    .desired_width(220.0)
                    .hint_text("Type to filter presets (2A-4)..."),
            );
            ui.separator();
            ui.label("Weight:");
            let _ = ui.button("All");
            let _ = ui.button("Light");
            let _ = ui.button("Med");
            let _ = ui.button("Heavy");
            ui.separator();
            ui.label("Folder:");
            let _ = ui.button("All v");
        });
    });

    ui.separator();

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (idx, row) in presets.iter().enumerate() {
                let row_bg = if idx % 2 == 0 {
                    BG_CARD
                } else {
                    Color32::from_rgb(20, 24, 31)
                };
                Frame::new()
                    .fill(row_bg)
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_min_height(24.0);
                        ui.horizontal(|ui| {
                            let (badge_bg, badge_fg) = complexity_colors(row.complexity.tier);
                            Frame::new()
                                .fill(badge_bg)
                                .corner_radius(CornerRadius::same(4))
                                .inner_margin(Margin::symmetric(6, 2))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{:<6} {:>4}",
                                            row.complexity.tier.label(),
                                            row.complexity.enabled_shape_instances
                                        ))
                                        .font(FontId::monospace(12.5))
                                        .strong()
                                        .color(badge_fg),
                                    );
                                });

                            ui.label(
                                RichText::new(&row.name)
                                    .font(FontId::proportional(15.0))
                                    .color(TEXT_PRIMARY),
                            );

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add_enabled(false, egui::Button::new("Load B (2A-4)"));
                                ui.add_enabled(false, egui::Button::new("Load A (2A-4)"));
                                ui.label(
                                    RichText::new(&row.folder)
                                        .font(FontId::proportional(13.5))
                                        .color(TEXT_MUTED),
                                );
                            });
                        });
                    });
            }
        });
}

fn draw_status_strip(ui: &mut egui::Ui, state: &ConsoleUiState<'_>) {
    ui.horizontal_centered(|ui| {
        ui.label(
            RichText::new(format!("Audio: {} Hz", state.audio_sample_rate))
                .font(FontId::monospace(13.0))
                .color(TEXT_MUTED),
        );
        ui.separator();

        if let Some(last) = state.last_load {
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
            ui.label(
                RichText::new(state.mappings_status)
                    .font(FontId::monospace(12.5))
                    .color(TEXT_MUTED),
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
