mod region;
mod sfz;
mod organ;
mod kontakt;
mod gig;
mod sampler;
mod midi;
mod midi_recorder;
mod instruments;
mod midi_player;
mod audio;
mod loader;
mod ui;
mod web;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;

use sampler::{MidiEvent, SamplerState};
use loader::{load_instrument_data, probe_sample_rate};
use ui::{
    stop_playback, playing_idx, print_menu,
    MenuItem, MenuAction, PlaybackState, PlaybackInfo, Settings, ProcStats,
};

/// Cumulative CPU ticks (utime+stime) from /proc/self/stat.
/// One tick = 1/CLK_TCK second; CLK_TCK is 100 on all common Linux systems.
#[cfg(target_os = "linux")]
fn read_cpu_ticks() -> Option<u64> {
    let data = std::fs::read_to_string("/proc/self/stat").ok()?;
    // comm field may contain spaces; find the last ')' to skip it.
    let after_comm = data.rfind(')')?.checked_add(1)?;
    let rest = data[after_comm..].trim_start();
    // Fields after ')': state ppid pgrp session tty_nr tpgid flags
    //                    minflt cminflt majflt cmajflt utime(11) stime(12)
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Cumulative CPU centiseconds (utime+stime) via getrusage.
/// Returns centiseconds (100ths of a second) to match the Linux CLK_TCK=100
/// convention so the same `delta / 100.0 / elapsed * 100` formula applies.
#[cfg(target_os = "macos")]
fn read_cpu_ticks() -> Option<u64> {
    use std::mem::MaybeUninit;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let u = unsafe { usage.assume_init() };
    let utime_cs = u.ru_utime.tv_sec as u64 * 100 + u.ru_utime.tv_usec as u64 / 10_000;
    let stime_cs = u.ru_stime.tv_sec as u64 * 100 + u.ru_stime.tv_usec as u64 / 10_000;
    Some(utime_cs + stime_cs)
}

/// Resident set size in MiB from /proc/self/status.
#[cfg(target_os = "linux")]
fn read_mem_mb() -> u32 {
    let data = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in data.lines() {
        if line.starts_with("VmRSS:") {
            if let Some(kb) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()) {
                return kb / 1024;
            }
        }
    }
    0
}

/// Peak RSS in MiB via getrusage.
/// On macOS ru_maxrss is in bytes and reflects peak RSS.  For a sampler that
/// loads all samples at startup and holds them in memory, peak ≈ current.
#[cfg(target_os = "macos")]
fn read_mem_mb() -> u32 {
    use std::mem::MaybeUninit;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let u = unsafe { usage.assume_init() };
    (u.ru_maxrss as u64 / 1024 / 1024) as u32
}

fn main() {
    // 1. Find samples directory.
    let samples_dir = match instruments::find_samples_dir() {
        Some(dir) => dir,
        None => {
            eprintln!("Error: could not find a samples/ directory.");
            eprintln!("Place instrument subdirectories under samples/ next to the binary.");
            std::process::exit(1);
        }
    };

    println!("Scanning instruments in {}...", samples_dir.display());

    // 2. Discover instruments.
    let raw_instruments = instruments::discover(&samples_dir);
    if raw_instruments.is_empty() {
        eprintln!("Error: no .sfz or .organ files found under {}", samples_dir.display());
        std::process::exit(1);
    }

    // 3. Discover MIDI files and build unified menu.
    let midi_dir = midi_player::find_midi_dir();
    let midi_files = match midi_dir.as_ref() {
        Some(dir) => midi_player::discover(dir),
        None => Vec::new(),
    };
    let mut menu: Vec<MenuItem> = raw_instruments.into_iter().map(MenuItem::Instrument).collect();
    for (name, path) in midi_files {
        menu.push(MenuItem::MidiFile { name, path });
    }

    // 4. Load first instrument.
    let first_inst = menu.iter().find_map(|m| {
        if let MenuItem::Instrument(i) = m { Some(i) } else { None }
    }).unwrap(); // guaranteed: raw_instruments was non-empty
    let first_loaded = match load_instrument_data(first_inst) {
        Ok(data) => data,
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    };
    let (regions, samples, first_cc, first_sw_lo, first_sw_hi, first_sw_def) = (
        first_loaded.regions, first_loaded.samples,
        first_loaded.cc_defaults, first_loaded.sw_lokey,
        first_loaded.sw_hikey, first_loaded.sw_default,
    );

    // 5. Probe sample rate from the first region's sample file.
    let probed_rate: Option<u32> = regions.iter()
        .find(|r| !r.sample.as_os_str().is_empty())
        .and_then(|r| probe_sample_rate(&r.sample));

    // 6. Open audio device; on Linux this connects to JACK/PipeWire, on macOS
    //    to CoreAudio.  The negotiated sample rate is needed before we build
    //    SamplerState, so audio starts in two phases: probe → build state → start.
    let audio_probe = audio::probe_audio(probed_rate)
        .unwrap_or_else(|e| { eprintln!("{}", e); std::process::exit(1); });
    let sample_rate = audio_probe.sample_rate as f64;

    // 7. Set up MIDI channel.
    let (midi_tx, midi_rx) = bounded::<MidiEvent>(4096);

    // 9. Build sampler state.
    let state = Arc::new(Mutex::new(SamplerState::new(
        regions,
        samples,
        sample_rate,
        midi_rx,
        first_cc,
        first_sw_lo,
        first_sw_hi,
        first_sw_def,
    )));

    // Auto-detect the pitch offset needed to make A4 (note 69) sound at 440 Hz.
    let mut auto_tune: f32 = state.lock().unwrap().detect_a4_tune_correction();
    if let Ok(ref mut s) = state.lock() {
        s.master_tune_semitones = auto_tune; // settings.tune defaults to A440 (0.0)
    }

    // 10. Start MIDI input thread.
    let record_handle = midi_recorder::new_handle();
    let _midi_conn = match midi::start_midi(midi_tx.clone(), Arc::clone(&record_handle)) {
        Ok(conn) => {
            println!("MIDI input connected.");
            conn
        }
        Err(e) => {
            eprintln!("Warning: MIDI connection failed: {}.", e);
            eprintln!("Hint: make sure the Keystation is plugged in.");
            std::process::exit(1);
        }
    };

    // 11. Start audio stream (activates JACK on Linux, CoreAudio on macOS;
    //     JACK backend also auto-connects to system playback ports).
    let _audio = audio::start_audio(audio_probe, Arc::clone(&state))
        .unwrap_or_else(|e| { eprintln!("{}", e); std::process::exit(1); });

    // 13. Enable raw mode and show menu.
    terminal::enable_raw_mode().expect("Failed to enable raw mode");
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

    // 14. Set up quit/reload flags, action channel, and web UI.
    let quit = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    let (action_tx, action_rx) = bounded::<MenuAction>(8);

    let web_snapshot: Arc<Mutex<String>> = Arc::new(Mutex::new("{}".to_string()));
    let _web_thread = web::spawn_web_server(
        Arc::clone(&web_snapshot),
        action_tx.clone(),
        Arc::clone(&reload),
    );

    // 15. Spawn input thread (reads crossterm key events).
    let quit_input = Arc::clone(&quit);
    let reload_input = Arc::clone(&reload);
    thread::spawn(move || {
        loop {
            if quit_input.load(Ordering::Relaxed) {
                break;
            }
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if let Ok(Event::Key(key)) = event::read() {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                quit_input.store(true, Ordering::SeqCst);
                                break;
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                quit_input.store(true, Ordering::SeqCst);
                                break;
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                reload_input.store(true, Ordering::SeqCst);
                            }
                            KeyCode::Up => {
                                let _ = action_tx.try_send(MenuAction::CursorUp);
                            }
                            KeyCode::Down => {
                                let _ = action_tx.try_send(MenuAction::CursorDown);
                            }
                            KeyCode::Left => {
                                let _ = action_tx.try_send(MenuAction::CycleVariant(-1));
                            }
                            KeyCode::Right => {
                                let _ = action_tx.try_send(MenuAction::CycleVariant(1));
                            }
                            KeyCode::Enter => {
                                let _ = action_tx.try_send(MenuAction::Select);
                            }
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
                            _ => {}
                        }
                    }
                }
                Ok(false) => {} // poll timeout — loop again
                Err(_) => break,
            }
        }
    });

    // 16. Main loop: handle quit, reload, instrument switches, and MIDI file playback.
    let mut current = 0usize; // index in `menu` of the loaded instrument
    let mut current_path = menu.iter()
        .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
        .unwrap_or_default();
    let mut cursor = 0usize;  // index of the highlighted menu row
    let mut playback: Option<PlaybackState> = None;

    loop {
        thread::sleep(Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            stop_playback(&mut playback);
            terminal::disable_raw_mode().ok();
            println!("\r\nExiting...");
            break;
        }

        // Poll sampler state and system stats; redraw if anything changed.
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
            clip_until = Some(now + std::time::Duration::from_secs(2));
        }
        let clip_holding = clip_until.map(|t| now < t).unwrap_or(false);

        let elapsed = now.duration_since(prev_cpu_instant).as_secs_f32().max(0.001);
        let ticks_now = read_cpu_ticks().unwrap_or(prev_cpu_ticks);
        // CLK_TCK is 100 on virtually all Linux systems.
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

        // Redraw on stats/sustain change, or whenever MIDI playback is active.
        let active_playback = playback.as_ref().map(|p| !p.done.load(Ordering::Relaxed)).unwrap_or(false);
        let recording_active = midi_recorder::is_recording(&record_handle);
        let needs_redraw = sustain_now != sustain_prev
            || new_stats != stats
            || active_playback
            || recording_active;

        sustain_prev = sustain_now;
        stats = new_stats;

        // Convenience: derive pb_info + recording elapsed here.
        let pb_info = mk_pb_info(&playback);
        let rec_elapsed = midi_recorder::elapsed(&record_handle);

        // Update web UI snapshot every loop iteration (~10 Hz).
        {
            let snap = web::build_snapshot(
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

        if needs_redraw {
            print_menu(
                &menu, cursor, current, playing_idx(&playback),
                &settings, sustain_prev, &stats,
                recording_active, rec_elapsed, pb_info.as_ref(),
            );
        }

        // Rescan instruments and MIDI files on R.
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
            // Keep current index pointing at the same instrument if still present.
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
                print_menu(
                    &menu, cursor, current, None,
                    &settings, sustain_prev, &stats,
                    midi_recorder::is_recording(&record_handle),
                    midi_recorder::elapsed(&record_handle), None,
                );
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
                        // Stop recording → save file → rescan menu.
                        if let Some(buf) = midi_recorder::stop(&record_handle) {
                            let save_dir = midi_dir.as_deref().unwrap_or_else(|| {
                                std::path::Path::new("midi")
                            });
                            // Ensure directory exists.
                            let _ = std::fs::create_dir_all(save_dir);
                            match midi_recorder::save(buf, save_dir) {
                                Ok(path) => {
                                    terminal::disable_raw_mode().ok();
                                    println!("\r\nRecording saved: {}", path.display());
                                    terminal::enable_raw_mode().ok();
                                }
                                Err(e) => {
                                    terminal::disable_raw_mode().ok();
                                    eprintln!("\r\nFailed to save recording: {}", e);
                                    terminal::enable_raw_mode().ok();
                                }
                            }
                            // Trigger rescan so the new file appears in the menu.
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
                            terminal::disable_raw_mode().ok();
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
                            terminal::enable_raw_mode().ok();
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
                MenuAction::Select => {
                    let idx = cursor;
                    match &menu[idx] {
                        MenuItem::Instrument(inst)
                            if idx != current || inst.path() != &current_path =>
                        {
                            // Drain rapid key repeats; keep MIDI playing across instrument switches.
                            while action_rx.try_recv().is_ok() {}
                            terminal::disable_raw_mode().ok();
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
                            terminal::enable_raw_mode().ok();
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
                        _ => {} // same instrument re-selected, ignore
                    }
                }
            }
        }
    }

}

