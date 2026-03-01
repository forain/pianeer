mod region;
mod sfz;
mod organ;
mod sampler;
mod midi;
mod instruments;

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal;
use jack::{AudioOut, Client, ClientOptions, Control, NotificationHandler, ProcessHandler, ProcessScope};

use instruments::Instrument;
use region::Region;
use sampler::{LoadedSample, MidiEvent, SamplerState};

// ─── JACK process handler ────────────────────────────────────────────────────

struct PianoProcessor {
    out_l: jack::Port<AudioOut>,
    out_r: jack::Port<AudioOut>,
    state: Arc<Mutex<SamplerState>>,
}

impl ProcessHandler for PianoProcessor {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        let out_l_slice: &mut [f32] = self.out_l.as_mut_slice(ps);
        let out_r_slice: &mut [f32] = self.out_r.as_mut_slice(ps);

        if let Ok(ref mut state) = self.state.try_lock() {
            state.process(out_l_slice, out_r_slice);
        } else {
            for s in out_l_slice.iter_mut() { *s = 0.0; }
            for s in out_r_slice.iter_mut() { *s = 0.0; }
        }

        Control::Continue
    }
}

struct Notifications;
impl NotificationHandler for Notifications {
    fn xrun(&mut self, _: &Client) -> Control {
        eprintln!("JACK xrun detected");
        Control::Continue
    }
}

// ─── Sample rate probing ──────────────────────────────────────────────────────

/// Open a file with Symphonia and read the codec sample rate without decoding audio.
fn probe_sample_rate(path: &Path) -> Option<u32> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    probed.format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .and_then(|t| t.codec_params.sample_rate)
}

// ─── Sample loading ───────────────────────────────────────────────────────────

fn load_sample(path: &Path, loop_start: Option<u64>, loop_end: Option<u64>) -> Result<LoadedSample, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path)
        .map_err(|e| format!("open {:?}: {}", path, e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe {:?}: {}", path, e))?;

    let mut format = probed.format;
    let track = format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| format!("no audio track in {:?}", path))?;

    let n_channels = track.codec_params.channels
        .map(|c| c.count())
        .unwrap_or(2);
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder {:?}: {}", path, e))?;

    let mut pcm: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(format!("packet {:?}: {}", path, e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode {:?}: {}", path, e)),
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        if n_channels == 2 {
            pcm.extend_from_slice(samples);
        } else {
            for &s in samples {
                pcm.push(s);
                pcm.push(s);
            }
        }
    }

    let frames = pcm.len() / 2;
    Ok(LoadedSample {
        data: pcm,
        frames,
        loop_start: loop_start.map(|v| v as usize),
        loop_end: loop_end.map(|v| v as usize),
    })
}

fn load_all_samples_parallel(regions: &[Region]) -> Vec<Option<LoadedSample>> {
    let total = regions.len();
    println!("Loading {} samples...", total);

    let entries: Vec<_> = regions.iter()
        .map(|r| (r.sample.clone(), r.loop_start, r.loop_end))
        .collect();

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let chunk_size = (total + num_threads - 1) / num_threads;
    let entries = Arc::new(entries);

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let entries = Arc::clone(&entries);
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                let start = t * chunk_size;
                let end = (start + chunk_size).min(total);
                let mut results: Vec<(usize, Option<LoadedSample>)> = Vec::new();
                for i in start..end {
                    let (ref path, loop_start, loop_end) = entries[i];
                    let loaded = match load_sample(path, loop_start, loop_end) {
                        Ok(s) => {
                            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total {
                                println!("  Loaded {}/{}", done, total);
                            }
                            Some(s)
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to load {:?}: {}", path, e);
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            None
                        }
                    };
                    results.push((i, loaded));
                }
                results
            })
        })
        .collect();

    let mut all_results: Vec<(usize, Option<LoadedSample>)> = Vec::with_capacity(total);
    for handle in handles {
        if let Ok(chunk) = handle.join() {
            all_results.extend(chunk);
        }
    }

    all_results.sort_by_key(|(i, _)| *i);
    all_results.into_iter().map(|(_, s)| s).collect()
}

// ─── Instrument loading ───────────────────────────────────────────────────────

fn load_instrument_data(inst: &Instrument) -> Result<(Vec<Region>, Vec<Option<LoadedSample>>), String> {
    let ext = inst.path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let regions = match ext.as_str() {
        "sfz" => {
            println!("Parsing SFZ: {}", inst.path.display());
            sfz::parse_sfz(&inst.path).map_err(|e| format!("Error parsing SFZ: {}", e))?
        }
        "organ" => {
            println!("Parsing Grand Orgue ODF: {}", inst.path.display());
            organ::parse_organ(&inst.path).map_err(|e| format!("Error parsing ODF: {}", e))?
        }
        other => return Err(format!("Unknown file extension '{}'", other)),
    };
    println!("Parsed {} regions.", regions.len());
    let samples = load_all_samples_parallel(&regions);
    println!("All samples loaded.");
    Ok((regions, samples))
}

// ─── Menu display ─────────────────────────────────────────────────────────────

fn print_menu(instruments: &[Instrument], current: usize) {
    // Clear screen and home cursor, then draw menu with raw-mode line endings.
    print!("\x1b[2J\x1b[H");
    print!("Instruments (press number to switch, Q / Ctrl+C to quit):\r\n");
    for (i, inst) in instruments.iter().enumerate() {
        if i == current {
            print!(" *{}. {}\r\n", i + 1, inst.name);
        } else {
            print!("  {}. {}\r\n", i + 1, inst.name);
        }
    }
    std::io::stdout().flush().ok();
}

// ─── Main ─────────────────────────────────────────────────────────────────────

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
    let instruments = instruments::discover(&samples_dir);
    if instruments.is_empty() {
        eprintln!("Error: no .sfz or .organ files found under {}", samples_dir.display());
        std::process::exit(1);
    }

    // 3. Load first instrument.
    let (regions, samples) = match load_instrument_data(&instruments[0]) {
        Ok(data) => data,
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    };

    // 4. Probe sample rate from the first region's sample file.
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

    // 5. Create JACK client.
    let (client, _status) = Client::new("pianeer", ClientOptions::NO_START_SERVER)
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

    // 6. Register output ports.
    let out_l = client
        .register_port("out_L", AudioOut::default())
        .expect("Failed to register out_L");
    let out_r = client
        .register_port("out_R", AudioOut::default())
        .expect("Failed to register out_R");

    // 7. Set up MIDI channel.
    let (midi_tx, midi_rx) = bounded::<MidiEvent>(4096);

    // 8. Build sampler state.
    let state = Arc::new(Mutex::new(SamplerState::new(
        regions,
        samples,
        sample_rate,
        midi_rx,
    )));

    // 9. Start MIDI input thread.
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

    // 10. Activate JACK client with process callback.
    let processor = PianoProcessor {
        out_l,
        out_r,
        state: Arc::clone(&state),
    };

    let active_client = client
        .activate_async(Notifications, processor)
        .expect("Failed to activate JACK client");

    // 11. Warm up: trigger a silent note before connecting outputs.
    let _ = midi_tx.send(MidiEvent::NoteOn { channel: 0, note: 60, velocity: 1 });
    thread::sleep(Duration::from_millis(150));
    let _ = midi_tx.send(MidiEvent::AllNotesOff);
    thread::sleep(Duration::from_millis(50));

    // 12. Auto-connect to system playback ports.
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

    // 13. Enable raw mode and show menu.
    terminal::enable_raw_mode().expect("Failed to enable raw mode");
    print_menu(&instruments, 0);

    // 14. Set up quit flag and switch channel.
    let quit = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = bounded::<usize>(8);

    // 15. Spawn input thread (reads crossterm key events).
    let quit_input = Arc::clone(&quit);
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
                            KeyCode::Char(c @ '1'..='9') => {
                                let idx = (c as usize) - ('1' as usize);
                                let _ = switch_tx.try_send(idx);
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

    // 16. Main loop: handle quit and instrument switches.
    let mut current = 0usize;
    loop {
        thread::sleep(Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            terminal::disable_raw_mode().ok();
            println!("\r\nExiting...");
            break;
        }

        if let Ok(idx) = switch_rx.try_recv() {
            if idx < instruments.len() && idx != current {
                // Drain any additional queued switch events.
                while switch_rx.try_recv().is_ok() {}

                terminal::disable_raw_mode().ok();
                println!("\r\nLoading {}...", instruments[idx].name);

                match load_instrument_data(&instruments[idx]) {
                    Ok((regions, samples)) => {
                        if let Ok(ref mut s) = state.lock() {
                            s.swap_instrument(regions, samples);
                        }
                        current = idx;
                    }
                    Err(e) => {
                        eprintln!("Error loading instrument: {}", e);
                    }
                }

                terminal::enable_raw_mode().ok();
                print_menu(&instruments, current);
            }
        }
    }

    drop(active_client);
}
