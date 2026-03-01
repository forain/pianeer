mod region;
mod sfz;
mod organ;
mod kontakt;
mod gig;
mod sampler;
mod midi;
mod instruments;
mod midi_player;
mod audio;
mod loader;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;
use jack::{AudioOut, ClientOptions};

use sampler::{MidiEvent, SamplerState};
use audio::{Notifications, PianoProcessor};
use loader::{load_instrument_data, probe_sample_rate};
use ui::{stop_playback, playing_idx, print_menu, MenuItem, MenuAction, PlaybackState, Settings, ProcStats};

/// Read cumulative CPU ticks (utime+stime) from /proc/self/stat.
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

/// Read resident set size in MiB from /proc/self/status.
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


    if let Some(rate) = probed_rate {
        let existing = std::env::var("PIPEWIRE_PROPS").unwrap_or_default();
        if !existing.contains("audio.rate") {
            let props = if existing.is_empty() {
                format!("audio.rate={}", rate)
            } else {
                format!("audio.rate={} {}", rate, existing)
            };
            std::env::set_var("PIPEWIRE_PROPS", &props);
            println!("Set PIPEWIRE_PROPS audio.rate={}", rate);
        }
    }

    // 6. Create JACK client.
    let (client, _status) = jack::Client::new("pianeer", ClientOptions::NO_START_SERVER)
        .expect("Failed to open JACK client. Is JACK/PipeWire running?");

    let sample_rate = client.sample_rate() as f64;
    println!("JACK sample rate: {} Hz", sample_rate);

    if let Some(probed) = probed_rate {
        if probed != client.sample_rate() {
            eprintln!(
                "Warning: JACK sample rate ({} Hz) differs from sample file rate ({} Hz); \
                 pitch and timing may be incorrect.",
                client.sample_rate(), probed
            );
        }
    }

    // 7. Register output ports.
    let out_l = client
        .register_port("out_L", AudioOut::default())
        .expect("Failed to register out_L");
    let out_r = client
        .register_port("out_R", AudioOut::default())
        .expect("Failed to register out_R");

    // 8. Set up MIDI channel.
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

    // 10. Start MIDI input thread.
    let _midi_conn = match midi::start_midi(midi_tx.clone()) {
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

    // 11. Activate JACK client with process callback.
    let processor = PianoProcessor {
        out_l,
        out_r,
        state: Arc::clone(&state),
    };

    let active_client = client
        .activate_async(Notifications, processor)
        .expect("Failed to activate JACK client");

    // 12. Warm up: trigger a silent note before connecting outputs.
    let _ = midi_tx.send(MidiEvent::NoteOn { channel: 0, note: 60, velocity: 1 });
    thread::sleep(Duration::from_millis(150));
    let _ = midi_tx.send(MidiEvent::AllNotesOff);
    thread::sleep(Duration::from_millis(50));

    // 13. Auto-connect to system playback ports.
    let jack_client = active_client.as_client();
    let all_ports = jack_client.ports(None, None, jack::PortFlags::IS_INPUT);
    let playback_fl = all_ports.iter().find(|p| p.contains("playback_FL") || p.contains("playback_1")).cloned();
    let playback_fr = all_ports.iter().find(|p| p.contains("playback_FR") || p.contains("playback_2")).cloned();

    let client_name = jack_client.name().to_string();

    if let Some(ref fl) = playback_fl {
        let src = format!("{}:out_L", client_name);
        match jack_client.connect_ports_by_name(&src, fl) {
            Ok(_) => println!("Connected out_L -> {}", fl),
            Err(e) => eprintln!("Warning: could not connect out_L: {}", e),
        }
    } else {
        eprintln!("Warning: could not find playback_FL port.");
    }

    if let Some(ref fr) = playback_fr {
        let src = format!("{}:out_R", client_name);
        match jack_client.connect_ports_by_name(&src, fr) {
            Ok(_) => println!("Connected out_R -> {}", fr),
            Err(e) => eprintln!("Warning: could not connect out_R: {}", e),
        }
    } else {
        eprintln!("Warning: could not find playback_FR port.");
    }

    // 14. Enable raw mode and show menu.
    terminal::enable_raw_mode().expect("Failed to enable raw mode");
    let mut settings = Settings::default();
    let mut sustain_prev = false;
    let mut stats = ProcStats::default();
    let mut prev_cpu_ticks: u64 = read_cpu_ticks().unwrap_or(0);
    let mut prev_cpu_instant = std::time::Instant::now();
    let mut clip_until: Option<std::time::Instant> = None;
    print_menu(&menu, 0, 0, None, &settings, sustain_prev, &stats);

    // 15. Set up quit/reload flags and action channel.
    let quit = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    let (action_tx, action_rx) = bounded::<MenuAction>(8);

    // 16. Spawn input thread (reads crossterm key events).
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
                            _ => {}
                        }
                    }
                }
                Ok(false) => {} // poll timeout — loop again
                Err(_) => break,
            }
        }
    });

    // 17. Main loop: handle quit, reload, instrument switches, and MIDI file playback.
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

        let needs_redraw = sustain_now != sustain_prev || new_stats != stats;
        sustain_prev = sustain_now;
        stats = new_stats;
        if needs_redraw {
            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
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
            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
            continue;
        }

        // Redraw if MIDI playback ended naturally.
        if let Some(ref p) = playback {
            if p.done.load(Ordering::Relaxed) {
                playback = None;
                print_menu(&menu, cursor, current, None, &settings, sustain_prev, &stats);
            }
        }

        if let Ok(action) = action_rx.try_recv() {
            match action {
                MenuAction::CursorUp => {
                    if cursor > 0 { cursor -= 1; }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::CursorDown => {
                    if cursor + 1 < menu.len() { cursor += 1; }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::CycleVariant(dir) => {
                    if let Some(MenuItem::Instrument(inst)) = menu.get_mut(cursor) {
                        inst.cycle_variant(dir);
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::CycleVeltrack => {
                    settings.veltrack = settings.veltrack.cycle();
                    if let Ok(ref mut s) = state.lock() {
                        s.veltrack_override = settings.veltrack.override_pct();
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::CycleTune => {
                    settings.tune = settings.tune.cycle();
                    if let Ok(ref mut s) = state.lock() {
                        s.master_tune_semitones = settings.tune.semitones();
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::ToggleRelease => {
                    settings.release_enabled = !settings.release_enabled;
                    if let Ok(ref mut s) = state.lock() {
                        s.release_enabled = settings.release_enabled;
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::VolumeChange(delta) => {
                    settings.volume_db = (settings.volume_db + delta as f32 * 3.0).clamp(-12.0, 12.0);
                    if let Ok(ref mut s) = state.lock() {
                        s.master_volume = 10.0_f32.powf(settings.volume_db / 20.0);
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::TransposeChange(delta) => {
                    settings.transpose = (settings.transpose + delta as i32).clamp(-12, 12);
                    if let Ok(ref mut s) = state.lock() {
                        s.transpose = settings.transpose;
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                }
                MenuAction::ToggleResonance => {
                    settings.resonance_enabled = !settings.resonance_enabled;
                    if let Ok(ref mut s) = state.lock() {
                        s.resonance_enabled = settings.resonance_enabled;
                    }
                    print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
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
                                    }
                                    current = idx;
                                    current_path = new_path;
                                }
                                Err(e) => eprintln!("Error loading instrument: {}", e),
                            }
                            terminal::enable_raw_mode().ok();
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                        }
                        MenuItem::MidiFile { path, .. } => {
                            let path = path.clone();
                            let already_playing = playback.as_ref()
                                .map_or(false, |p| p.menu_idx == idx && !p.done.load(Ordering::Relaxed));
                            stop_playback(&mut playback);
                            if !already_playing {
                                let stop_flag = Arc::new(AtomicBool::new(false));
                                let done_flag = Arc::new(AtomicBool::new(false));
                                let handle = midi_player::spawn(
                                    path, midi_tx.clone(),
                                    Arc::clone(&stop_flag), Arc::clone(&done_flag),
                                );
                                playback = Some(PlaybackState {
                                    stop: stop_flag, done: done_flag,
                                    _handle: handle, menu_idx: idx,
                                });
                            }
                            print_menu(&menu, cursor, current, playing_idx(&playback), &settings, sustain_prev, &stats);
                        }
                        _ => {} // same instrument re-selected, ignore
                    }
                }
            }
        }
    }

    drop(active_client);
}
