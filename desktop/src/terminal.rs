// Raw-mode terminal loop.
// Extracted from main.rs to keep the startup orchestrator slim.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;

fn raw_on() {
    terminal::enable_raw_mode().expect("Failed to enable raw mode");
}

fn raw_off() {
    terminal::disable_raw_mode().ok();
}

use pianeer_core::loader::load_instrument_data;
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::{instruments, midi_player, midi_recorder};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::{read_cpu_ticks, read_mem_mb};
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackInfo, PlaybackState, ProcStats, Settings,
    stop_playback, playing_idx,
};
use crate::ui::{print_menu, print_qr_modal};

pub fn run_terminal(
    mut menu:       Vec<MenuItem>,
    state:          Arc<Mutex<SamplerState>>,
    midi_tx:        Sender<MidiEvent>,
    record_handle:  pianeer_core::midi_recorder::RecordHandle,
    midi_dir:       Option<std::path::PathBuf>,
    samples_dir:    std::path::PathBuf,
    web_snapshot:   Arc<Mutex<String>>,
    action_tx:      Sender<MenuAction>,
    action_rx:      Receiver<MenuAction>,
    mut auto_tune:  f32,
    quit:           Arc<AtomicBool>,
    reload:         Arc<AtomicBool>,
    show_qr:        Arc<AtomicBool>,
) {
    raw_on();

    let mut settings = Settings::default();
    let mut sustain_prev = false;
    let mut stats = ProcStats::default();
    let mut prev_cpu_ticks: u64 = read_cpu_ticks().unwrap_or(0);
    let mut prev_cpu_instant = std::time::Instant::now();
    let mut clip_until: Option<std::time::Instant> = None;

    let mk_pb_info = |pb: &Option<PlaybackState>| -> Option<PlaybackInfo> {
        pb.as_ref()
            .filter(|p| !p.done.load(Ordering::Relaxed))
            .map(|p| p.info())
    };

    print_menu(
        &menu, 0, 0, None, &settings, sustain_prev, &stats,
        false, None, None,
    );

    // Spawn input thread.
    {
        let quit_input    = Arc::clone(&quit);
        let reload_input  = Arc::clone(&reload);
        let show_qr_input = Arc::clone(&show_qr);
        thread::spawn(move || {
            loop {
                if quit_input.load(Ordering::Relaxed) { break; }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        if let Ok(Event::Key(key)) = event::read() {
                            let is_quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                                || (matches!(key.code, KeyCode::Char('c'))
                                    && key.modifiers.contains(KeyModifiers::CONTROL));
                            if is_quit {
                                quit_input.store(true, Ordering::SeqCst);
                                break;
                            }
                            if show_qr_input.load(Ordering::Relaxed) {
                                let _ = action_tx.try_send(MenuAction::ShowQr);
                                continue;
                            }
                            match key.code {
                                KeyCode::Char('r') | KeyCode::Char('R') => {
                                    reload_input.store(true, Ordering::SeqCst);
                                }
                                KeyCode::Up    => { let _ = action_tx.try_send(MenuAction::CursorUp); }
                                KeyCode::Down  => { let _ = action_tx.try_send(MenuAction::CursorDown); }
                                KeyCode::Left  => { let _ = action_tx.try_send(MenuAction::CycleVariant(-1)); }
                                KeyCode::Right => { let _ = action_tx.try_send(MenuAction::CycleVariant(1)); }
                                KeyCode::Enter => { let _ = action_tx.try_send(MenuAction::Select); }
                                KeyCode::Char('v') | KeyCode::Char('V') => {
                                    let _ = action_tx.try_send(MenuAction::CycleVeltrack);
                                }
                                KeyCode::Char('t') | KeyCode::Char('T') => {
                                    let _ = action_tx.try_send(MenuAction::CycleTune);
                                }
                                KeyCode::Char('e') | KeyCode::Char('E') => {
                                    let _ = action_tx.try_send(MenuAction::ToggleRelease);
                                }
                                KeyCode::Char('+') | KeyCode::Char('=') => {
                                    let _ = action_tx.try_send(MenuAction::VolumeChange(1));
                                }
                                KeyCode::Char('-') => {
                                    let _ = action_tx.try_send(MenuAction::VolumeChange(-1));
                                }
                                KeyCode::Char('[') => {
                                    let _ = action_tx.try_send(MenuAction::TransposeChange(-1));
                                }
                                KeyCode::Char(']') => {
                                    let _ = action_tx.try_send(MenuAction::TransposeChange(1));
                                }
                                KeyCode::Char('h') | KeyCode::Char('H') => {
                                    let _ = action_tx.try_send(MenuAction::ToggleResonance);
                                }
                                KeyCode::Char('w') | KeyCode::Char('W') => {
                                    let _ = action_tx.try_send(MenuAction::ToggleRecord);
                                }
                                KeyCode::Char(' ') => {
                                    let _ = action_tx.try_send(MenuAction::PauseResume);
                                }
                                KeyCode::Char(',') => {
                                    let _ = action_tx.try_send(MenuAction::SeekRelative(-10));
                                }
                                KeyCode::Char('.') => {
                                    let _ = action_tx.try_send(MenuAction::SeekRelative(10));
                                }
                                KeyCode::Char('p') | KeyCode::Char('P') => {
                                    let _ = action_tx.try_send(MenuAction::ShowQr);
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
    }

    // Main loop: handle quit, reload, instrument switches, and MIDI file playback.
    let mut current = 0usize;
    let mut current_path = menu.iter()
        .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
        .unwrap_or_default();
    let mut cursor = 0usize;
    let mut playback: Option<PlaybackState> = None;

    loop {
        thread::sleep(Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            stop_playback(&mut playback);
            raw_off();
            println!("\r\nExiting...");
            break;
        }

        let (sustain_now, voices_now, peak_l, peak_r, clip_now) = state.try_lock()
            .map(|mut s| {
                let clip = s.clip_l || s.clip_r;
                s.clip_l = false;
                s.clip_r = false;
                (s.sustain_pedal, s.active_voice_count(), s.peak_l, s.peak_r, clip)
            })
            .unwrap_or((sustain_prev, stats.voices, stats.peak_l, stats.peak_r, false));

        let now = std::time::Instant::now();
        if clip_now {
            clip_until = Some(now + Duration::from_secs(2));
        }
        let clip_holding = clip_until.map(|t| now < t).unwrap_or(false);

        let elapsed = now.duration_since(prev_cpu_instant).as_secs_f32().max(0.001);
        let ticks_now = read_cpu_ticks().unwrap_or(prev_cpu_ticks);
        let cpu_pct = (((ticks_now.saturating_sub(prev_cpu_ticks)) as f32 / 100.0) / elapsed * 100.0)
            .round().clamp(0.0, 100.0) as u8;
        prev_cpu_ticks = ticks_now;
        prev_cpu_instant = now;

        let new_stats = ProcStats {
            cpu_pct,
            mem_mb: read_mem_mb(),
            voices: voices_now,
            peak_l,
            peak_r,
            clip: clip_holding,
        };

        let active_playback = playback.as_ref().map(|p| !p.done.load(Ordering::Relaxed)).unwrap_or(false);
        let recording_active = midi_recorder::is_recording(&record_handle);
        let needs_redraw = sustain_now != sustain_prev
            || new_stats != stats
            || active_playback
            || recording_active;

        sustain_prev = sustain_now;
        stats = new_stats;

        let pb_info = mk_pb_info(&playback);
        let rec_elapsed = midi_recorder::elapsed(&record_handle);

        // Update web UI snapshot.
        {
            let snap = build_snapshot(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain_prev, &stats,
                recording_active,
                rec_elapsed.map(|d| d.as_micros() as u64),
                pb_info.as_ref(),
            );
            if let Ok(json) = serde_json::to_string(&snap) {
                if let Ok(mut s) = web_snapshot.lock() { *s = json; }
            }
        }

        if needs_redraw && !show_qr.load(Ordering::Relaxed) {
            print_menu(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain_prev, &stats,
                recording_active, rec_elapsed, pb_info.as_ref(),
            );
        }

        // Rescan on R.
        if reload.swap(false, Ordering::SeqCst) {
            let saved_path = match menu.get(current) {
                Some(MenuItem::Instrument(i)) => Some(i.path().clone()),
                _ => None,
            };
            let new_instruments = instruments::discover(&samples_dir);
            let new_midi = match midi_dir.as_ref() {
                Some(dir) => midi_player::discover(dir),
                None => Vec::new(),
            };
            let mut new_menu: Vec<MenuItem> =
                new_instruments.into_iter().map(MenuItem::Instrument).collect();
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
            show_qr.store(false, Ordering::Relaxed);
            let pb_info = mk_pb_info(&playback);
            let rec_elapsed = midi_recorder::elapsed(&record_handle);
            print_menu(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain_prev, &stats,
                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref(),
            );
            continue;
        }

        // Redraw if MIDI playback ended naturally.
        if let Some(ref p) = playback {
            if p.done.load(Ordering::Relaxed) {
                playback = None;
                if !show_qr.load(Ordering::Relaxed) {
                    print_menu(
                        &menu, cursor, current, None,
                        &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle),
                        midi_recorder::elapsed(&record_handle), None,
                    );
                }
            }
        }

        if let Ok(action) = action_rx.try_recv() {
            match action {
                MenuAction::CursorUp => {
                    if cursor > 0 { cursor -= 1; }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::CursorDown => {
                    if cursor + 1 < menu.len() { cursor += 1; }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::CycleVariant(dir) => {
                    if let Some(MenuItem::Instrument(inst)) = menu.get_mut(cursor) {
                        inst.cycle_variant(dir);
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::CycleVariantAt { idx, dir } => {
                    if let Some(MenuItem::Instrument(inst)) = menu.get_mut(idx) {
                        inst.cycle_variant(dir);
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::CycleVeltrack => {
                    settings.veltrack = settings.veltrack.cycle();
                    if let Ok(ref mut s) = state.lock() {
                        s.veltrack_override = settings.veltrack.override_pct();
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::CycleTune => {
                    settings.tune = settings.tune.cycle();
                    if let Ok(ref mut s) = state.lock() {
                        s.master_tune_semitones = auto_tune + settings.tune.semitones();
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::ToggleRelease => {
                    settings.release_enabled = !settings.release_enabled;
                    if let Ok(ref mut s) = state.lock() {
                        s.release_enabled = settings.release_enabled;
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::VolumeChange(delta) => {
                    settings.volume_db = (settings.volume_db + delta as f32 * 3.0).clamp(-12.0, 12.0);
                    if let Ok(ref mut s) = state.lock() {
                        s.master_volume = 10.0_f32.powf(settings.volume_db / 20.0);
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::TransposeChange(delta) => {
                    settings.transpose = (settings.transpose + delta as i32).clamp(-12, 12);
                    if let Ok(ref mut s) = state.lock() {
                        s.transpose = settings.transpose;
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::ToggleResonance => {
                    settings.resonance_enabled = !settings.resonance_enabled;
                    if let Ok(ref mut s) = state.lock() {
                        s.resonance_enabled = settings.resonance_enabled;
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::ToggleRecord => {
                    if midi_recorder::is_recording(&record_handle) {
                        if let Some(buf) = midi_recorder::stop(&record_handle) {
                            let save_dir = midi_dir.as_deref().unwrap_or_else(|| {
                                std::path::Path::new("midi")
                            });
                            let _ = std::fs::create_dir_all(save_dir);
                            match midi_recorder::save(buf, save_dir) {
                                Ok(path) => {
                                    raw_off();
                                    println!("\r\nRecording saved: {}", path.display());
                                    raw_on();
                                }
                                Err(e) => {
                                    raw_off();
                                    eprintln!("\r\nFailed to save recording: {}", e);
                                    raw_on();
                                }
                            }
                            reload.store(true, Ordering::SeqCst);
                        }
                    } else {
                        midi_recorder::start(&record_handle);
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::PauseResume => {
                    if let Some(ref pb) = playback {
                        if !pb.done.load(Ordering::Relaxed) {
                            let was_paused = pb.paused.load(Ordering::Relaxed);
                            pb.paused.store(!was_paused, Ordering::Relaxed);
                        }
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::SeekRelative(secs) => {
                    if let Some(ref pb) = playback {
                        if !pb.done.load(Ordering::Relaxed) {
                            let cur = pb.current_us.load(Ordering::Relaxed);
                            let offset = secs * 1_000_000i64;
                            let new_pos = (cur as i64 + offset)
                                .clamp(0, pb.total_us as i64) as u64;
                            pb.seek_to.store(new_pos, Ordering::Relaxed);
                        }
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::SelectAt(target) => {
                    cursor = target.min(menu.len().saturating_sub(1));
                    let idx = cursor;
                    match &menu[idx] {
                        MenuItem::Instrument(inst)
                            if idx != current || inst.path() != &current_path =>
                        {
                            while action_rx.try_recv().is_ok() {}
                            raw_off();
                            println!("\r\nLoading {}...", inst.name);
                            let new_path = inst.path().clone();
                            match load_instrument_data(inst) {
                                Ok(loaded) => {
                                    if let Ok(ref mut s) = state.lock() {
                                        s.swap_instrument(
                                            loaded.regions, loaded.samples,
                                            loaded.cc_defaults,
                                            loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
                                        );
                                        auto_tune = s.detect_a4_tune_correction();
                                        s.master_tune_semitones = auto_tune + settings.tune.semitones();
                                    }
                                    current = idx;
                                    current_path = new_path;
                                }
                                Err(e) => eprintln!("Error loading instrument: {}", e),
                            }
                            raw_on();
                            let pb_info = mk_pb_info(&playback);
                            let rec_elapsed = midi_recorder::elapsed(&record_handle);
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                        }
                        MenuItem::MidiFile { path, .. } => {
                            let path = path.clone();
                            let already_playing = playback.as_ref()
                                .map_or(false, |p| p.menu_idx == idx && !p.done.load(Ordering::Relaxed));
                            stop_playback(&mut playback);
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
                                playback = Some(PlaybackState {
                                    stop: stop_flag, done: done_flag,
                                    paused: paused_flag,
                                    current_us: cur_us, total_us,
                                    seek_to: seek_flag,
                                    _handle: handle, menu_idx: idx,
                                });
                            }
                            let pb_info = mk_pb_info(&playback);
                            let rec_elapsed = midi_recorder::elapsed(&record_handle);
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                        }
                        _ => {
                            let pb_info = mk_pb_info(&playback);
                            let rec_elapsed = midi_recorder::elapsed(&record_handle);
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                        }
                    }
                }
                MenuAction::ShowQr => {
                    let new_val = !show_qr.load(Ordering::Relaxed);
                    show_qr.store(new_val, Ordering::Relaxed);
                    if new_val {
                        print_qr_modal();
                    } else {
                        let pb_info = mk_pb_info(&playback);
                        let rec_elapsed = midi_recorder::elapsed(&record_handle);
                        print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                            midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                    }
                }
                MenuAction::Rescan => {
                    reload.store(true, Ordering::SeqCst);
                }
                MenuAction::Select => {
                    let idx = cursor;
                    match &menu[idx] {
                        MenuItem::Instrument(inst)
                            if idx != current || inst.path() != &current_path =>
                        {
                            while action_rx.try_recv().is_ok() {}
                            raw_off();
                            println!("\r\nLoading {}...", inst.name);
                            let new_path = inst.path().clone();
                            match load_instrument_data(inst) {
                                Ok(loaded) => {
                                    if let Ok(ref mut s) = state.lock() {
                                        s.swap_instrument(
                                            loaded.regions, loaded.samples,
                                            loaded.cc_defaults,
                                            loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
                                        );
                                        auto_tune = s.detect_a4_tune_correction();
                                        s.master_tune_semitones = auto_tune + settings.tune.semitones();
                                    }
                                    current = idx;
                                    current_path = new_path;
                                }
                                Err(e) => eprintln!("Error loading instrument: {}", e),
                            }
                            raw_on();
                            let pb_info = mk_pb_info(&playback);
                            let rec_elapsed = midi_recorder::elapsed(&record_handle);
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                        }
                        MenuItem::MidiFile { path, .. } => {
                            let path = path.clone();
                            let already_playing = playback.as_ref()
                                .map_or(false, |p| p.menu_idx == idx && !p.done.load(Ordering::Relaxed));
                            stop_playback(&mut playback);
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
                                playback = Some(PlaybackState {
                                    stop: stop_flag, done: done_flag,
                                    paused: paused_flag,
                                    current_us: cur_us, total_us,
                                    seek_to: seek_flag,
                                    _handle: handle, menu_idx: idx,
                                });
                            }
                            let pb_info = mk_pb_info(&playback);
                            let rec_elapsed = midi_recorder::elapsed(&record_handle);
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                                midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
