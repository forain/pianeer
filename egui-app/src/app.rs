use std::time::Duration;

use egui::{CentralPanel, Color32, FontId, RichText, ScrollArea, SidePanel, TopBottomPanel, Ui};
use pianeer_core::snapshot::{ClientCmd, WebSnapshot};

use crate::PianeerBackend;

// ── App ───────────────────────────────────────────────────────────────────────

pub struct PianeerApp {
    backend:       Box<dyn PianeerBackend>,
    settings_open: bool, // portrait mode: settings drawer toggle
    show_qr:       bool,
    /// Pre-computed QR modules (dark=true, row-major) and module count.
    qr_data:       Option<(Vec<bool>, usize)>,
    server_url:    String,
}

impl PianeerApp {
    pub fn new(backend: Box<dyn PianeerBackend>, server_url: String) -> Self {
        let qr_data = qrcode::QrCode::new(server_url.as_bytes()).ok().map(|code| {
            let n = code.width();
            let dark: Vec<bool> = (0..n)
                .flat_map(|y| (0..n).map(move |x| (y, x)))
                .map(|(y, x)| code[(y, x)] == qrcode::Color::Dark)
                .collect();
            (dark, n)
        });
        Self { backend, settings_open: false, show_qr: false, qr_data, server_url }
    }
}

impl PianeerApp {
    /// Drive the UI for one frame. Called by eframe's `update` and also
    /// directly by the KMS rendering loop (which has no `eframe::Frame`).
    pub fn update_egui(&mut self, ctx: &egui::Context) {
        self.backend.poll();
        let snap = self.backend.snapshot().clone();

        // Repaint at ~20 Hz so VU meters and playback progress stay live.
        ctx.request_repaint_after(Duration::from_millis(50));

        let portrait = ctx.screen_rect().width() < 520.0;

        if portrait {
            // Larger touch targets on mobile.
            ctx.style_mut(|s| s.spacing.interact_size.y = 36.0);
        }

        render_stats_bar(ctx, &snap, portrait);

        if portrait {
            render_settings_bottom(ctx, &snap, &mut *self.backend, &mut self.settings_open, &mut self.show_qr);
        } else {
            render_settings_panel(ctx, &snap, &mut *self.backend, &mut self.show_qr);
        }

        render_main(ctx, &snap, &mut *self.backend);

        if self.show_qr {
            render_qr_window(ctx, &self.server_url, &self.qr_data, &mut self.show_qr);
        }
    }
}

impl eframe::App for PianeerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_egui(ctx);
    }
}

// ── Stats / VU top bar ────────────────────────────────────────────────────────

fn render_stats_bar(ctx: &egui::Context, snap: &WebSnapshot, portrait: bool) {
    TopBottomPanel::top("stats").show(ctx, |ui| {
        ui.horizontal(|ui| {
            // ── Left: title + fixed-width stats ──────────────────────────────
            ui.label(
                RichText::new("Pianeer")
                    .font(FontId::proportional(16.0))
                    .color(Color32::from_rgb(128, 208, 255)),
            );
            ui.separator();

            let s = &snap.stats;
            // Each stat in its own fixed-width slot so changing one value
            // never shifts the others' positions.
            stat_slot(ui, format!("CPU {:3}%", s.cpu_pct), 68.0);
            if !portrait {
                stat_slot(ui, format!("{:4}MB", s.mem_mb), 58.0);
            }
            stat_slot(ui, format!("V{:2}", s.voices), 34.0);

            if s.clip {
                ui.label(RichText::new("CLIP").color(Color32::RED).strong());
            }
            if snap.sustain {
                ui.label(RichText::new("Sus").color(Color32::from_rgb(128, 208, 255)).strong());
            }
            if snap.recording {
                ui.label(
                    RichText::new(format!("● REC {}", fmt_time(snap.rec_elapsed_us.unwrap_or(0))))
                        .color(Color32::RED)
                        .strong(),
                );
            }

            // ── Right: VU meters pinned to the right edge ─────────────────
            let vu_w = if portrait { 44.0 } else { 80.0 };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                vu_meter(ui, "R", s.peak_r, vu_w);
                vu_meter(ui, "L", s.peak_l, vu_w);
            });
        });

        if let Some(ref pb) = snap.playback {
            ui.horizontal(|ui| {
                let icon = if pb.paused { "⏸" } else { "▶" };
                ui.label(format!("{} {} / {}", icon, fmt_time(pb.current_us), fmt_time(pb.total_us)));
                let frac = if pb.total_us > 0 {
                    pb.current_us as f32 / pb.total_us as f32
                } else {
                    0.0
                };
                let bar_w = (ui.available_width() - 8.0).max(40.0);
                let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, 8.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 3.0, Color32::from_gray(60));
                let mut fill = rect;
                fill.set_right(rect.left() + rect.width() * frac);
                ui.painter().rect_filled(fill, 3.0, Color32::from_rgb(80, 192, 80));
            });
        }
    });
}

/// Allocate a fixed-width slot for a monospace stat label.
/// The slot width is constant regardless of the value rendered inside it,
/// so adjacent stats never shift each other.
fn stat_slot(ui: &mut Ui, text: String, width: f32) {
    let h = ui.available_height();
    ui.add_sized(
        [width, h],
        egui::Label::new(RichText::new(text).font(FontId::monospace(13.0))),
    );
}

fn vu_meter(ui: &mut Ui, label: &str, peak: f32, width: f32) {
    let db = if peak > 1e-10 { 20.0 * peak.log10() } else { -60.0_f32 };
    let frac = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
    ui.label(label);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, Color32::from_gray(40));
    let mut fill = rect;
    fill.set_right(rect.left() + rect.width() * frac);
    let color = if db > -6.0 {
        Color32::RED
    } else if db > -12.0 {
        Color32::YELLOW
    } else {
        Color32::from_rgb(80, 192, 80)
    };
    ui.painter().rect_filled(fill, 2.0, color);
}

// ── Settings side panel (landscape / desktop) ─────────────────────────────────

fn render_settings_panel(ctx: &egui::Context, snap: &WebSnapshot, backend: &mut dyn PianeerBackend, show_qr: &mut bool) {
    SidePanel::right("settings").min_width(180.0).show(ctx, |ui| {
        ui.heading("Settings");
        ui.separator();

        settings_controls(ui, snap, backend);

        ui.separator();
        if let Some(ref pb) = snap.playback {
            playback_controls(ui, pb.paused, backend);
            ui.separator();
        }

        record_rescan(ui, snap.recording, backend);
        ui.separator();
        if ui.button("QR Code").clicked() { *show_qr = !*show_qr; }
    });
}

// ── Settings bottom panel (portrait / mobile) ─────────────────────────────────

fn render_settings_bottom(
    ctx: &egui::Context,
    snap: &WebSnapshot,
    backend: &mut dyn PianeerBackend,
    open: &mut bool,
    show_qr: &mut bool,
) {
    TopBottomPanel::bottom("settings_bottom").show(ctx, |ui| {
        // Toggle header row.
        ui.horizontal(|ui| {
            let arrow = if *open { "▲ Settings" } else { "▼ Settings" };
            if ui.button(arrow).clicked() {
                *open = !*open;
            }

            // Quick status badges always visible.
            ui.separator();
            let s = &snap.settings;
            ui.label(RichText::new(format!("{}  {}  {:+.0}dB  {:+}st",
                s.veltrack.label(), s.tune.label(), s.volume_db, s.transpose))
                .color(Color32::from_gray(160)).small());

            if snap.recording {
                ui.separator();
                ui.label(RichText::new(format!("● REC {}", fmt_time(snap.rec_elapsed_us.unwrap_or(0))))
                    .color(Color32::RED).strong());
            }
        });

        if *open {
            ui.separator();

            // Row 1: toggles and cycles.
            ui.horizontal_wrapped(|ui| {
                let s = &snap.settings;

                if ui.button(format!("Vel: {}", s.veltrack.label())).clicked() {
                    backend.send(ClientCmd::CycleVeltrack);
                }
                if ui.button(format!("Tune: {}", s.tune.label())).clicked() {
                    backend.send(ClientCmd::CycleTune);
                }

                let rel_btn = egui::Button::new(format!("Rel: {}", if s.release_enabled { "on" } else { "off" }))
                    .fill(if s.release_enabled { Color32::from_rgb(0, 60, 0) } else { Color32::from_gray(40) });
                if ui.add(rel_btn).clicked() { backend.send(ClientCmd::ToggleRelease); }

                let res_btn = egui::Button::new(format!("Res: {}", if s.resonance_enabled { "on" } else { "off" }))
                    .fill(if s.resonance_enabled { Color32::from_rgb(0, 60, 0) } else { Color32::from_gray(40) });
                if ui.add(res_btn).clicked() { backend.send(ClientCmd::ToggleResonance); }

                let rec_btn = egui::Button::new(if snap.recording { "■ Stop Rec" } else { "● Rec" })
                    .fill(if snap.recording { Color32::from_rgb(80, 0, 0) } else { Color32::from_gray(40) });
                if ui.add(rec_btn).clicked() { backend.send(ClientCmd::ToggleRecord); }

                if ui.button("⟳").clicked() { backend.send(ClientCmd::Rescan); }
                if ui.button("QR").clicked() { *show_qr = !*show_qr; }
            });

            // Row 2: steppers.
            ui.horizontal_wrapped(|ui| {
                let s = &snap.settings;

                ui.label(format!("Vol {:+.0}dB", s.volume_db));
                if ui.button("−").clicked() { backend.send(ClientCmd::VolumeChange { delta: -1 }); }
                if ui.button("+").clicked() { backend.send(ClientCmd::VolumeChange { delta:  1 }); }

                ui.separator();
                ui.label(format!("Trns {:+}", s.transpose));
                if ui.button("−").clicked() { backend.send(ClientCmd::TransposeChange { delta: -1 }); }
                if ui.button("+").clicked() { backend.send(ClientCmd::TransposeChange { delta:  1 }); }

                if let Some(ref pb) = snap.playback {
                    ui.separator();
                    playback_controls(ui, pb.paused, backend);
                }
            });
        }
    });
}

// ── Shared settings widgets ───────────────────────────────────────────────────

fn settings_controls(ui: &mut Ui, snap: &WebSnapshot, backend: &mut dyn PianeerBackend) {
    let s = &snap.settings;

    ui.horizontal(|ui| {
        ui.label("Vel:");
        if ui.button(s.veltrack.label()).clicked() { backend.send(ClientCmd::CycleVeltrack); }
    });
    ui.horizontal(|ui| {
        ui.label("Tune:");
        if ui.button(s.tune.label()).clicked() { backend.send(ClientCmd::CycleTune); }
    });
    ui.horizontal(|ui| {
        ui.label("Release:");
        let btn = egui::Button::new(if s.release_enabled { "on" } else { "off" })
            .fill(if s.release_enabled { Color32::from_rgb(0, 60, 0) } else { Color32::from_gray(40) });
        if ui.add(btn).clicked() { backend.send(ClientCmd::ToggleRelease); }
    });
    ui.horizontal(|ui| {
        ui.label(format!("Vol: {:+.0}dB", s.volume_db));
        if ui.small_button("−").clicked() { backend.send(ClientCmd::VolumeChange { delta: -1 }); }
        if ui.small_button("+").clicked() { backend.send(ClientCmd::VolumeChange { delta:  1 }); }
    });
    ui.horizontal(|ui| {
        ui.label(format!("Trns: {:+}", s.transpose));
        if ui.small_button("−").clicked() { backend.send(ClientCmd::TransposeChange { delta: -1 }); }
        if ui.small_button("+").clicked() { backend.send(ClientCmd::TransposeChange { delta:  1 }); }
    });
    ui.horizontal(|ui| {
        ui.label("Res:");
        let btn = egui::Button::new(if s.resonance_enabled { "on" } else { "off" })
            .fill(if s.resonance_enabled { Color32::from_rgb(0, 60, 0) } else { Color32::from_gray(40) });
        if ui.add(btn).clicked() { backend.send(ClientCmd::ToggleResonance); }
    });
}

fn playback_controls(ui: &mut Ui, paused: bool, backend: &mut dyn PianeerBackend) {
    let play_label = if paused { "▶ Resume" } else { "⏸ Pause" };
    if ui.button(play_label).clicked() { backend.send(ClientCmd::PauseResume); }
    if ui.button("−10s").clicked() { backend.send(ClientCmd::SeekRelative { secs: -10 }); }
    if ui.button("+10s").clicked() { backend.send(ClientCmd::SeekRelative { secs:  10 }); }
}

fn record_rescan(ui: &mut Ui, recording: bool, backend: &mut dyn PianeerBackend) {
    let rec_btn = egui::Button::new(if recording { "■ Stop Rec" } else { "● Record" })
        .fill(if recording { Color32::from_rgb(80, 0, 0) } else { Color32::from_gray(40) });
    if ui.add(rec_btn).clicked() { backend.send(ClientCmd::ToggleRecord); }
    ui.separator();
    if ui.button("⟳ Rescan").clicked() { backend.send(ClientCmd::Rescan); }
}

// ── Instrument / MIDI file list ───────────────────────────────────────────────

fn render_main(ctx: &egui::Context, snap: &WebSnapshot, backend: &mut dyn PianeerBackend) {
    CentralPanel::default().show(ctx, |ui| {
        ScrollArea::vertical().show(ui, |ui| {
            let mut last_was_instrument = true;

            for (i, item) in snap.menu.iter().enumerate() {
                let is_midi = item.type_label == "MID";

                if is_midi && last_was_instrument {
                    ui.separator();
                    ui.label(RichText::new("MIDI FILES").color(Color32::from_gray(100)).small());
                } else if !is_midi && i == 0 {
                    ui.label(RichText::new("INSTRUMENTS").color(Color32::from_gray(100)).small());
                }
                last_was_instrument = !is_midi;

                let bg = if item.cursor { Color32::from_rgb(27, 43, 64) } else { Color32::TRANSPARENT };
                let name_color = if item.loaded {
                    Color32::from_rgb(112, 200, 255)
                } else if item.playing {
                    Color32::from_rgb(112, 255, 170)
                } else {
                    Color32::from_gray(224)
                };

                egui::Frame::none().fill(bg).inner_margin(4.0).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let badge = if item.playing { "▶" } else if item.loaded { "✓" } else { "  " };
                        ui.label(RichText::new(badge).color(name_color));

                        ui.label(RichText::new(&item.type_label).color(Color32::from_gray(85)).small());

                        if ui.add(
                            egui::Label::new(RichText::new(&item.name).color(name_color))
                                .sense(egui::Sense::click()),
                        ).clicked() {
                            backend.send(ClientCmd::SelectAt { idx: i });
                        }

                        if let Some(ref vname) = item.variant {
                            if ui.small_button("◄").clicked() {
                                backend.send(ClientCmd::CycleVariantAt { idx: i, dir: -1 });
                            }
                            ui.label(RichText::new(vname.as_str()).color(Color32::from_gray(150)).small());
                            if ui.small_button("►").clicked() {
                                backend.send(ClientCmd::CycleVariantAt { idx: i, dir: 1 });
                            }
                        }
                    });
                });
            }
        });
    });
}

// ── QR code popup ─────────────────────────────────────────────────────────────

fn render_qr_window(
    ctx: &egui::Context,
    url: &str,
    qr_data: &Option<(Vec<bool>, usize)>,
    open: &mut bool,
) {
    egui::Window::new("QR Code")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            if let Some((dark, n)) = qr_data {
                let margin = 3usize;
                let module_size = 7.0_f32;
                let side = (*n + 2 * margin) as f32 * module_size;
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(side, side),
                    egui::Sense::hover(),
                );
                // White background (includes quiet zone).
                ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
                // Dark modules.
                for y in 0..*n {
                    for x in 0..*n {
                        if dark[y * n + x] {
                            let px = rect.left() + (x + margin) as f32 * module_size;
                            let py = rect.top()  + (y + margin) as f32 * module_size;
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(
                                    egui::pos2(px, py),
                                    egui::vec2(module_size, module_size),
                                ),
                                0.0,
                                Color32::BLACK,
                            );
                        }
                    }
                }
            } else {
                ui.label("QR generation failed.");
            }
            ui.separator();
            ui.label(RichText::new(url).color(Color32::from_rgb(128, 208, 255)));
            ui.separator();
            if ui.button("Close").clicked() {
                *open = false;
            }
        });
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fmt_time(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

trait LabelExt {
    fn label(&self) -> &'static str;
}

use pianeer_core::types::{TunePreset, VeltrackLevel};

impl LabelExt for VeltrackLevel {
    fn label(&self) -> &'static str { VeltrackLevel::label(*self) }
}

impl LabelExt for TunePreset {
    fn label(&self) -> &'static str { TunePreset::label(*self) }
}
