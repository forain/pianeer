mod midi;
mod audio;
mod ui;
mod terminal;
#[cfg(feature = "native-ui")]
mod gui;
#[cfg(all(feature = "kms", target_os = "linux"))]
mod drm_ui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crossbeam_channel::bounded;

use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::loader::{
    load_instrument_data, probe_sample_rate, probe_instrument_rate, LoadingJob,
    last_instrument_path, save_last_instrument_path,
};
use pianeer_core::{instruments, midi_player, midi_recorder};
use pianeer_core::types::{MenuAction, MenuItem};

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

    let use_text = std::env::args().any(|a| a == "--text" || a == "-t");

    // 4. Load first instrument.
    //    GUI modes: start an async LoadingJob so progress is shown in the egui window.
    //    Terminal mode: synchronous load with progress printed to stdout.
    //    Restore last session's instrument when the path still exists on disk.
    let state_file = samples_dir.join(".last_instrument");
    let saved_path = last_instrument_path(&state_file);
    let (first_idx, first_inst) = {
        let found = saved_path.as_ref().and_then(|saved| {
            menu.iter().enumerate().find_map(|(i, m)| {
                if let MenuItem::Instrument(inst) = m {
                    if inst.path() == saved.as_path() { Some((i, inst)) } else { None }
                } else { None }
            })
        });
        found.or_else(|| {
            menu.iter().enumerate()
                .find_map(|(i, m)| if let MenuItem::Instrument(inst) = m { Some((i, inst)) } else { None })
        }).unwrap()
    };

    let (regions, samples, first_cc, first_sw_lo, first_sw_hi, first_sw_def,
         probed_rate, initial_loading_job) =
        if !use_text && will_use_gui_path() {
            let probed_rate = probe_instrument_rate(first_inst);
            let job = LoadingJob::start(first_inst.clone());
            (vec![], vec![], [0u8; 128], 0u8, 0u8, None,
             probed_rate, Some((first_idx, job)))
        } else {
            let loaded = match load_instrument_data(first_inst, &Arc::new(std::sync::atomic::AtomicU32::new(0))) {
                Ok(data) => data,
                Err(e) => { eprintln!("{}", e); std::process::exit(1); }
            };
            let probed_rate = loaded.regions.iter()
                .find(|r| !r.sample.as_os_str().is_empty())
                .and_then(|r| probe_sample_rate(&r.sample));
            save_last_instrument_path(&state_file, first_inst.path());
            (loaded.regions, loaded.samples, loaded.cc_defaults,
             loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
             probed_rate, None)
        };

    // 6. Open audio device.
    let audio_probe = audio::probe_audio(probed_rate)
        .unwrap_or_else(|e| { eprintln!("{}", e); std::process::exit(1); });
    let sample_rate = audio_probe.sample_rate as f64;

    // 7. Set up MIDI channel.
    let (midi_tx, midi_rx) = bounded::<MidiEvent>(4096);

    // 8. Build sampler state.
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

    // Auto-detect the pitch offset needed to make A4 sound at 440 Hz.
    let auto_tune: f32 = state.lock().unwrap().detect_a4_tune_correction();
    if let Ok(ref mut s) = state.lock() {
        s.master_tune_semitones = auto_tune;
    }

    // 9. Start MIDI watcher thread.
    let record_handle = midi_recorder::new_handle();
    midi::spawn_midi_watcher(midi_tx.clone(), Arc::clone(&record_handle));

    // 10. Start audio stream.
    let _audio = audio::start_audio(audio_probe, Arc::clone(&state))
        .unwrap_or_else(|e| { eprintln!("{}", e); std::process::exit(1); });

    // 11. Set up quit/reload flags, action channel, and web UI.
    let quit   = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    let (action_tx, action_rx) = bounded::<MenuAction>(8);
    // show_qr is only used by the terminal path but always allocated.
    let show_qr = Arc::new(AtomicBool::new(false));

    let web_snapshot: Arc<Mutex<String>> = Arc::new(Mutex::new("{}".to_string()));
    {
        let (rb_prod, rb_cons) = ringbuf::HeapRb::<f32>::new(
            (sample_rate as usize) * 2 * 2 // 2 s headroom, stereo
        ).split();
        let audio_handle = pianeer_core::audio_stream::start(rb_cons, sample_rate as u32);
        state.lock().unwrap().audio_sink = Some(rb_prod);
        // Dropping the JoinHandle detaches the thread; it keeps running.
        pianeer_core::web::spawn_web_server(
            Arc::clone(&web_snapshot),
            action_tx.clone(),
            Arc::clone(&reload),
            Arc::new(audio_handle),
        );
    }

    // 12. Choose UI mode.
    //   --text  : always use the raw-mode terminal
    //   default : native GUI (eframe) when a display server is running,
    //             DRM/KMS egui when running on bare Linux with no display server,
    //             terminal as final fallback.

    if use_text {
        terminal::run_terminal(
            menu, state, midi_tx, record_handle, midi_dir,
            samples_dir, web_snapshot, action_tx, action_rx,
            auto_tune, quit, reload, show_qr, first_idx,
        );
        return;
    }

    // Native eframe GUI — only when a display server is available.
    #[cfg(feature = "native-ui")]
    if has_display() {
        gui::run_native_ui(
            menu, state, midi_tx, record_handle, midi_dir,
            samples_dir, web_snapshot, action_tx, action_rx,
            auto_tune, quit, reload, initial_loading_job,
        );
        return;
    }

    // DRM/KMS — Linux only, when compiled with the `kms` feature and no display server.
    #[cfg(all(feature = "kms", target_os = "linux"))]
    if drm_ui::drm_available() {
        drm_ui::run_drm_ui(
            menu, state, midi_tx, record_handle, midi_dir,
            samples_dir, web_snapshot, action_tx, action_rx,
            auto_tune, quit, reload, initial_loading_job,
        );
        return;
    }

    // Final fallback: terminal.
    terminal::run_terminal(
        menu, state, midi_tx, record_handle, midi_dir,
        samples_dir, web_snapshot, action_tx, action_rx,
        auto_tune, quit, reload, show_qr, first_idx,
    );
}

/// True when startup will use a GUI mode (native-ui or KMS) rather than the terminal.
/// Used to decide whether to load the first instrument asynchronously.
fn will_use_gui_path() -> bool {
    #[cfg(feature = "native-ui")]
    if has_display() { return true; }
    #[cfg(all(feature = "kms", target_os = "linux"))]
    if drm_ui::drm_available() { return true; }
    false
}

/// Returns true when a Wayland or X11 display server is reachable.
/// On non-Linux platforms a display is always assumed available.
fn has_display() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
