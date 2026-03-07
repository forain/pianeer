// DRM/KMS UI path — Linux headless rendering via dumb-buffer + software egui rasterizer.
//
// Used automatically when neither WAYLAND_DISPLAY nor DISPLAY is set and at
// least one connected DRM output is found.  Requires the `kms` feature flag.
//
// Architecture
// ───────────
//   • main thread  : DRM setup → egui render loop (evdev input → paint → flip)
//   • bg thread    : action dispatch / snapshot (mirrors gui.rs background thread)

use std::collections::HashMap;
use std::os::unix::io::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use drm::buffer::DrmFourcc;
use drm::control::{connector, crtc, Device as ControlDevice};
use evdev::{Device as EvdevDevice, EventSummary, KeyCode, RelativeAxisCode};

use pianeer_core::dispatch::{apply_settings_action, rebuild_menu};
use pianeer_core::loader::{LoadingJob, poll_loading_job, save_last_instrument_path};
use pianeer_core::midi_recorder::RecordHandle;
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::StatsCollector;
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackState, ProcStats, Settings,
    playing_idx, stop_playback, pause_resume, seek_relative,
};
use pianeer_core::{midi_player, midi_recorder};
use pianeer_egui::{LocalBackend, PianeerApp};

// ── DRM card wrapper ──────────────────────────────────────────────────────────

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

struct DisplayInfo {
    crtc:       crtc::Handle,
    connector:  connector::Handle,
    mode:       drm::control::Mode,
    width:      u32,
    height:     u32,
    saved_crtc: Option<drm::control::crtc::Info>,
}

// ── Public helpers ────────────────────────────────────────────────────────────

/// True when no display server env vars are set AND a usable DRM device exists.
pub fn drm_available() -> bool {
    !has_display_env() && find_card_and_display().is_some()
}

fn has_display_env() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn find_card_and_display() -> Option<(Card, DisplayInfo)> {
    for i in 0..8 {
        let path = format!("/dev/dri/card{}", i);
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).ok()?;
        let card = Card(f);
        let Ok(res) = card.resource_handles() else { continue };

        for &ch in res.connectors() {
            let Ok(conn) = card.get_connector(ch, true) else { continue };
            if conn.state() != connector::State::Connected || conn.modes().is_empty() {
                continue;
            }
            let mode = conn.modes()[0];
            let (w, h) = (mode.size().0 as u32, mode.size().1 as u32);

            let Some(enc_h) = conn.current_encoder() else { continue };
            let Ok(enc)     = card.get_encoder(enc_h) else { continue };
            let Some(crtc_h) = enc.crtc()             else { continue };

            let saved = card.get_crtc(crtc_h).ok();
            return Some((card, DisplayInfo {
                crtc: crtc_h, connector: ch, mode, width: w, height: h, saved_crtc: saved,
            }));
        }
    }
    None
}

// ── Texture storage ───────────────────────────────────────────────────────────

struct EguiTexture {
    data:   Vec<u8>, // RGBA, straight alpha
    width:  usize,
    height: usize,
}

fn apply_texture_delta(
    textures: &mut HashMap<egui::TextureId, EguiTexture>,
    delta:    &egui::TexturesDelta,
) {
    for (id, img_delta) in &delta.set {
        let (rgba, w, h) = match &img_delta.image {
            egui::ImageData::Color(img) => {
                let d: Vec<u8> = img.pixels.iter()
                    .flat_map(|c| [c.r(), c.g(), c.b(), c.a()])
                    .collect();
                (d, img.width(), img.height())
            }
            egui::ImageData::Font(img) => {
                // Alpha-only coverage → white glyph with that alpha.
                let d: Vec<u8> = img.pixels.iter()
                    .flat_map(|&cov| { let a = (cov * 255.0).round() as u8; [255u8, 255, 255, a] })
                    .collect();
                (d, img.width(), img.height())
            }
        };

        if let Some([px, py]) = img_delta.pos {
            if let Some(tex) = textures.get_mut(id) {
                for row in 0..h {
                    let dst = ((py + row) * tex.width + px) * 4;
                    let src = row * w * 4;
                    tex.data[dst..dst + w * 4].copy_from_slice(&rgba[src..src + w * 4]);
                }
            }
        } else {
            textures.insert(*id, EguiTexture { data: rgba, width: w, height: h });
        }
    }
    for id in &delta.free {
        textures.remove(id);
    }
}

// ── Software rasterizer ───────────────────────────────────────────────────────
//
// Renders egui's tessellated output into a `u32` pixel buffer in DRM
// Xrgb8888 format (0x00RRGGBB on little-endian).

#[inline(always)]
fn edge_fn(a: egui::Pos2, b: egui::Pos2, p: egui::Pos2) -> f32 {
    (p.x - a.x) * (b.y - a.y) - (p.y - a.y) * (b.x - a.x)
}

fn render_mesh(
    pixels:   &mut [u32],
    screen_w: usize,
    screen_h: usize,
    clip:     egui::Rect,
    mesh:     &egui::Mesh,
    textures: &HashMap<egui::TextureId, EguiTexture>,
) {
    let tex  = textures.get(&mesh.texture_id);
    let cx0  = clip.min.x.max(0.0) as i32;
    let cy0  = clip.min.y.max(0.0) as i32;
    let cx1  = clip.max.x.min(screen_w as f32).ceil() as i32;
    let cy1  = clip.max.y.min(screen_h as f32).ceil() as i32;
    let sw   = screen_w as i32;

    for tri in mesh.indices.chunks_exact(3) {
        let v0 = &mesh.vertices[tri[0] as usize];
        let v1 = &mesh.vertices[tri[1] as usize];
        let v2 = &mesh.vertices[tri[2] as usize];

        let area = edge_fn(v0.pos, v1.pos, v2.pos);
        if area.abs() < 0.5 { continue; }
        let inv = 1.0 / area;

        let bbx0 = (v0.pos.x.min(v1.pos.x).min(v2.pos.x) as i32).max(cx0);
        let bby0 = (v0.pos.y.min(v1.pos.y).min(v2.pos.y) as i32).max(cy0);
        let bbx1 = (v0.pos.x.max(v1.pos.x).max(v2.pos.x).ceil() as i32).min(cx1);
        let bby1 = (v0.pos.y.max(v1.pos.y).max(v2.pos.y).ceil() as i32).min(cy1);
        if bbx0 >= bbx1 || bby0 >= bby1 { continue; }

        for py in bby0..bby1 {
            for px in bbx0..bbx1 {
                let p  = egui::pos2(px as f32 + 0.5, py as f32 + 0.5);
                let w0 = edge_fn(v1.pos, v2.pos, p) * inv;
                let w1 = edge_fn(v2.pos, v0.pos, p) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 { continue; }

                // Interpolate vertex colour (straight alpha).
                let cr = (v0.color.r() as f32 * w0 + v1.color.r() as f32 * w1 + v2.color.r() as f32 * w2) as u32;
                let cg = (v0.color.g() as f32 * w0 + v1.color.g() as f32 * w1 + v2.color.g() as f32 * w2) as u32;
                let cb = (v0.color.b() as f32 * w0 + v1.color.b() as f32 * w1 + v2.color.b() as f32 * w2) as u32;
                let ca = (v0.color.a() as f32 * w0 + v1.color.a() as f32 * w1 + v2.color.a() as f32 * w2) as u32;

                // Sample texture (nearest neighbour).
                let (tr, tg, tb, ta) = if let Some(t) = tex {
                    let u  = v0.uv.x * w0 + v1.uv.x * w1 + v2.uv.x * w2;
                    let v  = v0.uv.y * w0 + v1.uv.y * w1 + v2.uv.y * w2;
                    let tx = ((u * t.width  as f32) as usize).min(t.width.saturating_sub(1));
                    let ty = ((v * t.height as f32) as usize).min(t.height.saturating_sub(1));
                    let o  = (ty * t.width + tx) * 4;
                    (t.data[o] as u32, t.data[o+1] as u32, t.data[o+2] as u32, t.data[o+3] as u32)
                } else {
                    (255, 255, 255, 255)
                };

                // Modulate: vertex_color × texture_color / 255.
                let sr = (cr * tr / 255).min(255);
                let sg = (cg * tg / 255).min(255);
                let sb = (cb * tb / 255).min(255);
                let sa = (ca * ta / 255).min(255);
                if sa == 0 { continue; }

                let idx = py as usize * sw as usize + px as usize;
                if sa >= 255 {
                    pixels[idx] = (sr << 16) | (sg << 8) | sb;
                } else {
                    let dst  = pixels[idx];
                    let dr   = (dst >> 16) & 0xFF;
                    let dg   = (dst >>  8) & 0xFF;
                    let db   =  dst        & 0xFF;
                    let ia   = 255 - sa;
                    pixels[idx] = (((sr * sa + dr * ia) / 255) << 16)
                                | (((sg * sa + dg * ia) / 255) <<  8)
                                |  ((sb * sa + db * ia) / 255);
                }
            }
        }
    }
}

// ── evdev input ───────────────────────────────────────────────────────────────

struct EvdevInput {
    devices:   Vec<EvdevDevice>,
    modifiers: egui::Modifiers,
}

impl EvdevInput {
    fn new() -> Self {
        let mut devices = Vec::new();
        for i in 0..32 {
            let path = format!("/dev/input/event{}", i);
            let Ok(mut dev) = EvdevDevice::open(&path) else { continue };
            let has_keys = dev.supported_keys().is_some();
            let has_rel  = dev.supported_relative_axes().is_some();
            if !has_keys && !has_rel { continue; }
            let _ = dev.set_nonblocking(true);
            if dev.grab().is_err() {
                eprintln!("KMS: could not grab {}", path);
            }
            devices.push(dev);
        }
        Self { devices, modifiers: egui::Modifiers::default() }
    }

    fn collect(&mut self, sw: f32, sh: f32, cursor: &mut egui::Pos2) -> egui::RawInput {
        let mut events = Vec::new();
        for dev in &mut self.devices {
            match dev.fetch_events() {
                Ok(evts) => {
                    for ev in evts {
                        match ev.destructure() {
                            EventSummary::Key(_, code, value) => {
                                let pressed = value != 0;
                                let repeat  = value == 2;
                                // Update modifier state inline (avoids double borrow).
                                match code {
                                    KeyCode::KEY_LEFTSHIFT  | KeyCode::KEY_RIGHTSHIFT  => self.modifiers.shift = pressed,
                                    KeyCode::KEY_LEFTCTRL   | KeyCode::KEY_RIGHTCTRL   => self.modifiers.ctrl  = pressed,
                                    KeyCode::KEY_LEFTALT    | KeyCode::KEY_RIGHTALT    => self.modifiers.alt   = pressed,
                                    _ => {}
                                }
                                let mods = self.modifiers;
                                if let Some(btn) = key_to_ptr_btn(code) {
                                    events.push(egui::Event::PointerButton {
                                        pos: *cursor, button: btn,
                                        pressed, modifiers: mods,
                                    });
                                } else if let Some(key) = key_to_egui(code) {
                                    if pressed || repeat {
                                        events.push(egui::Event::Key {
                                            key, physical_key: None,
                                            pressed, repeat,
                                            modifiers: mods,
                                        });
                                    }
                                    if pressed && !repeat {
                                        if let Some(ch) = key_to_char(code, mods.shift) {
                                            events.push(egui::Event::Text(ch.to_string()));
                                        }
                                    }
                                }
                            }
                            EventSummary::RelativeAxis(_, code, value) => {
                                let mods = self.modifiers;
                                match code {
                                    RelativeAxisCode::REL_X =>
                                        cursor.x = (cursor.x + value as f32 * 0.8).clamp(0.0, sw - 1.0),
                                    RelativeAxisCode::REL_Y =>
                                        cursor.y = (cursor.y + value as f32 * 0.8).clamp(0.0, sh - 1.0),
                                    RelativeAxisCode::REL_WHEEL =>
                                        events.push(egui::Event::MouseWheel {
                                            unit: egui::MouseWheelUnit::Line,
                                            delta: egui::vec2(0.0, value as f32),
                                            modifiers: mods,
                                        }),
                                    _ => {}
                                }
                                events.push(egui::Event::PointerMoved(*cursor));
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0), egui::vec2(sw, sh),
            )),
            events,
            modifiers: self.modifiers,
            ..Default::default()
        }
    }

}

fn key_to_ptr_btn(code: KeyCode) -> Option<egui::PointerButton> {
    match code {
        KeyCode::BTN_LEFT   => Some(egui::PointerButton::Primary),
        KeyCode::BTN_RIGHT  => Some(egui::PointerButton::Secondary),
        KeyCode::BTN_MIDDLE => Some(egui::PointerButton::Middle),
        _ => None,
    }
}

fn key_to_egui(code: KeyCode) -> Option<egui::Key> {
    Some(match code {
        KeyCode::KEY_UP       => egui::Key::ArrowUp,
        KeyCode::KEY_DOWN     => egui::Key::ArrowDown,
        KeyCode::KEY_LEFT     => egui::Key::ArrowLeft,
        KeyCode::KEY_RIGHT    => egui::Key::ArrowRight,
        KeyCode::KEY_ENTER | KeyCode::KEY_KPENTER => egui::Key::Enter,
        KeyCode::KEY_ESC      => egui::Key::Escape,
        KeyCode::KEY_BACKSPACE=> egui::Key::Backspace,
        KeyCode::KEY_DELETE   => egui::Key::Delete,
        KeyCode::KEY_TAB      => egui::Key::Tab,
        KeyCode::KEY_HOME     => egui::Key::Home,
        KeyCode::KEY_END      => egui::Key::End,
        KeyCode::KEY_PAGEUP   => egui::Key::PageUp,
        KeyCode::KEY_PAGEDOWN => egui::Key::PageDown,
        KeyCode::KEY_SPACE    => egui::Key::Space,
        KeyCode::KEY_A => egui::Key::A, KeyCode::KEY_B => egui::Key::B,
        KeyCode::KEY_C => egui::Key::C, KeyCode::KEY_D => egui::Key::D,
        KeyCode::KEY_E => egui::Key::E, KeyCode::KEY_F => egui::Key::F,
        KeyCode::KEY_G => egui::Key::G, KeyCode::KEY_H => egui::Key::H,
        KeyCode::KEY_I => egui::Key::I, KeyCode::KEY_J => egui::Key::J,
        KeyCode::KEY_K => egui::Key::K, KeyCode::KEY_L => egui::Key::L,
        KeyCode::KEY_M => egui::Key::M, KeyCode::KEY_N => egui::Key::N,
        KeyCode::KEY_O => egui::Key::O, KeyCode::KEY_P => egui::Key::P,
        KeyCode::KEY_Q => egui::Key::Q, KeyCode::KEY_R => egui::Key::R,
        KeyCode::KEY_S => egui::Key::S, KeyCode::KEY_T => egui::Key::T,
        KeyCode::KEY_U => egui::Key::U, KeyCode::KEY_V => egui::Key::V,
        KeyCode::KEY_W => egui::Key::W, KeyCode::KEY_X => egui::Key::X,
        KeyCode::KEY_Y => egui::Key::Y, KeyCode::KEY_Z => egui::Key::Z,
        KeyCode::KEY_0 => egui::Key::Num0, KeyCode::KEY_1 => egui::Key::Num1,
        KeyCode::KEY_2 => egui::Key::Num2, KeyCode::KEY_3 => egui::Key::Num3,
        KeyCode::KEY_4 => egui::Key::Num4, KeyCode::KEY_5 => egui::Key::Num5,
        KeyCode::KEY_6 => egui::Key::Num6, KeyCode::KEY_7 => egui::Key::Num7,
        KeyCode::KEY_8 => egui::Key::Num8, KeyCode::KEY_9 => egui::Key::Num9,
        KeyCode::KEY_F1  => egui::Key::F1,  KeyCode::KEY_F2  => egui::Key::F2,
        KeyCode::KEY_F3  => egui::Key::F3,  KeyCode::KEY_F4  => egui::Key::F4,
        KeyCode::KEY_F5  => egui::Key::F5,  KeyCode::KEY_F6  => egui::Key::F6,
        KeyCode::KEY_F7  => egui::Key::F7,  KeyCode::KEY_F8  => egui::Key::F8,
        KeyCode::KEY_F9  => egui::Key::F9,  KeyCode::KEY_F10 => egui::Key::F10,
        KeyCode::KEY_F11 => egui::Key::F11, KeyCode::KEY_F12 => egui::Key::F12,
        _ => return None,
    })
}

fn key_to_char(code: KeyCode, shift: bool) -> Option<char> {
    Some(if shift {
        match code {
            KeyCode::KEY_A => 'A', KeyCode::KEY_B => 'B', KeyCode::KEY_C => 'C',
            KeyCode::KEY_D => 'D', KeyCode::KEY_E => 'E', KeyCode::KEY_F => 'F',
            KeyCode::KEY_G => 'G', KeyCode::KEY_H => 'H', KeyCode::KEY_I => 'I',
            KeyCode::KEY_J => 'J', KeyCode::KEY_K => 'K', KeyCode::KEY_L => 'L',
            KeyCode::KEY_M => 'M', KeyCode::KEY_N => 'N', KeyCode::KEY_O => 'O',
            KeyCode::KEY_P => 'P', KeyCode::KEY_Q => 'Q', KeyCode::KEY_R => 'R',
            KeyCode::KEY_S => 'S', KeyCode::KEY_T => 'T', KeyCode::KEY_U => 'U',
            KeyCode::KEY_V => 'V', KeyCode::KEY_W => 'W', KeyCode::KEY_X => 'X',
            KeyCode::KEY_Y => 'Y', KeyCode::KEY_Z => 'Z',
            KeyCode::KEY_1 => '!', KeyCode::KEY_2 => '@', KeyCode::KEY_3 => '#',
            KeyCode::KEY_4 => '$', KeyCode::KEY_5 => '%', KeyCode::KEY_6 => '^',
            KeyCode::KEY_7 => '&', KeyCode::KEY_8 => '*', KeyCode::KEY_9 => '(',
            KeyCode::KEY_0 => ')', KeyCode::KEY_MINUS => '_', KeyCode::KEY_EQUAL => '+',
            _ => return None,
        }
    } else {
        match code {
            KeyCode::KEY_A => 'a', KeyCode::KEY_B => 'b', KeyCode::KEY_C => 'c',
            KeyCode::KEY_D => 'd', KeyCode::KEY_E => 'e', KeyCode::KEY_F => 'f',
            KeyCode::KEY_G => 'g', KeyCode::KEY_H => 'h', KeyCode::KEY_I => 'i',
            KeyCode::KEY_J => 'j', KeyCode::KEY_K => 'k', KeyCode::KEY_L => 'l',
            KeyCode::KEY_M => 'm', KeyCode::KEY_N => 'n', KeyCode::KEY_O => 'o',
            KeyCode::KEY_P => 'p', KeyCode::KEY_Q => 'q', KeyCode::KEY_R => 'r',
            KeyCode::KEY_S => 's', KeyCode::KEY_T => 't', KeyCode::KEY_U => 'u',
            KeyCode::KEY_V => 'v', KeyCode::KEY_W => 'w', KeyCode::KEY_X => 'x',
            KeyCode::KEY_Y => 'y', KeyCode::KEY_Z => 'z',
            KeyCode::KEY_0 => '0', KeyCode::KEY_1 => '1', KeyCode::KEY_2 => '2',
            KeyCode::KEY_3 => '3', KeyCode::KEY_4 => '4', KeyCode::KEY_5 => '5',
            KeyCode::KEY_6 => '6', KeyCode::KEY_7 => '7', KeyCode::KEY_8 => '8',
            KeyCode::KEY_9 => '9', KeyCode::KEY_SPACE => ' ',
            KeyCode::KEY_MINUS => '-', KeyCode::KEY_EQUAL => '=',
            KeyCode::KEY_BACKSLASH => '\\', KeyCode::KEY_SLASH => '/',
            KeyCode::KEY_DOT => '.', KeyCode::KEY_COMMA => ',',
            KeyCode::KEY_SEMICOLON => ';', KeyCode::KEY_APOSTROPHE => '\'',
            KeyCode::KEY_LEFTBRACE => '[', KeyCode::KEY_RIGHTBRACE => ']',
            _ => return None,
        }
    })
}

// ── LAN server URL ────────────────────────────────────────────────────────────

fn lan_server_url() -> String {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0").ok()
        .and_then(|s| { let _ = s.connect("1.1.1.1:80"); s.local_addr().ok() })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "localhost".to_string());
    format!("http://{}:4000", ip)
}

// ── Action dispatch (mirrors gui.rs background thread) ────────────────────────

fn dispatch_loop(
    mut menu:            Vec<MenuItem>,
    state:               Arc<Mutex<SamplerState>>,
    midi_tx:             Sender<MidiEvent>,
    record_handle:       RecordHandle,
    midi_dir:            Option<PathBuf>,
    samples_dir:         PathBuf,
    web_snapshot:        Arc<Mutex<String>>,
    action_rx:           Receiver<MenuAction>,
    auto_tune_init:      f32,
    quit:                Arc<AtomicBool>,
    reload:              Arc<AtomicBool>,
    initial_loading_job: Option<(usize, LoadingJob)>,
) {
    let state_file = samples_dir.join(".last_instrument");
    let mut cursor   = initial_loading_job.as_ref().map(|(idx, _)| *idx).unwrap_or(0);
    let mut current  = 0usize;
    let mut current_path = if initial_loading_job.is_some() {
        PathBuf::new()
    } else {
        menu.iter()
            .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
            .unwrap_or_default()
    };
    let mut playback: Option<PlaybackState> = None;
    let mut settings = Settings::default();
    let mut auto_tune = auto_tune_init;
    let mut stats_collector = StatsCollector::new();
    let mut stats = ProcStats::default();
    let mut sustain = false;
    let mut loading_job = initial_loading_job;

    loop {
        std::thread::sleep(Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            stop_playback(&mut playback);
            break;
        }

        // ── Stats ─────────────────────────────────────────────────────────────
        (stats, sustain) = stats_collector.tick(&state);

        // ── Poll in-progress instrument load ──────────────────────────────────
        if let Some((new_idx, new_path, new_at)) =
            poll_loading_job(&mut loading_job, &state, &settings, &menu)
        {
            current = new_idx;
            save_last_instrument_path(&state_file, &new_path);
            current_path = new_path;
            auto_tune = new_at;
        }

        // ── Snapshot ──────────────────────────────────────────────────────────
        let loading_info = loading_job.as_ref().map(|(idx, job)| (*idx, job.pct()));
        let pb_info = playback.as_ref()
            .filter(|p| !p.done.load(Ordering::Relaxed))
            .map(|p| p.info());
        let rec_elapsed  = midi_recorder::elapsed(&record_handle);
        let recording    = midi_recorder::is_recording(&record_handle);
        let snap = build_snapshot(
            &menu, cursor, current, playing_idx(&playback),
            &settings, sustain, &stats,
            recording,
            rec_elapsed.map(|d| d.as_micros() as u64),
            pb_info.as_ref(),
            loading_info,
        );
        if let Ok(json) = serde_json::to_string(&snap) {
            if let Ok(mut s) = web_snapshot.lock() { *s = json; }
        }

        // ── Rescan ────────────────────────────────────────────────────────────
        if reload.swap(false, Ordering::SeqCst) {
            (menu, current, cursor, current_path) = rebuild_menu(
                &samples_dir, midi_dir.as_deref(), &menu, current, cursor,
            );
            continue;
        }

        // ── Playback ended ────────────────────────────────────────────────────
        if let Some(ref p) = playback {
            if p.done.load(Ordering::Relaxed) { playback = None; }
        }

        // ── Actions ───────────────────────────────────────────────────────────
        while let Ok(action) = action_rx.try_recv() {
            match action {
                MenuAction::CursorUp   => { if cursor > 0 { cursor -= 1; } }
                MenuAction::CursorDown => { if cursor + 1 < menu.len() { cursor += 1; } }
                MenuAction::CycleVariant(dir) => {
                    if let Some(MenuItem::Instrument(inst)) = menu.get_mut(cursor) {
                        inst.cycle_variant(dir);
                    }
                }
                MenuAction::CycleVariantAt { idx, dir } => {
                    if let Some(MenuItem::Instrument(inst)) = menu.get_mut(idx) {
                        inst.cycle_variant(dir);
                    }
                }
                MenuAction::ToggleRecord => {
                    if midi_recorder::is_recording(&record_handle) {
                        if let Some(buf) = midi_recorder::stop(&record_handle) {
                            let save_dir = midi_dir.as_deref().unwrap_or(std::path::Path::new("midi"));
                            let _ = std::fs::create_dir_all(save_dir);
                            match midi_recorder::save(buf, save_dir) {
                                Ok(p)  => eprintln!("KMS: recording saved: {}", p.display()),
                                Err(e) => eprintln!("KMS: failed to save recording: {}", e),
                            }
                            reload.store(true, Ordering::SeqCst);
                        }
                    } else {
                        midi_recorder::start(&record_handle);
                    }
                }
                MenuAction::PauseResume => { pause_resume(&playback); }
                MenuAction::SeekRelative(secs) => { seek_relative(&playback, secs); }
                MenuAction::Select => {
                    let idx = cursor;
                    if idx < menu.len() {
                        match &menu[idx] {
                            MenuItem::Instrument(inst)
                                if idx != current || inst.path() != current_path.as_path() =>
                            {
                                loading_job = Some((idx, LoadingJob::start(inst.clone())));
                            }
                            MenuItem::MidiFile { .. } => {
                                do_select(idx, &mut menu, &midi_tx, &mut playback);
                            }
                            _ => {}
                        }
                    }
                }
                MenuAction::SelectAt(target) => {
                    cursor = target.min(menu.len().saturating_sub(1));
                    let idx = cursor;
                    if idx < menu.len() {
                        match &menu[idx] {
                            MenuItem::Instrument(inst)
                                if idx != current || inst.path() != current_path.as_path() =>
                            {
                                loading_job = Some((idx, LoadingJob::start(inst.clone())));
                            }
                            MenuItem::MidiFile { .. } => {
                                do_select(idx, &mut menu, &midi_tx, &mut playback);
                            }
                            _ => {}
                        }
                    }
                }
                MenuAction::ShowQr | MenuAction::Rescan => {
                    reload.store(true, Ordering::SeqCst);
                }
                _ => { apply_settings_action(&action, &mut settings, auto_tune, &state); }
            }
        }
    }
}

fn do_select(
    idx:      usize,
    menu:     &mut Vec<MenuItem>,
    midi_tx:  &Sender<MidiEvent>,
    playback: &mut Option<PlaybackState>,
) {
    match &menu[idx] {
        MenuItem::MidiFile { path, .. } => {
            let path          = path.clone();
            let already_playing = playback.as_ref()
                .map_or(false, |p| p.menu_idx == idx && !p.done.load(Ordering::Relaxed));
            stop_playback(playback);
            if !already_playing {
                let stop_flag   = Arc::new(AtomicBool::new(false));
                let done_flag   = Arc::new(AtomicBool::new(false));
                let paused_flag = Arc::new(AtomicBool::new(false));
                let cur_us      = Arc::new(AtomicU64::new(0));
                let seek_flag   = Arc::new(AtomicU64::new(u64::MAX));
                let (handle, total_us) = midi_player::spawn(
                    path, midi_tx.clone(),
                    Arc::clone(&stop_flag), Arc::clone(&done_flag),
                    Arc::clone(&paused_flag), Arc::clone(&cur_us),
                    Arc::clone(&seek_flag),
                );
                *playback = Some(PlaybackState {
                    stop: stop_flag, done: done_flag,
                    paused: paused_flag,
                    current_us: cur_us, total_us,
                    seek_to: seek_flag,
                    _handle: handle, menu_idx: idx,
                });
            }
        }
        _ => {}
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub fn run_drm_ui(
    menu:                Vec<MenuItem>,
    state:               Arc<Mutex<SamplerState>>,
    midi_tx:             Sender<MidiEvent>,
    record_handle:       RecordHandle,
    midi_dir:            Option<PathBuf>,
    samples_dir:         PathBuf,
    web_snapshot:        Arc<Mutex<String>>,
    action_tx:           Sender<MenuAction>,
    action_rx:           Receiver<MenuAction>,
    auto_tune:           f32,
    quit:                Arc<AtomicBool>,
    reload:              Arc<AtomicBool>,
    initial_loading_job: Option<(usize, LoadingJob)>,
) {
    let Some((card, display)) = find_card_and_display() else {
        eprintln!("KMS: no connected DRM display found; falling back to terminal.");
        return;
    };
    let (w, h) = (display.width, display.height);
    eprintln!("KMS: {}×{} via /dev/dri/card* (DRM dumb-buffer, software rasterizer)", w, h);

    // Create dumb buffer (CPU-accessible pixel memory).
    let mut dumb = match card.create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32) {
        Ok(d)  => d,
        Err(e) => { eprintln!("KMS: dumb buffer: {}", e); return; }
    };
    let fb = match card.add_framebuffer(&dumb, 24, 32) {
        Ok(f)  => f,
        Err(e) => { eprintln!("KMS: add_framebuffer: {}", e); let _ = card.destroy_dumb_buffer(dumb); return; }
    };

    // Switch the display to our framebuffer.
    if let Err(e) = card.set_crtc(display.crtc, Some(fb), (0, 0), &[display.connector], Some(display.mode)) {
        eprintln!("KMS: set_crtc: {}", e);
        let _ = card.destroy_framebuffer(fb);
        let _ = card.destroy_dumb_buffer(dumb);
        return;
    }

    // CPU pixel buffer — written by the rasterizer, then copied to the dumb buffer.
    let mut pixels = vec![0u32; (w * h) as usize];

    // Spawn action dispatch background thread (takes ownership of menu).
    {
        let state2  = Arc::clone(&state);
        let midi_tx2 = midi_tx.clone();
        let rh2     = Arc::clone(&record_handle);
        let ws2     = Arc::clone(&web_snapshot);
        let ar2     = action_rx;
        let quit2   = Arc::clone(&quit);
        let reload2 = Arc::clone(&reload);
        let md      = midi_dir.clone();
        let sd      = samples_dir.clone();
        std::thread::spawn(move || dispatch_loop(
            menu, state2, midi_tx2, rh2, md, sd,
            ws2, ar2, auto_tune, quit2, reload2, initial_loading_job,
        ));
    }

    // egui setup.
    let ctx     = egui::Context::default();
    let backend = LocalBackend::new(web_snapshot, action_tx);
    let mut app = PianeerApp::new(Box::new(backend), lan_server_url());
    let mut input    = EvdevInput::new();
    let mut cursor   = egui::pos2(w as f32 / 2.0, h as f32 / 2.0);
    let mut textures: HashMap<egui::TextureId, EguiTexture> = HashMap::new();

    let target = Duration::from_millis(50); // ~20 fps

    loop {
        let t0 = Instant::now();
        if quit.load(Ordering::SeqCst) { break; }

        // Collect input and run egui.
        let raw       = input.collect(w as f32, h as f32, &mut cursor);
        let full_out  = ctx.run(raw, |ctx| app.update_egui(ctx));

        apply_texture_delta(&mut textures, &full_out.textures_delta);

        // Rasterize into the CPU pixel buffer.
        pixels.fill(0x00_10_10_10); // dark background clear
        let primitives = ctx.tessellate(full_out.shapes, 1.0);
        for cp in &primitives {
            if let egui::epaint::Primitive::Mesh(mesh) = &cp.primitive {
                render_mesh(&mut pixels, w as usize, h as usize, cp.clip_rect, mesh, &textures);
            }
        }

        // Copy to DRM dumb buffer with RGBA→Xrgb8888 channel swap (R↔B).
        {
            let Ok(mut map) = card.map_dumb_buffer(&mut dumb) else { break };
            let dst = map.as_mut();
            for (i, &pix) in pixels.iter().enumerate() {
                let r = ((pix >> 16) & 0xFF) as u8;
                let g = ((pix >>  8) & 0xFF) as u8;
                let b = ( pix        & 0xFF) as u8;
                // Xrgb8888 on LE: byte[0]=B, [1]=G, [2]=R, [3]=0
                let base = i * 4;
                dst[base]     = b;
                dst[base + 1] = g;
                dst[base + 2] = r;
                dst[base + 3] = 0;
            }
        }

        // Present the frame.
        let _ = card.set_crtc(display.crtc, Some(fb), (0, 0), &[display.connector], Some(display.mode));

        // Open URLs requested by the UI (e.g. QR link).
        if let Some(url) = full_out.platform_output.open_url {
            let _ = std::process::Command::new("xdg-open").arg(url.url).spawn();
        }

        let elapsed = t0.elapsed();
        if elapsed < target { std::thread::sleep(target - elapsed); }
    }

    // Restore the original CRTC state if we saved it.
    if let Some(saved) = display.saved_crtc {
        let _ = card.set_crtc(
            display.crtc,
            saved.framebuffer(),
            saved.position(),
            &[display.connector],
            saved.mode(),
        );
    }
    let _ = card.destroy_framebuffer(fb);
    let _ = card.destroy_dumb_buffer(dumb);
}
