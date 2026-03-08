use std::time::Duration;

use egui::{CentralPanel, Color32, FontId, RichText, ScrollArea, SidePanel, TopBottomPanel, Ui};
use pianeer_core::snapshot::{ClientCmd, WebSnapshot};

use crate::{LocalBackend, PianeerBackend, RemoteBackend};

#[cfg(target_arch = "wasm32")]
use crate::wasm_audio::AudioPlayer;
#[cfg(not(target_arch = "wasm32"))]
use crate::remote_audio::RemoteAudioPlayer;

// ── App ───────────────────────────────────────────────────────────────────────

pub struct PianeerApp {
    backend:       Box<dyn PianeerBackend>,
    settings_open: bool, // portrait mode: settings drawer toggle
    show_qr:       bool,
    /// Pre-computed QR modules (dark=true, row-major) and module count.
    qr_data:       Option<(Vec<bool>, usize)>,
    server_url:    String,
    // Remote connection
    connect_input:  String,
    connect_error:  Option<String>,
    is_remote:      bool,
    /// LAN-discovered peers (label, ws_url). Updated by the discovery background thread.
    peers: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// Recreates the LocalBackend so the user can disconnect from a remote and return local.
    /// None on WASM (always remote) or when started with a RemoteBackend.
    local_restore: Option<Box<dyn Fn() -> Box<dyn PianeerBackend>>>,
    // WASM audio streaming
    #[cfg(target_arch = "wasm32")]
    audio_player:   Option<AudioPlayer>,
    // Native (Android/desktop) remote audio streaming
    #[cfg(not(target_arch = "wasm32"))]
    remote_audio:   Option<Box<dyn RemoteAudioPlayer>>,
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

        // Pre-fill the connect input with the current server's host:port.
        let connect_input = server_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();

        // WASM always starts connected to a remote backend.
        #[cfg(target_arch = "wasm32")]
        let is_remote = true;
        #[cfg(not(target_arch = "wasm32"))]
        let is_remote = false;

        Self {
            backend,
            settings_open: false,
            show_qr: false,
            qr_data,
            server_url,
            connect_input,
            connect_error: None,
            is_remote,
            peers: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            local_restore: None,
            #[cfg(target_arch = "wasm32")]
            audio_player: None,
            #[cfg(not(target_arch = "wasm32"))]
            remote_audio: None,
        }
    }

    /// Provide a platform audio player for remote streaming (Android/desktop).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_remote_audio(&mut self, player: Box<dyn RemoteAudioPlayer>) {
        self.remote_audio = Some(player);
    }

    pub fn is_remote(&self) -> bool { self.is_remote }

    /// Inject a shared peer list populated by a platform discovery mechanism (e.g. mDNS).
    pub fn set_peers(&mut self, peers: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        self.peers = peers;
    }

    /// Store the local backend construction parameters so the user can return
    /// to local after connecting to a remote instance.
    pub fn set_local_source(
        &mut self,
        snapshot: std::sync::Arc<std::sync::Mutex<String>>,
        action_tx: crossbeam_channel::Sender<pianeer_core::types::MenuAction>,
    ) {
        self.local_restore = Some(Box::new(move || {
            Box::new(LocalBackend::new(
                std::sync::Arc::clone(&snapshot),
                action_tx.clone(),
            ))
        }));
    }

    /// Current value of the remote-connect text field (for pre-filling dialogs).
    pub fn get_connect_input(&self) -> &str {
        &self.connect_input
    }

    /// Accept a text result from a platform dialog, update connect_input and connect.
    pub fn handle_connect_result(&mut self, text: String) {
        self.connect_input = text.clone();
        do_connect(
            &mut self.backend,
            &text,
            &mut self.is_remote,
            &mut self.connect_error,
            &mut self.server_url,
        );
    }
}

impl PianeerApp {
    /// Drive the UI for one frame. Called by eframe's `update` and also
    /// directly by the KMS rendering loop (which has no `eframe::Frame`).
    pub fn update_egui(&mut self, ctx: &egui::Context) {
        self.backend.poll();
        let snap = self.backend.snapshot().clone();

        // Poll audio player if streaming.
        #[cfg(target_arch = "wasm32")]
        if let Some(ref mut ap) = self.audio_player {
            ap.poll();
        }

        // Repaint frequently only when something is visually changing.
        let animating = snap.playback.is_some()
            || snap.loading.is_some()
            || snap.stats.voices > 0
            || snap.stats.peak_l > 0.001
            || snap.stats.peak_r > 0.001
            || snap.rec_elapsed_us.is_some();
        // Audio scheduling is driven by a JS timer, not the repaint cycle.
        ctx.request_repaint_after(Duration::from_millis(if animating { 50 } else { 500 }));

        let portrait = ctx.screen_rect().width() < 520.0;

        if portrait {
            ctx.style_mut(|s| s.spacing.interact_size.y = 36.0);
        }

        render_stats_bar(ctx, &snap, portrait);

        // WASM audio button — thin strip at the bottom.
        #[cfg(target_arch = "wasm32")]
        render_audio_bar(ctx, &self.server_url, &mut self.audio_player);

        let can_go_local = self.local_restore.is_some();
        let want_local = if portrait {
            render_settings_bottom(
                ctx, &snap, &mut self.backend,
                &mut self.settings_open, &mut self.show_qr,
                &mut self.connect_input, &mut self.is_remote,
                &mut self.connect_error, &mut self.server_url,
                &self.peers, can_go_local,
            )
        } else {
            render_settings_panel(
                ctx, &snap, &mut self.backend, &mut self.show_qr,
                &mut self.connect_input, &mut self.is_remote,
                &mut self.connect_error, &mut self.server_url,
                &self.peers, can_go_local,
            )
        };
        if want_local {
            if let Some(ref restore) = self.local_restore {
                self.backend = restore();
                self.is_remote = false;
                self.connect_error = None;
            }
        }

        render_main(ctx, &snap, &mut *self.backend);

        if self.show_qr {
            render_qr_window(ctx, &self.server_url, &self.qr_data, &mut self.show_qr);
        }

        // Native remote audio floating button (Android/desktop) — rendered last
        // so it floats above all panels, clear of the system navigation bar.
        #[cfg(not(target_arch = "wasm32"))]
        if self.is_remote {
            if let Some(ref mut player) = self.remote_audio {
                let server_url = self.server_url.clone();
                render_listen_window(ctx, &server_url, &mut **player);
            }
        }
    }
}

impl eframe::App for PianeerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_egui(ctx);
    }
}

// ── WASM audio bar ────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
fn render_audio_bar(ctx: &egui::Context, server_url: &str, player: &mut Option<AudioPlayer>) {
    TopBottomPanel::bottom("audio_ctrl").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if player.is_none() {
                if ui.button("🔊 Listen").clicked() {
                    let ws_url = format!(
                        "{}/audio-pcm-ws",
                        server_url
                            .replacen("http://", "ws://", 1)
                            .replacen("https://", "wss://", 1)
                    );
                    if let Ok(p) = AudioPlayer::new(&ws_url) {
                        *player = Some(p);
                    }
                }
            } else {
                if ui.button("🔇").clicked() {
                    *player = None;
                }
                ui.label(
                    RichText::new("● Live")
                        .color(Color32::from_rgb(80, 192, 80))
                        .strong(),
                );
            }
        });
    });
}

// ── Native remote audio floating window ──────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
fn render_listen_window(ctx: &egui::Context, server_url: &str, player: &mut dyn RemoteAudioPlayer) {
    egui::Window::new("listen_ctrl")
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, [-8.0, -72.0])
        .resizable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if player.is_active() {
                    if ui.button("🔇").clicked() {
                        player.disconnect();
                    }
                    ui.label(
                        RichText::new("● Live")
                            .color(Color32::from_rgb(80, 192, 80))
                            .strong(),
                    );
                } else {
                    if ui.button("🔊 Listen").clicked() {
                        let ws_url = format!(
                            "{}/audio-pcm-ws",
                            server_url
                                .replacen("http://", "ws://", 1)
                                .replacen("https://", "wss://", 1)
                        );
                        player.connect(&ws_url);
                    }
                }
            });
        });
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

fn render_settings_panel(
    ctx: &egui::Context,
    snap: &WebSnapshot,
    backend: &mut Box<dyn PianeerBackend>,
    show_qr: &mut bool,
    connect_input: &mut String,
    is_remote: &mut bool,
    connect_error: &mut Option<String>,
    server_url: &mut String,
    peers: &std::sync::Mutex<Vec<(String, String)>>,
    can_go_local: bool,
) -> bool {
    let mut want_local = false;
    SidePanel::right("settings").min_width(180.0).show(ctx, |ui| {
        ui.heading("Settings");
        ui.separator();

        settings_controls(ui, snap, &mut **backend);

        ui.separator();
        if let Some(ref pb) = snap.playback {
            playback_controls(ui, pb.paused, &mut **backend);
            ui.separator();
        }

        record_rescan(ui, snap.recording, &mut **backend);
        ui.separator();
        if ui.button("QR Code").clicked() { *show_qr = !*show_qr; }

        ui.separator();
        want_local = connect_section(ui, backend, connect_input, is_remote,
                                     connect_error, server_url, peers, can_go_local);
    });
    want_local
}

// ── Settings bottom panel (portrait / mobile) ─────────────────────────────────

fn render_settings_bottom(
    ctx: &egui::Context,
    snap: &WebSnapshot,
    backend: &mut Box<dyn PianeerBackend>,
    open: &mut bool,
    show_qr: &mut bool,
    connect_input: &mut String,
    is_remote: &mut bool,
    connect_error: &mut Option<String>,
    server_url: &mut String,
    peers: &std::sync::Mutex<Vec<(String, String)>>,
    can_go_local: bool,
) -> bool {
    let mut want_local = false;
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
                    playback_controls(ui, pb.paused, &mut **backend);
                }
            });

            // Row 3: remote connect.
            ui.separator();
            if connect_section_compact(ui, backend, connect_input, is_remote,
                                       connect_error, server_url, peers, can_go_local) {
                want_local = true;
            }
        }
    });
    want_local
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

// ── Remote connect UI ─────────────────────────────────────────────────────────

/// Full connect section for the landscape side panel.
/// Returns true if the user requested disconnecting back to local.
fn connect_section(
    ui: &mut Ui,
    backend: &mut Box<dyn PianeerBackend>,
    input: &mut String,
    is_remote: &mut bool,
    error: &mut Option<String>,
    server_url: &mut String,
    peers: &std::sync::Mutex<Vec<(String, String)>>,
    can_go_local: bool,
) -> bool {
    let mut want_local = false;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(if *is_remote { "Remote ✓" } else { "Remote" })
                .color(if *is_remote { Color32::from_rgb(80, 192, 80) } else { Color32::from_gray(180) })
                .small(),
        );
        if *is_remote && can_go_local {
            if ui.small_button("← Local").clicked() {
                want_local = true;
            }
        }
    });

    // Discovered peers (mDNS).
    let discovered: Vec<(String, String)> = peers.lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !discovered.is_empty() {
        ui.label(RichText::new("Discovered").color(Color32::from_gray(130)).small());
        for (label, ws_url) in &discovered {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).color(Color32::from_gray(220)));
                if ui.small_button("Connect").clicked() {
                    *input = ws_url
                        .trim_start_matches("ws://")
                        .trim_end_matches("/ws")
                        .to_string();
                    do_connect(backend, input, is_remote, error, server_url);
                }
            });
        }
        ui.separator();
    }

    // Manual entry.
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(input)
            .hint_text("host:4000")
            .desired_width(120.0));
        if ui.button("Connect").clicked() {
            do_connect(backend, input, is_remote, error, server_url);
        }
    });
    if let Some(ref e) = *error {
        ui.label(RichText::new(e.as_str()).color(Color32::RED).small());
    }
    want_local
}

/// Compact connect row for the portrait bottom drawer.
/// Returns true if the user requested disconnecting back to local.
fn connect_section_compact(
    ui: &mut Ui,
    backend: &mut Box<dyn PianeerBackend>,
    input: &mut String,
    is_remote: &mut bool,
    error: &mut Option<String>,
    server_url: &mut String,
    peers: &std::sync::Mutex<Vec<(String, String)>>,
    can_go_local: bool,
) -> bool {
    let mut want_local = false;
    // Discovered peers (one button each).
    let discovered: Vec<(String, String)> = peers.lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !discovered.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("🔍").color(Color32::from_gray(130)));
            for (label, ws_url) in &discovered {
                if ui.button(RichText::new(label).small()).clicked() {
                    *input = ws_url
                        .trim_start_matches("ws://")
                        .trim_end_matches("/ws")
                        .to_string();
                    do_connect(backend, input, is_remote, error, server_url);
                }
            }
        });
    }

    ui.horizontal_wrapped(|ui| {
        let label = if *is_remote { "🌐 ✓" } else { "🌐" };
        ui.label(RichText::new(label).color(if *is_remote {
            Color32::from_rgb(80, 192, 80)
        } else {
            Color32::from_gray(160)
        }));
        if *is_remote && can_go_local {
            if ui.small_button("← Local").clicked() {
                want_local = true;
            }
        }
        ui.add(egui::TextEdit::singleline(input)
            .hint_text("host:4000")
            .desired_width(110.0));
        if ui.button("Connect").clicked() {
            do_connect(backend, input, is_remote, error, server_url);
        }
        if let Some(ref e) = *error {
            ui.label(RichText::new(e.as_str()).color(Color32::RED).small());
        }
    });
    want_local
}

/// True if `host_port` refers to this machine (loopback or LAN self-address).
fn is_local_address(host_port: &str) -> bool {
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" {
        return true;
    }
    // Check against the outbound LAN IP (the same trick used for QR code generation).
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("1.1.1.1:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if addr.ip().to_string() == host {
                    return true;
                }
            }
        }
    }
    false
}

/// Replace the backend with a new RemoteBackend pointing at `input`.
fn do_connect(
    backend: &mut Box<dyn PianeerBackend>,
    input: &str,
    is_remote: &mut bool,
    error: &mut Option<String>,
    server_url: &mut String,
) {
    let host = input.trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{}:4000", host)
    };

    // Refuse self-connection: connecting to localhost or our own LAN IP would
    // silently replace the efficient LocalBackend with a WebSocket round-trip
    // to ourselves with no way back.
    if !*is_remote && is_local_address(&host_port) {
        *error = Some("Already running locally — no need to connect.".to_string());
        return;
    }

    let ws_url = format!("ws://{}/ws", host_port);
    match RemoteBackend::connect(&ws_url) {
        Ok(new_backend) => {
            *backend    = Box::new(new_backend);
            *is_remote  = true;
            *server_url = format!("http://{}", host_port);
            *error      = None;
        }
        Err(e) => { *error = Some(e); }
    }
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
                        let is_loading = snap.loading.map_or(false, |(li, _)| li == i);
                        let badge = if is_loading { "⟳" } else if item.playing { "▶" } else if item.loaded { "✓" } else { "  " };
                        let badge_color = if is_loading { Color32::from_rgb(80, 160, 255) } else { name_color };
                        ui.label(RichText::new(badge).color(badge_color));

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

                        if let Some((_, pct)) = snap.loading.filter(|(li, _)| *li == i) {
                            let bar_w = 72.0_f32.min(ui.available_width() - 4.0).max(10.0);
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, 8.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 3.0, Color32::from_gray(50));
                            let mut fill = rect;
                            fill.set_right(rect.left() + rect.width() * pct as f32 / 100.0);
                            ui.painter().rect_filled(fill, 3.0, Color32::from_rgb(60, 140, 255));
                            ui.label(RichText::new(format!("{}%", pct)).color(Color32::from_gray(180)).small());
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
                ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
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
