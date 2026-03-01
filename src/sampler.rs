//! Voice engine for the piano sampler.
//!
//! Designed to run inside the JACK process callback (real-time safe).
//! Uses try_lock() on the shared mutex to avoid blocking.
#![allow(dead_code)]
use crossbeam_channel::Receiver;

use crate::region::{Region, Trigger};

/// A loaded audio sample — stereo interleaved f32 PCM.
pub struct LoadedSample {
    /// Stereo interleaved frames: [L0, R0, L1, R1, …]
    pub data: Vec<f32>,
    /// Number of stereo frames.
    pub frames: usize,
    /// Loop start position in frames (None = no loop).
    pub loop_start: Option<usize>,
    /// Loop end position in frames, inclusive (None = no loop).
    pub loop_end: Option<usize>,
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
/// Keep-alive tone amplitude (-40 dBFS). A 20 Hz AC sine passes output
/// coupling capacitors and prevents codec auto-mute between notes.
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
    region_idx: usize,
    /// Current playback position in stereo frames (sub-sample precision).
    position: f64,
    /// Playback rate: 2^((midi_note − pitch_keycenter) / 12) × pitch_keytrack_ratio
    playback_rate: f64,
    /// Linear amplitude after velocity mapping and volume gain.
    amplitude: f32,
    envelope: EnvelopeState,
    /// The MIDI note that triggered this voice (for matching note-off).
    note: u8,
    group: Option<u32>,
    off_by: Option<u32>,
    pedal_held: bool,
    pending_release: bool,
    /// Precomputed release decay per sample.
    decay_per_sample: f32,
    is_release_trigger: bool,
}

pub struct SamplerState {
    pub regions: Vec<Region>,
    pub samples: Vec<Option<LoadedSample>>,
    voices: Vec<Option<Voice>>,
    sustain_pedal: bool,
    sample_rate: f64,
    midi_rx: Receiver<MidiEvent>,
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

        for i in 0..n_frames {
            out_l[i] = 0.0;
            out_r[i] = 0.0;
        }

        // Drain MIDI events from the channel (non-blocking).
        while let Ok(event) = self.midi_rx.try_recv() {
            self.handle_midi(event);
        }

        // Render voices.
        for i in 0..MAX_VOICES {
            if let Some(ref mut voice) = self.voices[i] {
                let region_idx = voice.region_idx;
                if let Some(Some(ref sample)) = self.samples.get(region_idx) {
                    let data = &sample.data;
                    let total_frames = sample.frames;
                    let loop_start = sample.loop_start;
                    let loop_end = sample.loop_end;

                    for frame in 0..n_frames {
                        // Apply looping: only while the voice is sustaining.
                        if matches!(voice.envelope, EnvelopeState::Sustaining) {
                            if let (Some(ls), Some(le)) = (loop_start, loop_end) {
                                if le > ls && voice.position > le as f64 {
                                    let loop_len = (le - ls) as f64;
                                    voice.position = ls as f64
                                        + (voice.position - ls as f64).rem_euclid(loop_len);
                                }
                            }
                        }

                        let pos_floor = voice.position as usize;
                        let frac = (voice.position - pos_floor as f64) as f32;

                        if pos_floor >= total_frames {
                            break;
                        }

                        let (l0, r0) = (data[pos_floor * 2], data[pos_floor * 2 + 1]);
                        let (l1, r1) = if pos_floor + 1 < total_frames {
                            (data[(pos_floor + 1) * 2], data[(pos_floor + 1) * 2 + 1])
                        } else {
                            (l0, r0)
                        };

                        let l = l0 + (l1 - l0) * frac;
                        let r = r0 + (r1 - r0) * frac;

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
        }

        // Keep-alive: 20 Hz sine at -40 dBFS.
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
                    // A looping voice in Sustaining state never ends by position.
                    let has_loop = if let Some(Some(ref s)) = self.samples.get(region_idx) {
                        s.loop_start.is_some() && s.loop_end.is_some()
                    } else {
                        false
                    };
                    let looping = has_loop && matches!(v.envelope, EnvelopeState::Sustaining);
                    if looping {
                        false
                    } else if let Some(Some(ref s)) = self.samples.get(region_idx) {
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
        let rand_val: f32 = rand_f32();

        let mut to_spawn: Vec<(usize, u32)> = Vec::new();
        for (idx, region) in self.regions.iter().enumerate() {
            if region.trigger != Trigger::Attack {
                continue;
            }
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
                if region.hirand < 1.0 || rand_val < region.lorand {
                    continue;
                }
            }
            to_spawn.push((idx, region.group.unwrap_or(0)));
        }

        for (region_idx, _) in &to_spawn {
            let (off_by_grp, max_poly) = {
                let r = &self.regions[*region_idx];
                (r.off_by, r.note_polyphony)
            };
            if let Some(g) = off_by_grp {
                self.kill_group(g);
            }
            if let Some(p) = max_poly {
                self.enforce_note_polyphony(note, p as usize);
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
            self.spawn_release_voice(region_idx, note, 64);
        }
    }

    fn set_sustain(&mut self, held: bool) {
        let was_held = self.sustain_pedal;
        self.sustain_pedal = held;

        if was_held && !held {
            for slot in self.voices.iter_mut() {
                if let Some(ref mut v) = slot {
                    if v.pending_release && !v.is_release_trigger {
                        begin_release(v);
                    }
                }
            }

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
            let rand_val = rand_f32();
            let mut pedal_down_regions: Vec<usize> = Vec::new();
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

    fn enforce_note_polyphony(&mut self, note: u8, max_poly: usize) {
        let fast_decay = decay_per_sample(0.01, self.sample_rate);
        let mut count = self.voices.iter()
            .filter(|s| s.as_ref().map_or(false, |v| v.note == note && !v.is_release_trigger))
            .count();
        for slot in self.voices.iter_mut() {
            if count < max_poly { break; }
            if let Some(ref mut v) = slot {
                if v.note == note && !v.is_release_trigger {
                    v.envelope = EnvelopeState::Releasing {
                        amplitude: 1.0,
                        decay_per_sample: fast_decay,
                    };
                    count -= 1;
                }
            }
        }
    }

    fn kill_group(&mut self, group: u32) {
        for slot in self.voices.iter_mut() {
            if let Some(ref mut v) = slot {
                if v.group == Some(group) {
                    v.envelope = EnvelopeState::Releasing {
                        amplitude: 1.0,
                        decay_per_sample: decay_per_sample(0.01, self.sample_rate),
                    };
                }
            }
        }
    }

    fn find_free_voice(&mut self) -> usize {
        for (i, slot) in self.voices.iter().enumerate() {
            if slot.is_none() { return i; }
        }
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if v.is_release_trigger && matches!(v.envelope, EnvelopeState::Releasing { .. }) {
                    return i;
                }
            }
        }
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if matches!(v.envelope, EnvelopeState::Releasing { .. }) { return i; }
            }
        }
        for (i, slot) in self.voices.iter().enumerate() {
            if let Some(ref v) = slot {
                if v.is_release_trigger { return i; }
            }
        }
        0
    }

    fn spawn_voice(&mut self, region_idx: usize, note: u8, velocity: u8, is_release_trigger: bool) {
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
        let (playback_rate, amplitude, release_time, group, off_by) = {
            let region = &self.regions[region_idx];
            (
                compute_playback_rate(note, region),
                compute_amplitude(velocity, region),
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

    /// Replace the loaded instrument without restarting JACK.
    /// Clears all active voices and resets the sustain pedal.
    pub fn swap_instrument(&mut self, regions: Vec<Region>, samples: Vec<Option<LoadedSample>>) {
        for slot in self.voices.iter_mut() {
            *slot = None;
        }
        self.regions = regions;
        self.samples = samples;
        self.sustain_pedal = false;
    }
}

fn begin_release(v: &mut Voice) {
    v.envelope = EnvelopeState::Releasing {
        amplitude: 1.0,
        decay_per_sample: v.decay_per_sample,
    };
}

fn decay_per_sample(release_time_s: f64, sample_rate: f64) -> f32 {
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
    let linear_gain = 10.0_f32.powf(region.volume / 20.0);
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
