// Native GUI path (feature = "native-ui").
// Extracted from main.rs to keep the startup orchestrator slim.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use pianeer_core::loader::load_instrument_data;
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::{instruments, midi_player, midi_recorder};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::{read_cpu_ticks, read_mem_mb};
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackState, ProcStats, Settings,
    stop_playback, playing_idx,
};

#[cfg(feature = "native-ui")]
pub fn run_native_ui(
    menu_init:      Vec<MenuItem>,
    state:          Arc<Mutex<SamplerState>>,
    midi_tx:        Sender<MidiEvent>,
    record_handle:  pianeer_core::midi_recorder::RecordHandle,
    midi_dir:       Option<std::path::PathBuf>,
    samples_dir:    std::path::PathBuf,
    web_snapshot:   Arc<Mutex<String>>,
    action_tx:      Sender<MenuAction>,
    action_rx:      Receiver<MenuAction>,
    auto_tune_init: f32,
    quit:           Arc<AtomicBool>,
    reload:         Arc<AtomicBool>,
) {
    use pianeer_egui::{LocalBackend, PianeerApp};

    let snap_bg = Arc::clone(&web_snapshot);
    let quit_bg = Arc::clone(&quit);

    // Background thread: action dispatch + snapshot updates (no terminal I/O).
    thread::spawn(move || {
        let mut menu = menu_init;
        let mut auto_tune = auto_tune_init;
        let mut cursor = 0usize;
        let mut current = 0usize;
        let mut current_path = menu.iter()
            .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
            .unwrap_or_default();
        let mut playback: Option<PlaybackState> = None;
        let mut settings = Settings::default();
        let mut sustain_prev = false;
        let mut stats = ProcStats::default();
        let mut prev_cpu_ticks: u64 = read_cpu_ticks().unwrap_or(0);
        let mut prev_cpu_instant = std::time::Instant::now();
        let mut clip_until: Option<std::time::Instant> = None;

        loop {
            thread::sleep(Duration::from_millis(100));

            if quit_bg.load(Ordering::Relaxed) {
                stop_playback(&mut playback);
                break;
            }

            // ── Stats ─────────────────────────────────────────────────────────
            let (sustain_now, voices_now, peak_l, peak_r, clip_now) = state.try_lock()
                .map(|mut s| {
                    let clip = s.clip_l || s.clip_r;
                    s.clip_l = false; s.clip_r = false;
                    (s.sustain_pedal, s.active_voice_count(), s.peak_l, s.peak_r, clip)
                })
                .unwrap_or((sustain_prev, stats.voices, stats.peak_l, stats.peak_r, false));

            let now = std::time::Instant::now();
            if clip_now { clip_until = Some(now + Duration::from_secs(2)); }
            let clip_holding = clip_until.map(|t| now < t).unwrap_or(false);

            let elapsed = now.duration_since(prev_cpu_instant).as_secs_f32().max(0.001);
            let ticks_now = read_cpu_ticks().unwrap_or(prev_cpu_ticks);
            let cpu_pct = (((ticks_now.saturating_sub(prev_cpu_ticks)) as f32 / 100.0) / elapsed * 100.0)
                .round().clamp(0.0, 100.0) as u8;
            prev_cpu_ticks = ticks_now;
            prev_cpu_instant = now;

            stats = ProcStats { cpu_pct, mem_mb: read_mem_mb(), voices: voices_now,
                                peak_l, peak_r, clip: clip_holding };
            sustain_prev = sustain_now;

            // ── Snapshot ──────────────────────────────────────────────────────
            let pb_info = playback.as_ref()
                .filter(|p| !p.done.load(Ordering::Relaxed))
                .map(|p| p.info());
            let rec_elapsed = midi_recorder::elapsed(&record_handle);
            let recording_active = midi_recorder::is_recording(&record_handle);
            let snap = build_snapshot(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain_prev, &stats,
                recording_active,
                rec_elapsed.map(|d| d.as_micros() as u64),
                pb_info.as_ref(),
            );
            if let Ok(json) = serde_json::to_string(&snap) {
                if let Ok(mut s) = snap_bg.lock() { *s = json; }
            }

            // ── Rescan ────────────────────────────────────────────────────────
            if reload.swap(false, Ordering::SeqCst) {
                let saved_path = match menu.get(current) {
                    Some(MenuItem::Instrument(i)) => Some(i.path().clone()),
                    _ => None,
                };
                let new_instr = instruments::discover(&samples_dir);
                let new_midi  = midi_dir.as_ref()
                    .map(|d| midi_player::discover(d))
                    .unwrap_or_default();
                let mut new_menu: Vec<MenuItem> =
                    new_instr.into_iter().map(MenuItem::Instrument).collect();
                for (name, path) in new_midi {
                    new_menu.push(MenuItem::MidiFile { name, path });
                }
                current = saved_path
                    .and_then(|p| new_menu.iter().position(|m| {
                        matches!(m, MenuItem::Instrument(i) if i.path() == &p)
                    }))
                    .unwrap_or(0);
                cursor = cursor.min(new_menu.len().saturating_sub(1));
                menu = new_menu;
                current_path = menu.get(current)
                    .and_then(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
                    .unwrap_or_default();
                continue;
            }

            // ── Playback ended ────────────────────────────────────────────────
            if let Some(ref p) = playback {
                if p.done.load(Ordering::Relaxed) { playback = None; }
            }

            // ── Actions ───────────────────────────────────────────────────────
            if let Ok(action) = action_rx.try_recv() {
                match action {
                    MenuAction::CursorUp => { if cursor > 0 { cursor -= 1; } }
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
                    MenuAction::CycleVeltrack => {
                        settings.veltrack = settings.veltrack.cycle();
                        if let Ok(ref mut s) = state.lock() {
                            s.veltrack_override = settings.veltrack.override_pct();
                        }
                    }
                    MenuAction::CycleTune => {
                        settings.tune = settings.tune.cycle();
                        if let Ok(ref mut s) = state.lock() {
                            s.master_tune_semitones = auto_tune + settings.tune.semitones();
                        }
                    }
                    MenuAction::ToggleRelease => {
                        settings.release_enabled = !settings.release_enabled;
                        if let Ok(ref mut s) = state.lock() {
                            s.release_enabled = settings.release_enabled;
                        }
                    }
                    MenuAction::VolumeChange(delta) => {
                        settings.volume_db = (settings.volume_db + delta as f32 * 3.0).clamp(-12.0, 12.0);
                        if let Ok(ref mut s) = state.lock() {
                            s.master_volume = 10.0_f32.powf(settings.volume_db / 20.0);
                        }
                    }
                    MenuAction::TransposeChange(delta) => {
                        settings.transpose = (settings.transpose + delta as i32).clamp(-12, 12);
                        if let Ok(ref mut s) = state.lock() {
                            s.transpose = settings.transpose;
                        }
                    }
                    MenuAction::ToggleResonance => {
                        settings.resonance_enabled = !settings.resonance_enabled;
                        if let Ok(ref mut s) = state.lock() {
                            s.resonance_enabled = settings.resonance_enabled;
                        }
                    }
                    MenuAction::ToggleRecord => {
                        if midi_recorder::is_recording(&record_handle) {
                            if let Some(buf) = midi_recorder::stop(&record_handle) {
                                let save_dir = midi_dir.as_deref()
                                    .unwrap_or_else(|| std::path::Path::new("midi"));
                                let _ = std::fs::create_dir_all(save_dir);
                                match midi_recorder::save(buf, save_dir) {
                                    Ok(p)  => eprintln!("Recording saved: {}", p.display()),
                                    Err(e) => eprintln!("Failed to save recording: {}", e),
                                }
                                reload.store(true, Ordering::SeqCst);
                            }
                        } else {
                            midi_recorder::start(&record_handle);
                        }
                    }
                    MenuAction::PauseResume => {
                        if let Some(ref pb) = playback {
                            if !pb.done.load(Ordering::Relaxed) {
                                let was = pb.paused.load(Ordering::Relaxed);
                                pb.paused.store(!was, Ordering::Relaxed);
                            }
                        }
                    }
                    MenuAction::SeekRelative(secs) => {
                        if let Some(ref pb) = playback {
                            if !pb.done.load(Ordering::Relaxed) {
                                let cur = pb.current_us.load(Ordering::Relaxed);
                                let new_us = (cur as i64 + secs * 1_000_000i64)
                                    .clamp(0, pb.total_us as i64) as u64;
                                pb.seek_to.store(new_us, Ordering::Relaxed);
                            }
                        }
                    }
                    MenuAction::Select => {
                        gui_select(cursor, &mut menu, &mut current, &mut current_path,
                                   &mut auto_tune, &settings, &state, &midi_tx, &mut playback);
                    }
                    MenuAction::SelectAt(target) => {
                        cursor = target.min(menu.len().saturating_sub(1));
                        gui_select(cursor, &mut menu, &mut current, &mut current_path,
                                   &mut auto_tune, &settings, &state, &midi_tx, &mut playback);
                    }
                    // In GUI mode the QR modal is shown via the egui button; treat ShowQr as rescan.
                    MenuAction::ShowQr => { reload.store(true, Ordering::SeqCst); }
                    MenuAction::Rescan => { reload.store(true, Ordering::SeqCst); }
                }
            }
        }
    });

    // Run eframe on the main thread (blocks until window closed).
    let backend = LocalBackend::new(web_snapshot, action_tx);
    let server_url = lan_server_url();
    let app = PianeerApp::new(Box::new(backend), server_url);
    eframe::run_native(
        "Pianeer",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    ).unwrap_or_else(|e| eprintln!("eframe error: {e}"));

    // Window closed — wake background thread so it exits cleanly.
    quit.store(true, Ordering::SeqCst);
}

/// Return the LAN-accessible HTTP URL for the web server, e.g. "http://192.168.1.5:4000".
/// Falls back to localhost if the LAN IP cannot be determined.
#[cfg(feature = "native-ui")]
pub fn lan_server_url() -> String {
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("1.1.1.1:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return format!("http://{}:4000", addr.ip());
            }
        }
    }
    "http://localhost:4000".to_string()
}

/// Instrument-load / MIDI-start helper for the GUI background thread.
#[cfg(feature = "native-ui")]
fn gui_select(
    idx:          usize,
    menu:         &mut Vec<MenuItem>,
    current:      &mut usize,
    current_path: &mut std::path::PathBuf,
    auto_tune:    &mut f32,
    settings:     &Settings,
    state:        &Arc<Mutex<SamplerState>>,
    midi_tx:      &Sender<MidiEvent>,
    playback:     &mut Option<PlaybackState>,
) {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    match &menu[idx] {
        MenuItem::Instrument(inst)
            if idx != *current || inst.path() != current_path.as_path() =>
        {
            let new_path = inst.path().clone();
            println!("Loading {}...", inst.name);
            match load_instrument_data(inst) {
                Ok(loaded) => {
                    if let Ok(ref mut s) = state.lock() {
                        s.swap_instrument(
                            loaded.regions, loaded.samples,
                            loaded.cc_defaults,
                            loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
                        );
                        *auto_tune = s.detect_a4_tune_correction();
                        s.master_tune_semitones = *auto_tune + settings.tune.semitones();
                    }
                    *current = idx;
                    *current_path = new_path;
                }
                Err(e) => eprintln!("Error loading instrument: {}", e),
            }
        }
        MenuItem::MidiFile { path, .. } => {
            let path = path.clone();
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
