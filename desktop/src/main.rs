mod midi;
mod audio;
mod ui;
mod terminal;
#[cfg(feature = "native-ui")]
mod gui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crossbeam_channel::bounded;

use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::loader::{load_instrument_data, probe_sample_rate};
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

    // 4. Load first instrument.
    let first_inst = menu.iter().find_map(|m| {
        if let MenuItem::Instrument(i) = m { Some(i) } else { None }
    }).unwrap();
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
    #[cfg(not(feature = "native-ui"))]
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

    // 12. Run either the native GUI or the raw-mode terminal.
    #[cfg(feature = "native-ui")]
    gui::run_native_ui(
        menu, state, midi_tx, record_handle, midi_dir,
        samples_dir, web_snapshot, action_tx, action_rx,
        auto_tune, quit, reload,
    );

    #[cfg(not(feature = "native-ui"))]
    terminal::run_terminal(
        menu, state, midi_tx, record_handle, midi_dir,
        samples_dir, web_snapshot, action_tx, action_rx,
        auto_tune, quit, reload, show_qr,
    );
}
