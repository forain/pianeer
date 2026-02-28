mod sfz;
mod sampler;
mod midi;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use jack::{AudioOut, Client, ClientOptions, Control, NotificationHandler, ProcessHandler, ProcessScope};

use sampler::{LoadedSample, MidiEvent, SamplerState};
use sfz::parse_sfz;

const SFZ_PATH: &str = "/usr/share/sounds/SalamanderGrandPiano-SFZ+FLAC-V3+20200602/SalamanderGrandPiano-V3+20200602.sfz";

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

        // try_lock to stay real-time safe: skip the frame if locked.
        if let Ok(ref mut state) = self.state.try_lock() {
            state.process(out_l_slice, out_r_slice);
        } else {
            // Silent frame rather than blocking.
            for s in out_l_slice.iter_mut() { *s = 0.0; }
            for s in out_r_slice.iter_mut() { *s = 0.0; }
        }

        Control::Continue
    }
}

// ─── JACK notification handler (no-op) ───────────────────────────────────────

struct Notifications;
impl NotificationHandler for Notifications {
    fn xrun(&mut self, _: &Client) -> Control {
        eprintln!("JACK xrun detected");
        Control::Continue
    }
}

// ─── Sample loading ───────────────────────────────────────────────────────────

fn load_sample(path: &Path) -> Result<LoadedSample, String> {
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::core::audio::SampleBuffer;

    let file = std::fs::File::open(path)
        .map_err(|e| format!("open {:?}: {}", path, e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");

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
    Ok(LoadedSample { data: pcm, frames })
}

fn load_all_samples_parallel(regions: &[sfz::Region]) -> Vec<Option<LoadedSample>> {
    let total = regions.len();
    println!("Loading {} samples...", total);

    // Collect paths first.
    let paths: Vec<_> = regions.iter().map(|r| r.sample.clone()).collect();

    // Use a thread pool sized to number of CPUs.
    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);

    // Shared progress counter.
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Split into chunks and load in parallel.
    let chunk_size = (total + num_threads - 1) / num_threads;
    let paths = Arc::new(paths);

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let paths = Arc::clone(&paths);
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                let start = t * chunk_size;
                let end = (start + chunk_size).min(total);
                let mut results: Vec<(usize, Option<LoadedSample>)> = Vec::new();
                for i in start..end {
                    let loaded = match load_sample(&paths[i]) {
                        Ok(s) => {
                            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total {
                                println!("  Loaded {}/{}", done, total);
                            }
                            Some(s)
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to load {:?}: {}", paths[i], e);
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

    // Re-sort by original index.
    all_results.sort_by_key(|(i, _)| *i);
    all_results.into_iter().map(|(_, s)| s).collect()
}

// ─── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    // 1. Parse SFZ.
    println!("Parsing SFZ: {}", SFZ_PATH);
    let regions = match parse_sfz(Path::new(SFZ_PATH)) {
        Ok(r) => {
            println!("Parsed {} regions.", r.len());
            r
        }
        Err(e) => {
            eprintln!("Error parsing SFZ: {}", e);
            std::process::exit(1);
        }
    };

    // 2. Load all samples into RAM.
    let samples = load_all_samples_parallel(&regions);
    println!("All samples loaded.");

    // 3. Create JACK client.
    let (client, _status) = Client::new("pianosampler", ClientOptions::NO_START_SERVER)
        .expect("Failed to open JACK client. Is JACK/PipeWire running?");

    let sample_rate = client.sample_rate() as f64;
    println!("JACK sample rate: {} Hz", sample_rate);

    // 4. Register output ports.
    let out_l = client
        .register_port("out_L", AudioOut::default())
        .expect("Failed to register out_L");
    let out_r = client
        .register_port("out_R", AudioOut::default())
        .expect("Failed to register out_R");

    // 5. Set up MIDI channel.
    let (midi_tx, midi_rx) = bounded::<MidiEvent>(4096);

    // 6. Build sampler state.
    let state = Arc::new(Mutex::new(SamplerState::new(
        regions,
        samples,
        sample_rate,
        midi_rx,
    )));

    // 7. Start MIDI input thread.
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

    // 8. Activate JACK client with process callback.
    let processor = PianoProcessor {
        out_l,
        out_r,
        state: Arc::clone(&state),
    };

    let active_client = client
        .activate_async(Notifications, processor)
        .expect("Failed to activate JACK client");

    // 9. Warm up: trigger a silent note before connecting outputs so the rendering
    //    hot path and CPU frequency are warm when the user plays the first key.
    let _ = midi_tx.send(MidiEvent::NoteOn { channel: 0, note: 60, velocity: 1 });
    thread::sleep(Duration::from_millis(150));
    let _ = midi_tx.send(MidiEvent::AllNotesOff);
    thread::sleep(Duration::from_millis(50));

    // 10. Auto-connect to system playback ports.
    let jack_client = active_client.as_client();

    // Find ports matching playback_FL and playback_FR.
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

    println!("Ready. Press Ctrl+C to quit.");

    // 10. Block until Ctrl+C.
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
