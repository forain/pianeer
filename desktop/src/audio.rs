use std::sync::{Arc, Mutex};

use pianeer_core::sampler::SamplerState;

// ── Linux / JACK ─────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod jack_impl {
    use std::sync::{Arc, Mutex};

    use jack::{AudioOut, Client, ClientOptions, Control, NotificationHandler, ProcessHandler,
               ProcessScope};

    use pianeer_core::sampler::SamplerState;

    pub struct JackProbe {
        pub sample_rate: u32,
        client: Client,
        out_l: jack::Port<AudioOut>,
        out_r: jack::Port<AudioOut>,
    }

    pub fn probe(probed_rate: Option<u32>) -> Result<JackProbe, String> {
        // Set PipeWire sample rate before the JACK client connects.
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

        let (client, _status) = Client::new("pianeer", ClientOptions::NO_START_SERVER)
            .map_err(|e| format!("Failed to open JACK client (is JACK/PipeWire running?): {}", e))?;

        let sample_rate = client.sample_rate() as u32;
        println!("JACK sample rate: {} Hz", sample_rate);

        if let Some(probed) = probed_rate {
            if probed != sample_rate {
                eprintln!(
                    "Warning: JACK sample rate ({} Hz) differs from sample file rate ({} Hz); \
                     pitch and timing may be incorrect.",
                    sample_rate, probed
                );
            }
        }

        let out_l = client.register_port("out_L", AudioOut::default())
            .map_err(|e| format!("Failed to register out_L: {}", e))?;
        let out_r = client.register_port("out_R", AudioOut::default())
            .map_err(|e| format!("Failed to register out_R: {}", e))?;

        Ok(JackProbe { sample_rate, client, out_l, out_r })
    }

    struct PianoProcessor {
        out_l: jack::Port<AudioOut>,
        out_r: jack::Port<AudioOut>,
        state: Arc<Mutex<SamplerState>>,
    }

    impl ProcessHandler for PianoProcessor {
        fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
            let out_l_slice = self.out_l.as_mut_slice(ps);
            let out_r_slice = self.out_r.as_mut_slice(ps);
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

    pub struct JackActive {
        _active_client: jack::AsyncClient<Notifications, PianoProcessor>,
    }

    pub fn start(probe: JackProbe, state: Arc<Mutex<SamplerState>>) -> Result<JackActive, String> {
        let processor = PianoProcessor { out_l: probe.out_l, out_r: probe.out_r, state };
        let active_client = probe.client
            .activate_async(Notifications, processor)
            .map_err(|e| format!("Failed to activate JACK client: {}", e))?;

        // Auto-connect to system playback ports.
        let jc = active_client.as_client();
        let all_ports = jc.ports(None, None, jack::PortFlags::IS_INPUT);
        let name = jc.name().to_string();

        let playback_fl = all_ports.iter()
            .find(|p| p.contains("playback_FL") || p.contains("playback_1")).cloned();
        let playback_fr = all_ports.iter()
            .find(|p| p.contains("playback_FR") || p.contains("playback_2")).cloned();

        if let Some(ref fl) = playback_fl {
            match jc.connect_ports_by_name(&format!("{}:out_L", name), fl) {
                Ok(_) => println!("Connected out_L -> {}", fl),
                Err(e) => eprintln!("Warning: could not connect out_L: {}", e),
            }
        } else {
            eprintln!("Warning: could not find playback_FL port.");
        }

        if let Some(ref fr) = playback_fr {
            match jc.connect_ports_by_name(&format!("{}:out_R", name), fr) {
                Ok(_) => println!("Connected out_R -> {}", fr),
                Err(e) => eprintln!("Warning: could not connect out_R: {}", e),
            }
        } else {
            eprintln!("Warning: could not find playback_FR port.");
        }

        Ok(JackActive { _active_client: active_client })
    }
}

// ── macOS / CoreAudio via cpal ────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod cpal_impl {
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use pianeer_core::sampler::SamplerState;

    pub struct CpalProbe {
        pub sample_rate: u32,
        device: cpal::Device,
        config: cpal::StreamConfig,
        channels: usize,
    }

    pub fn probe(probed_rate: Option<u32>) -> Result<CpalProbe, String> {
        let host = cpal::default_host();
        let device = host.default_output_device()
            .ok_or_else(|| "No default audio output device found".to_string())?;
        let supported = device.default_output_config()
            .map_err(|e| format!("Could not get default output config: {}", e))?;

        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        println!(
            "CoreAudio: device={:?}, sample_rate={} Hz, channels={}",
            device.name().as_deref().unwrap_or("unknown"), sample_rate, channels
        );

        if let Some(probed) = probed_rate {
            if probed != sample_rate {
                eprintln!(
                    "Warning: CoreAudio sample rate ({} Hz) differs from sample file rate ({} Hz); \
                     pitch and timing may be incorrect.",
                    sample_rate, probed
                );
            }
        }

        let config: cpal::StreamConfig = supported.into();
        Ok(CpalProbe { sample_rate, device, config, channels })
    }

    pub struct CpalActive {
        _stream: cpal::Stream,
    }

    pub fn start(probe: CpalProbe, state: Arc<Mutex<SamplerState>>) -> Result<CpalActive, String> {
        let channels = probe.channels;
        let stream = probe.device.build_output_stream(
            &probe.config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels;
                let mut buf_l = vec![0f32; frames];
                let mut buf_r = vec![0f32; frames];
                if let Ok(ref mut s) = state.try_lock() {
                    s.process(&mut buf_l, &mut buf_r);
                }
                // Interleave L/R into the output buffer.
                for i in 0..frames {
                    data[i * channels] = buf_l[i];
                    if channels > 1 {
                        data[i * channels + 1] = buf_r[i];
                    }
                    // Any remaining channels (>2) are left as zero.
                }
            },
            |err| eprintln!("CoreAudio stream error: {}", err),
            None,
        ).map_err(|e| format!("Failed to build CoreAudio output stream: {}", e))?;

        stream.play().map_err(|e| format!("Failed to start CoreAudio stream: {}", e))?;
        println!("CoreAudio stream started.");
        Ok(CpalActive { _stream: stream })
    }
}

// ── Unsupported platforms ─────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Unsupported platform: only Linux (JACK) and macOS (CoreAudio) are supported.");

// ── Platform type aliases ─────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
type ProbeInner = jack_impl::JackProbe;
#[cfg(target_os = "macos")]
type ProbeInner = cpal_impl::CpalProbe;

#[cfg(target_os = "linux")]
type ActiveInner = jack_impl::JackActive;
#[cfg(target_os = "macos")]
type ActiveInner = cpal_impl::CpalActive;

// ── Public API ────────────────────────────────────────────────────────────────

/// Opened audio device, not yet processing audio.
/// Holds the negotiated sample rate so `SamplerState` can be built before
/// the audio callback starts.
pub struct AudioProbe {
    pub sample_rate: u32,
    inner: ProbeInner,
}

/// Live audio stream.  Dropping this stops audio output.
pub struct ActiveAudio {
    _inner: ActiveInner,
}

/// Open the platform audio device and negotiate the sample rate.
/// On Linux this opens a JACK client; on macOS it queries CoreAudio.
/// Pass `probed_rate` (from the sample files) so a mismatch warning can be
/// printed early, before `SamplerState` is constructed.
pub fn probe_audio(probed_rate: Option<u32>) -> Result<AudioProbe, String> {
    #[cfg(target_os = "linux")]
    let inner = jack_impl::probe(probed_rate)?;
    #[cfg(target_os = "macos")]
    let inner = cpal_impl::probe(probed_rate)?;

    let sample_rate = inner.sample_rate;
    Ok(AudioProbe { sample_rate, inner })
}

/// Start the audio stream.  The sampler's `process()` will be called from the
/// audio thread on every buffer fill.  Keep the returned `ActiveAudio` alive
/// for as long as audio should play.
pub fn start_audio(
    probe: AudioProbe,
    state: Arc<Mutex<SamplerState>>,
) -> Result<ActiveAudio, String> {
    #[cfg(target_os = "linux")]
    let inner = jack_impl::start(probe.inner, state)?;
    #[cfg(target_os = "macos")]
    let inner = cpal_impl::start(probe.inner, state)?;

    Ok(ActiveAudio { _inner: inner })
}
