// Raw-mode terminal loop.
// Extracted from main.rs to keep the startup orchestrator slim.

use std::sync::atomic::{AtomicBool, Ordering};
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

use pianeer_core::dispatch::{apply_nav_action, apply_settings_action, rebuild_menu};
use pianeer_core::loader::{load_instrument_data, save_last_instrument_path};
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::midi_recorder;
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::StatsCollector;
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackInfo, PlaybackState, ProcStats, Settings,
    stop_playback, playing_idx, pause_resume, seek_relative, start_midi_playback,
};
use crate::ui::{print_menu, print_qr_modal};

pub fn run_terminal(
    mut menu:        Vec<MenuItem>,
    state:           Arc<Mutex<SamplerState>>,
    midi_tx:         Sender<MidiEvent>,
    record_handle:   pianeer_core::midi_recorder::RecordHandle,
    midi_dir:        Option<std::path::PathBuf>,
    samples_dir:     std::path::PathBuf,
    web_snapshot:    Arc<Mutex<String>>,
    action_tx:       Sender<MenuAction>,
    action_rx:       Receiver<MenuAction>,
    mut auto_tune:   f32,
    quit:            Arc<AtomicBool>,
    reload:          Arc<AtomicBool>,
    show_qr:         Arc<AtomicBool>,
    initial_current: usize,
) {
    raw_on();

    let mut settings = Settings::default();
    let mut sustain_prev = false;
    let mut stats = ProcStats::default();
    let mut stats_collector = StatsCollector::new();

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
    let state_file = samples_dir.join(".last_instrument");
    let mut current = initial_current;
    let mut current_path = menu.get(initial_current)
        .and_then(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
        .unwrap_or_default();
    let mut cursor = initial_current;
    let mut playback: Option<PlaybackState> = None;

    loop {
        thread::sleep(Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            stop_playback(&mut playback);
            raw_off();
            println!("\r\nExiting...");
            break;
        }

        let prev_stats = stats.clone();
        let prev_sustain = sustain_prev;
        let sustain_now;
        (stats, sustain_now) = stats_collector.tick(&state);
        sustain_prev = sustain_now;

        let active_playback = playback.as_ref().map(|p| !p.done.load(Ordering::Relaxed)).unwrap_or(false);
        let recording_active = midi_recorder::is_recording(&record_handle);
        let needs_redraw = sustain_now != prev_sustain
            || stats != prev_stats
            || active_playback
            || recording_active;

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
                None,
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
            (menu, current, cursor, current_path) = rebuild_menu(
                &samples_dir, midi_dir.as_deref(), &menu, current, cursor,
            );
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
            if apply_nav_action(&action, &mut cursor, &mut menu) {
                let pb_info = mk_pb_info(&playback);
                let rec_elapsed = midi_recorder::elapsed(&record_handle);
                print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                    midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
            } else { match action {
                MenuAction::ToggleRecord => {
                    let save_dir = midi_dir.as_deref().unwrap_or(std::path::Path::new("midi"));
                    match midi_recorder::stop_and_save(&record_handle, save_dir) {
                        None => { midi_recorder::start(&record_handle); }
                        Some(Ok(path)) => {
                            raw_off();
                            println!("\r\nRecording saved: {}", path.display());
                            raw_on();
                            reload.store(true, Ordering::SeqCst);
                        }
                        Some(Err(e)) => {
                            raw_off();
                            eprintln!("\r\nFailed to save recording: {}", e);
                            raw_on();
                        }
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::PauseResume => {
                    pause_resume(&playback);
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::SeekRelative(secs) => {
                    seek_relative(&playback, secs);
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                }
                MenuAction::SelectAt(target) => {
                    cursor = target.min(menu.len().saturating_sub(1));
                    let prev_path = current_path.clone();
                    term_do_select(cursor, &menu, &mut current, &mut current_path,
                                   &mut auto_tune, &settings, &state, &midi_tx,
                                   &mut playback, &action_rx);
                    if current_path != prev_path && !current_path.as_os_str().is_empty() {
                        save_last_instrument_path(&state_file, &current_path);
                    }
                    let pb_info = mk_pb_info(&playback);
                    let rec_elapsed = midi_recorder::elapsed(&record_handle);
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                        midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
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
                    let needs_redraw = match &menu[idx] {
                        MenuItem::Instrument(inst)
                            if idx != current || inst.path() != &current_path => true,
                        MenuItem::MidiFile { .. } => true,
                        _ => false,
                    };
                    let prev_path = current_path.clone();
                    term_do_select(idx, &menu, &mut current, &mut current_path,
                                   &mut auto_tune, &settings, &state, &midi_tx,
                                   &mut playback, &action_rx);
                    if current_path != prev_path && !current_path.as_os_str().is_empty() {
                        save_last_instrument_path(&state_file, &current_path);
                    }
                    if needs_redraw {
                        let pb_info = mk_pb_info(&playback);
                        let rec_elapsed = midi_recorder::elapsed(&record_handle);
                        print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                            midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                    }
                }
                action => {
                    if apply_settings_action(&action, &mut settings, auto_tune, &state) {
                        let pb_info = mk_pb_info(&playback);
                        let rec_elapsed = midi_recorder::elapsed(&record_handle);
                        print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats,
                            midi_recorder::is_recording(&record_handle), rec_elapsed, pb_info.as_ref());
                    }
                }
            } } // close match action + else block
        }
    }
}

fn term_do_select(
    idx:          usize,
    menu:         &[MenuItem],
    current:      &mut usize,
    current_path: &mut std::path::PathBuf,
    auto_tune:    &mut f32,
    settings:     &Settings,
    state:        &Arc<Mutex<SamplerState>>,
    midi_tx:      &Sender<MidiEvent>,
    playback:     &mut Option<PlaybackState>,
    action_rx:    &Receiver<MenuAction>,
) {
    if idx >= menu.len() { return; }
    match &menu[idx] {
        MenuItem::Instrument(inst) if idx != *current || inst.path() != current_path.as_path() => {
            while action_rx.try_recv().is_ok() {}
            raw_off();
            println!("\r\nLoading {}...", inst.name);
            let new_path = inst.path().clone();
            match load_instrument_data(inst, &Arc::new(std::sync::atomic::AtomicU32::new(0))) {
                Ok(loaded) => {
                    if let Ok(ref mut s) = state.lock() {
                        s.swap_instrument(
                            loaded.regions, loaded.samples, loaded.cc_defaults,
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
            raw_on();
        }
        MenuItem::MidiFile { path, .. } => {
            start_midi_playback(idx, path.clone(), midi_tx, playback);
        }
        _ => {}
    }
}
