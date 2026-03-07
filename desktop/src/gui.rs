// Native GUI path (feature = "native-ui").
// Extracted from main.rs to keep the startup orchestrator slim.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use pianeer_core::dispatch::{apply_settings_action, rebuild_menu};
use pianeer_core::loader::{LoadingJob, poll_loading_job, save_last_instrument_path};
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::{midi_player, midi_recorder};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::StatsCollector;
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackState, ProcStats, Settings,
    stop_playback, playing_idx, pause_resume, seek_relative,
};

#[cfg(feature = "native-ui")]
pub fn run_native_ui(
    menu_init:            Vec<MenuItem>,
    state:                Arc<Mutex<SamplerState>>,
    midi_tx:              Sender<MidiEvent>,
    record_handle:        pianeer_core::midi_recorder::RecordHandle,
    midi_dir:             Option<std::path::PathBuf>,
    samples_dir:          std::path::PathBuf,
    web_snapshot:         Arc<Mutex<String>>,
    action_tx:            Sender<MenuAction>,
    action_rx:            Receiver<MenuAction>,
    auto_tune_init:       f32,
    quit:                 Arc<AtomicBool>,
    reload:               Arc<AtomicBool>,
    initial_loading_job:  Option<(usize, LoadingJob)>,
) {
    use pianeer_egui::{LocalBackend, PianeerApp};

    let snap_bg = Arc::clone(&web_snapshot);
    let quit_bg = Arc::clone(&quit);

    // Background thread: action dispatch + snapshot updates (no terminal I/O).
    thread::spawn(move || {
        let state_file = samples_dir.join(".last_instrument");
        let mut menu = menu_init;
        let mut auto_tune = auto_tune_init;
        let mut cursor = initial_loading_job.as_ref().map(|(idx, _)| *idx).unwrap_or(0);
        let mut current = 0usize;
        // If an async load was started before the window opened, current_path stays
        // empty until poll_loading_job sets it on completion.
        let mut current_path = if initial_loading_job.is_some() {
            std::path::PathBuf::new()
        } else {
            menu.iter()
                .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
                .unwrap_or_default()
        };
        let mut playback: Option<PlaybackState> = None;
        let mut settings = Settings::default();
        let mut stats_collector = StatsCollector::new();
        let mut stats = ProcStats::default();
        let mut sustain = false;
        let mut loading_job = initial_loading_job;

        loop {
            thread::sleep(Duration::from_millis(100));

            if quit_bg.load(Ordering::Relaxed) {
                stop_playback(&mut playback);
                break;
            }

            // ── Stats ─────────────────────────────────────────────────────────
            (stats, sustain) = stats_collector.tick(&state);

            // ── Poll in-progress instrument load ──────────────────────────────
            if let Some((new_idx, new_path, new_at)) =
                poll_loading_job(&mut loading_job, &state, &settings, &menu)
            {
                current = new_idx;
                save_last_instrument_path(&state_file, &new_path);
                current_path = new_path;
                auto_tune = new_at;
            }

            // ── Snapshot ──────────────────────────────────────────────────────
            let loading_info = loading_job.as_ref().map(|(idx, job)| (*idx, job.pct()));
            let pb_info = playback.as_ref()
                .filter(|p| !p.done.load(Ordering::Relaxed))
                .map(|p| p.info());
            let rec_elapsed = midi_recorder::elapsed(&record_handle);
            let recording_active = midi_recorder::is_recording(&record_handle);
            let snap = build_snapshot(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain, &stats,
                recording_active,
                rec_elapsed.map(|d| d.as_micros() as u64),
                pb_info.as_ref(),
                loading_info,
            );
            if let Ok(json) = serde_json::to_string(&snap) {
                if let Ok(mut s) = snap_bg.lock() { *s = json; }
            }

            // ── Rescan ────────────────────────────────────────────────────────
            if reload.swap(false, Ordering::SeqCst) {
                (menu, current, cursor, current_path) = rebuild_menu(
                    &samples_dir, midi_dir.as_deref(), &menu, current, cursor,
                );
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
                    MenuAction::PauseResume => { pause_resume(&playback); }
                    MenuAction::SeekRelative(secs) => { seek_relative(&playback, secs); }
                    MenuAction::Select => {
                        gui_do_select(cursor, &menu, &mut loading_job, &mut current,
                                      &current_path, &midi_tx, &mut playback);
                    }
                    MenuAction::SelectAt(target) => {
                        cursor = target.min(menu.len().saturating_sub(1));
                        gui_do_select(cursor, &menu, &mut loading_job, &mut current,
                                      &current_path, &midi_tx, &mut playback);
                    }
                    // In GUI mode the QR modal is shown via the egui button; treat ShowQr as rescan.
                    MenuAction::ShowQr | MenuAction::Rescan => {
                        reload.store(true, Ordering::SeqCst);
                    }
                    _ => { apply_settings_action(&action, &mut settings, auto_tune, &state); }
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

#[cfg(feature = "native-ui")]
fn gui_do_select(
    idx:         usize,
    menu:        &[MenuItem],
    loading_job: &mut Option<(usize, LoadingJob)>,
    current:     &usize,
    current_path: &std::path::Path,
    midi_tx:     &Sender<MidiEvent>,
    playback:    &mut Option<PlaybackState>,
) {
    if idx >= menu.len() { return; }
    match &menu[idx] {
        MenuItem::Instrument(inst) if idx != *current || inst.path() != current_path => {
            *loading_job = Some((idx, LoadingJob::start(inst.clone())));
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
