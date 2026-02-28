//! Voice engine for the piano sampler.
//!
//! Designed to run inside the JACK process callback (real-time safe).
//! Uses try_lock() on the shared mutex to avoid blocking.
#![allow(dead_code)]
use crossbeam_channel::Receiver;

use crate::sfz::{Region, Trigger};

/// A loaded audio sample — stereo interleaved f32 PCM, 44100 Hz.
pub struct LoadedSample {
    /// Stereo interleaved frames: [L0, R0, L1, R1, ...]
    pub data: Vec<f32>,
    /// Number of stereo frames
    pub frames: usize,
}

/// MIDI events sent from the MIDI thread to the audio thread.
#[derive(Debug, Clone)]
pub enum MidiEvent {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    ControlChange { channel: u8, controller: u8, value: u8 },
    AllNotesOff,
}

const MAX_VOICES: usize = 128;
/// Amplitude below which a releasing voice is considered silent (linear).
/// Corresponds to -96 dB: 10^(-96/20) ≈ 1.585e-5
const SILENCE_AMPLITUDE: f32 = 1.585e-5;
/// Keep-alive tone amplitude (-40 dBFS).  A DC constant is blocked by the
/// output coupling capacitors and never reaches the codec's signal detector.
/// Instead we generate a 20 Hz sine — below audible perception at this level
/// but AC so it passes the caps and keeps the codec's mute circuit deactivated.
const KEEP_ALIVE_AMP: f32 = 0.01;

#[derive(Clone)]
enum EnvelopeState {
    Sustaining,
    Releasing {
        /// Current envelope amplitude multiplier (starts at 1.0, decays to 0).
        amplitude: f32,
        /// Multiplicative decay per sample for the release envelope.
        decay_per_sample: f32,
    },
}

#[derive(Clone)]
struct Voice {
    /// Index into the regions / loaded_samples arrays.
    region_idx: usize,
    /// Current playback position in stereo frames (sub-sample precision).
    position: f64,
    /// Playback rate: 2^((midi_note - pitch_keycenter) / 12) * pitch_keytrack_ratio
    playback_rate: f64,
    /// Linear amplitude after velocity mapping and volume gain.
    amplitude: f32,
    /// Envelope state
    envelope: EnvelopeState,
    /// The MIDI note that triggered this voice (for matching note-off).
    note: u8,
    /// Voice group number (for off_by exclusion).
    group: Option<u32>,
    /// Whether this voice kills voices of a given group when it starts.
    off_by: Option<u32>,
    /// Whether sustain pedal is holding this voice alive through note-off.
    pedal_held: bool,
    /// Whether a note-off was received while the pedal was down.
    pending_release: bool,
    /// Precomputed amplitude decay per sample for the release envelope (at actual JACK sample rate).
    decay_per_sample: f32,
    /// Whether this is a release-trigger voice (so pedal doesn't affect it).
    is_release_trigger: bool,
}

pub struct SamplerState {
    pub regions: Vec<Region>,
    pub samples: Vec<Option<LoadedSample>>,
    voices: Vec<Option<Voice>>,
    /// Current sustain pedal state (CC64 >= 64 = held).
    sustain_pedal: bool,
    sample_rate: f64,
    midi_rx: Receiver<MidiEvent>,
    /// Phase accumulator for the keep-alive 20 Hz sine (0..1).
    keep_alive_phase: f64,
}

impl SamplerState {
    pub fn new(
        regions: Vec<Region>,
        samples: Vec<Option<LoadedSample>>,
        sample_rate: f64,
        midi_rx: Receiver<MidiEvent>,
    ) -> Self {
        SamplerState {
            regions,
            samples,
            voices: (0..MAX_VOICES).map(|_| None).collect(),
            sustain_pedal: false,
            sample_rate,
            midi_rx,
            keep_alive_phase: 0.0,
        }
    }

    /// Process one JACK buffer. Write to out_l and out_r slices.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n_frames = out_l.len();

        // Zero output buffers.
        for i in 0..n_frames {
            out_l[i] = 0.0;
            out_r[i] = 0.0;
        }

        // Drain MIDI events from the channel (non-blocking).
        while let Ok(event) = self.midi_rx.try_recv() {
            self.handle_midi(event);
        }

        // Render voices.
        let mut i = 0;
        while i < MAX_VOICES {
            if let Some(ref mut voice) = self.voices[i] {
                let region_idx = voice.region_idx;
                if let Some(Some(ref sample)) = self.samples.get(region_idx) {
                    let data = &sample.data;
                    let total_frames = sample.frames;

                    for frame in 0..n_frames {
                        // Linear interpolation between adjacent stereo frames.
                        let pos_floor = voice.position as usize;
                        let frac = (voice.position - pos_floor as f64) as f32;

                        if pos_floor >= total_frames {
                            break;
                        }

                        let (l0, r0) = if pos_floor < total_frames {
                            (data[pos_floor * 2], data[pos_floor * 2 + 1])
                        } else {
                            (0.0f32, 0.0f32)
                        };
                        let (l1, r1) = if pos_floor + 1 < total_frames {
                            (data[(pos_floor + 1) * 2], data[(pos_floor + 1) * 2 + 1])
                        } else {
                            (l0, r0)
                        };

                        let l = l0 + (l1 - l0) * frac;
                        let r = r0 + (r1 - r0) * frac;

                        // Apply amplitude and envelope.
                        let env_amp = match &mut voice.envelope {
                            EnvelopeState::Sustaining => 1.0f32,
                            EnvelopeState::Releasing { ref mut amplitude, decay_per_sample } => {
                                let a = *amplitude;
                                *amplitude *= *decay_per_sample;
                                a
                            }
                        };

                        let gain = voice.amplitude * env_amp;
                        out_l[frame] += l * gain;
                        out_r[frame] += r * gain;

                        voice.position += voice.playback_rate;
                    }
                }
            }
            i += 1;
        }

        // Keep-alive: 20 Hz sine at -40 dBFS.
        // DC is blocked by output coupling capacitors and never reaches the
        // codec's signal detector. An AC tone passes the caps and keeps the
        // codec's auto-mute circuit deactivated between notes.
        let phase_step = 20.0 / self.sample_rate;
        for i in 0..n_frames {
            let ka = (self.keep_alive_phase * std::f64::consts::TAU).sin() as f32 * KEEP_ALIVE_AMP;
            out_l[i] += ka;
            out_r[i] += ka;
            self.keep_alive_phase += phase_step;
            if self.keep_alive_phase >= 1.0 {
                self.keep_alive_phase -= 1.0;
            }
        }

        // Retire finished voices.
        for slot in self.voices.iter_mut() {
            let done = if let Some(ref v) = slot {
                let pos_done = {
                    let region_idx = v.region_idx;
                    if let Some(Some(ref s)) = self.samples.get(region_idx) {
                        v.position as usize >= s.frames
                    } else {
                        true
                    }
                };
                let env_done = match &v.envelope {
                    EnvelopeState::Releasing { amplitude, .. } => *amplitude < SILENCE_AMPLITUDE,
                    _ => false,
                };
                pos_done || env_done
            } else {
                false
            };
            if done {
                *slot = None;
            }
        }
    }

    fn handle_midi(&mut self, event: MidiEvent) {
        match event {
            MidiEvent::NoteOn { note, velocity, .. } => {
                if velocity == 0 {
                    self.note_off(note);
                } else {
                    self.note_on(note, velocity);
                }
            }
            MidiEvent::NoteOff { note, .. } => {
                self.note_off(note);
            }
            MidiEvent::ControlChange { controller: 64, value, .. } => {
                self.set_sustain(value >= 64);
            }
            MidiEvent::ControlChange { .. } => {}
            MidiEvent::AllNotesOff => {
                self.all_notes_off();
            }
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        eprintln!("[SAMPLER] note_on note={} vel={} at {:.3}s", note, velocity, t.as_secs_f64());
        let rand_val: f32 = rand_f32();

        // Collect matching regions to spawn voices for.
        let mut to_spawn: Vec<(usize, u32)> = Vec::new(); // (region_idx, off_by_group)
        for (idx, region) in self.regions.iter().enumerate() {
            if region.trigger != Trigger::Attack {
                continue;
            }
            // Skip pedal-action regions (they have on_locc64/on_hicc64 and lokey=-1/hikey=-1 effectively)
            if region.on_locc64.is_some() || region.on_hicc64.is_some() {
                continue;
            }
            if note < region.lokey || note > region.hikey {
                continue;
            }
            if velocity < region.lovel || velocity > region.hivel {
                continue;
            }
            if rand_val < region.lorand || rand_val >= region.hirand {
                // When hirand==1.0 it's effectively unbounded
                if region.hirand < 1.0 || rand_val < region.lorand {
                    continue;
                }
            }
            to_spawn.push((idx, region.group.unwrap_or(0)));
        }

        for (region_idx, _) in &to_spawn {
            let region = &self.regions[*region_idx];
            // Kill voices in the off_by group.
            if let Some(off_by_grp) = region.off_by {
                self.kill_group(off_by_grp);
            }
            self.spawn_voice(*region_idx, note, velocity, false);
        }
    }

    fn note_off(&mut self, note: u8) {
        for slot in self.voices.iter_mut() {
            if let Some(ref mut v) = slot {
                if v.note == note && !v.is_release_trigger {
                    if self.sustain_pedal {
                        v.pending_release = true;
                    } else {
                        begin_release(v);
                    }
                }
            }
        }

        // Spawn release-trigger voices.
        let rand_val = rand_f32();
        let mut release_regions: Vec<usize> = Vec::new();
        for (idx, region) in self.regions.iter().enumerate() {
            if region.trigger != Trigger::Release {
                continue;
            }
            if region.on_locc64.is_some() || region.on_hicc64.is_some() {
                continue;
            }
            if note < region.lokey || note > region.hikey {
                continue;
            }
            if rand_val < region.lorand {
                continue;
            }
            if region.hirand < 1.0 && rand_val >= region.hirand {
                continue;
            }
            release_regions.push(idx);
        }
        for region_idx in release_regions {
            // Use a moderate velocity for release triggers (the held velocity is unknown here,
            // but rt_decay handles the level; use 64 as a neutral velocity).
            self.spawn_release_voice(region_idx, note, 64);
        }
    }

    fn set_sustain(&mut self, held: bool) {
        let was_held = self.sustain_pedal;
        self.sustain_pedal = held;

        if was_held && !held {
            // Pedal released: begin release on all pending voices.
            for slot in self.voices.iter_mut() {
                if let Some(ref mut v) = slot {
                    if v.pending_release && !v.is_release_trigger {
                        begin_release(v);
                    }
                }
            }

            // Trigger pedal-up action regions.
            let rand_val = rand_f32();
            let mut pedal_up_regions: Vec<usize> = Vec::new();
            for (idx, region) in self.regions.iter().enumerate() {
                if let (Some(lo), Some(hi)) = (region.on_locc64, region.on_hicc64) {
                    if lo == 0 && hi == 1 {
                        if rand_val >= region.lorand && (region.hirand >= 1.0 || rand_val < region.hirand) {
                            pedal_up_regions.push(idx);
                        }
                    }
                }
            }
            for idx in pedal_up_regions {
                self.spawn_voice(idx, 0, 100, false);
            }
        } else if !was_held && held {
            // Pedal pressed: trigger pedal-down action regions.
            let rand_val = rand_f32();
            let mut pedal_down_regions: Vec<usize> = Vec::new();
            // Also handle off_by for pedal-down group.
            let mut kill_groups: Vec<u32> = Vec::new();
            for (idx, region) in self.regions.iter().enumerate() {
                if let (Some(lo), Some(hi)) = (region.on_locc64, region.on_hicc64) {
                    if lo >= 64 && hi == 127 {
                        if rand_val >= region.lorand && (region.hirand >= 1.0 || rand_val < region.hirand) {
                            pedal_down_regions.push(idx);
                            if let Some(ob) = region.off_by {
                                kill_groups.push(ob);
                            }
                        }
                    }
                }
            }
            for grp in kill_groups {
                self.kill_group(grp);
            }
            for idx in pedal_down_regions {
                self.spawn_voice(idx, 0, 100, false);
            }
        }
    }

    fn all_notes_off(&mut self) {
        for slot in self.voices.iter_mut() {
            if let Some(ref mut v) = slot {
                begin_release(v);
            }
        }
    }

    fn kill_group(&mut self, group: u32) {
        for slot in self.voices.iter_mut() {
            if let Some(ref mut v) = slot {
                if v.group == Some(group) {
                    // Instant cut (short release).
                    v.envelope = EnvelopeState::Releasing {
                        amplitude: 1.0,
                        decay_per_sample: decay_per_sample(0.01, self.sample_rate),
                    };
                }
            }
        }
    }

    fn find_free_voice(&mut self) -> usize {
        // 1. Empty slot.
        for (i, slot) in self.voices.iter().enumerate() {
            if slot.is_none() {
                return i;
            }
        }
        // 2. Releasing release-trigger voice (lowest priority — steal first).
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if v.is_release_trigger && matches!(v.envelope, EnvelopeState::Releasing { .. }) {
                    return i;
                }
            }
        }
        // 3. Any releasing voice.
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if matches!(v.envelope, EnvelopeState::Releasing { .. }) {
                    return i;
                }
            }
        }
        // 4. Sustaining release-trigger voice.
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if v.is_release_trigger {
                    return i;
                }
            }
        }
        // 5. Last resort: slot 0.
        0
    }

    fn spawn_voice(&mut self, region_idx: usize, note: u8, velocity: u8, is_release_trigger: bool) {
        // Extract all needed data from the region before borrowing voices.
        let (playback_rate, amplitude, release_time, group, off_by) = {
            let region = &self.regions[region_idx];
            (
                compute_playback_rate(note, region),
                compute_amplitude(velocity, region),
                region.ampeg_release.max(0.001),
                region.group,
                region.off_by,
            )
        };

        let slot_idx = self.find_free_voice();
        self.voices[slot_idx] = Some(Voice {
            region_idx,
            position: 0.0,
            playback_rate,
            amplitude,
            envelope: EnvelopeState::Sustaining,
            note,
            group,
            off_by,
            pedal_held: false,
            pending_release: false,
            decay_per_sample: decay_per_sample(release_time as f64, self.sample_rate),
            is_release_trigger,
        });
    }

    fn spawn_release_voice(&mut self, region_idx: usize, note: u8, velocity: u8) {
        // Extract all needed data from the region before borrowing voices.
        let (playback_rate, amplitude, release_time, group, off_by) = {
            let region = &self.regions[region_idx];
            (
                compute_playback_rate(note, region),
                compute_amplitude(velocity, region),
                // Release triggers play and fade naturally; give them a short release.
                region.ampeg_release.max(0.1),
                region.group,
                region.off_by,
            )
        };

        let slot_idx = self.find_free_voice();
        self.voices[slot_idx] = Some(Voice {
            region_idx,
            position: 0.0,
            playback_rate,
            amplitude,
            envelope: EnvelopeState::Sustaining,
            note,
            group,
            off_by,
            pedal_held: false,
            pending_release: false,
            decay_per_sample: decay_per_sample(release_time as f64, self.sample_rate),
            is_release_trigger: true,
        });
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

fn begin_release(v: &mut Voice) {
    v.envelope = EnvelopeState::Releasing {
        amplitude: 1.0,
        decay_per_sample: v.decay_per_sample,
    };
}

fn decay_per_sample(release_time_s: f64, sample_rate: f64) -> f32 {
    // Time constant: amplitude drops to 1/e in release_time seconds.
    // exp(-1.0 / (release_time * sample_rate)) gives smooth exponential decay.
    // We target reaching SILENCE_AMPLITUDE (~-96dB) in release_time seconds.
    // That means: amplitude * decay^(release_time * sr) = SILENCE_AMPLITUDE
    // decay = SILENCE_AMPLITUDE^(1 / (release_time * sr))
    let n_samples = release_time_s * sample_rate;
    (SILENCE_AMPLITUDE as f64).powf(1.0 / n_samples) as f32
}

fn compute_playback_rate(note: u8, region: &Region) -> f64 {
    let semitones = (note as f64 - region.pitch_keycenter as f64)
        * (region.pitch_keytrack as f64 / 100.0);
    2.0_f64.powf(semitones / 12.0)
}

fn compute_amplitude(velocity: u8, region: &Region) -> f32 {
    let vel_norm = velocity as f32 / 127.0;
    let vel_gain = vel_norm.powf(region.amp_veltrack / 100.0);
    let db_gain = region.volume;
    let linear_gain = 10.0_f32.powf(db_gain / 20.0);
    vel_gain * linear_gain
}

/// Simple LCG-based pseudo-random f32 in [0, 1). Not cryptographic, but
/// safe for round-robin selection in a real-time context (no allocation).
fn rand_f32() -> f32 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(12345678901234567);
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x & 0xFFFFFF) as f32 / (0x1000000 as f32)
    })
}
