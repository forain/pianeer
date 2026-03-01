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
    /// Frame index when this voice was spawned (for rt_decay and voice stealing).
    started_at: u64,
    /// Equal-power pan gain for left channel.
    pan_l: f32,
    /// Equal-power pan gain for right channel.
    pan_r: f32,
    /// Fade-out time (seconds) when killed by group exclusion (off_time).
    off_time: f32,
}

pub struct SamplerState {
    pub regions: Vec<Region>,
    pub samples: Vec<Option<LoadedSample>>,
    voices: Vec<Option<Voice>>,
    sustain_pedal: bool,
    sample_rate: f64,
    midi_rx: Receiver<MidiEvent>,
    /// Cumulative frame counter, incremented at the start of each process() call.
    frame_count: u64,
    /// Current MIDI CC values (updated on every ControlChange event).
    cc_values: [u8; 128],
    /// Active keyswitch note (None = default / no keyswitch used yet).
    active_keyswitch: Option<u8>,
    /// Keyswitch range low (inclusive). When sw_lokey > sw_hikey, keyswitches are disabled.
    sw_lokey: u8,
    /// Keyswitch range high (inclusive).
    sw_hikey: u8,
}

impl SamplerState {
    pub fn new(
        regions: Vec<Region>,
        samples: Vec<Option<LoadedSample>>,
        sample_rate: f64,
        midi_rx: Receiver<MidiEvent>,
        cc_defaults: [u8; 128],
        sw_lokey: u8,
        sw_hikey: u8,
        sw_default: Option<u8>,
    ) -> Self {
        SamplerState {
            regions,
            samples,
            voices: (0..MAX_VOICES).map(|_| None).collect(),
            sustain_pedal: false,
            sample_rate,
            midi_rx,
            frame_count: 0,
            cc_values: cc_defaults,
            active_keyswitch: sw_default,
            sw_lokey,
            sw_hikey,
        }
    }

    /// Process one JACK buffer. Write to out_l and out_r slices.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n_frames = out_l.len();
        self.frame_count += n_frames as u64;

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

                        // Hermite cubic interpolation using 4 surrounding frames.
                        let (l_1, r_1) = sample_at(data, total_frames, pos_floor.saturating_sub(1));
                        let (l0, r0) = sample_at(data, total_frames, pos_floor);
                        let (l1, r1) = sample_at(data, total_frames, pos_floor + 1);
                        let (l2, r2) = sample_at(data, total_frames, pos_floor + 2);
                        let l = hermite(l_1, l0, l1, l2, frac);
                        let r = hermite(r_1, r0, r1, r2, frac);

                        let env_amp = match &mut voice.envelope {
                            EnvelopeState::Sustaining => 1.0f32,
                            EnvelopeState::Releasing { ref mut amplitude, decay_per_sample } => {
                                let a = *amplitude;
                                *amplitude *= *decay_per_sample;
                                a
                            }
                        };

                        let gain = voice.amplitude * env_amp;
                        out_l[frame] += l * gain * voice.pan_l;
                        out_r[frame] += r * gain * voice.pan_r;

                        voice.position += voice.playback_rate;
                    }
                }
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
            MidiEvent::ControlChange { controller, value, .. } => {
                self.cc_values[controller as usize] = value;
                if controller == 64 {
                    self.set_sustain(value >= 64);
                }
            }
            MidiEvent::AllNotesOff => {
                self.all_notes_off();
            }
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        // Keyswitch: if note is in the keyswitch range, update active keyswitch and return.
        if self.sw_lokey <= self.sw_hikey
            && note >= self.sw_lokey
            && note <= self.sw_hikey
        {
            self.active_keyswitch = Some(note);
            return;
        }

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
            // Keyswitch filter: region only active when its sw_last matches.
            if let Some(sw) = region.sw_last {
                if self.active_keyswitch != Some(sw) {
                    continue;
                }
            }
            // CC condition filter.
            if !cc_conds_met(&region.cc_conds, &self.cc_values) {
                continue;
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
        // Compute how long the note was held (for rt_decay on release voices).
        let held_frames = self.voices.iter()
            .filter_map(|s| s.as_ref())
            .filter(|v| v.note == note && !v.is_release_trigger
                && matches!(v.envelope, EnvelopeState::Sustaining))
            .map(|v| self.frame_count.saturating_sub(v.started_at))
            .max()
            .unwrap_or(0);
        let held_secs = held_frames as f32 / self.sample_rate as f32;

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
            if let Some(sw) = region.sw_last {
                if self.active_keyswitch != Some(sw) {
                    continue;
                }
            }
            if !cc_conds_met(&region.cc_conds, &self.cc_values) {
                continue;
            }
            release_regions.push(idx);
        }

        // If any candidate has a timed MaxKeyPressTime (>= 0), use organ-style
        // selection: pick the one with the smallest limit that is still >= held_ms,
        // falling back to the unlimited (-1) candidate.
        // Otherwise (all -1, e.g. SFZ) spawn all of them.
        let held_ms = (held_secs * 1000.0) as i32;
        let has_timed = release_regions.iter()
            .any(|&i| self.regions[i].max_key_press_ms >= 0);
        if has_timed {
            let best = release_regions.iter().copied()
                .filter(|&i| {
                    let ms = self.regions[i].max_key_press_ms;
                    ms >= 0 && ms >= held_ms
                })
                .min_by_key(|&i| self.regions[i].max_key_press_ms)
                .or_else(|| release_regions.iter().copied()
                    .find(|&i| self.regions[i].max_key_press_ms < 0));
            if let Some(region_idx) = best {
                self.spawn_release_voice(region_idx, note, 64, held_secs);
            }
        } else {
            for region_idx in release_regions {
                self.spawn_release_voice(region_idx, note, 64, held_secs);
            }
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
        let sr = self.sample_rate;
        for slot in self.voices.iter_mut() {
            if let Some(ref mut v) = slot {
                if v.group == Some(group) {
                    let fade = (v.off_time as f64).max(0.005);
                    v.envelope = EnvelopeState::Releasing {
                        amplitude: 1.0,
                        decay_per_sample: decay_per_sample(fade, sr),
                    };
                }
            }
        }
    }

    fn find_free_voice(&mut self) -> usize {
        let fc = self.frame_count;
        let mut best_idx = 0;
        let mut best_score = u64::MAX;

        for (i, slot) in self.voices.iter().enumerate() {
            if slot.is_none() {
                return i;
            }
            let v = slot.as_ref().unwrap();
            let age = fc.saturating_sub(v.started_at);
            let score: u64 = match (&v.envelope, v.is_release_trigger) {
                (EnvelopeState::Releasing { amplitude, .. }, true) =>
                    (*amplitude * 1000.0) as u64,
                (EnvelopeState::Releasing { amplitude, .. }, false) =>
                    1_000_000 + (*amplitude * 1000.0) as u64,
                (EnvelopeState::Sustaining, true) =>
                    2_000_000u64.saturating_sub(age),
                (EnvelopeState::Sustaining, false) =>
                    3_000_000u64.saturating_sub(age),
            };
            if score < best_score {
                best_score = score;
                best_idx = i;
            }
        }
        best_idx
    }

    fn spawn_voice(&mut self, region_idx: usize, note: u8, velocity: u8, is_release_trigger: bool) {
        let cc = &self.cc_values;
        let (playback_rate, amplitude, release_time, group, off_by, pan_l, pan_r, start_pos, off_time) = {
            let region = &self.regions[region_idx];
            let eff_veltrack = effective_veltrack(region, cc);
            let eff_amp = compute_amplitude(velocity, region, eff_veltrack)
                * amplitude_multiplier(region, cc);
            let eff_pan = effective_pan(region, cc);
            let eff_release = effective_release(region, cc).max(0.001);
            let start = effective_offset(region, cc);
            let angle = std::f32::consts::FRAC_PI_4 * (eff_pan / 100.0 + 1.0);
            (
                compute_playback_rate(note, region),
                eff_amp,
                eff_release,
                region.group,
                region.off_by,
                angle.cos(),
                angle.sin(),
                start as f64,
                region.off_time,
            )
        };

        let slot_idx = self.find_free_voice();
        self.voices[slot_idx] = Some(Voice {
            region_idx,
            position: start_pos,
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
            started_at: self.frame_count,
            pan_l,
            pan_r,
            off_time,
        });
    }

    fn spawn_release_voice(&mut self, region_idx: usize, note: u8, velocity: u8, held_secs: f32) {
        let cc = &self.cc_values;
        let (playback_rate, mut amplitude, release_time, group, off_by, rt_decay, pan_l, pan_r, off_time) = {
            let region = &self.regions[region_idx];
            let eff_veltrack = effective_veltrack(region, cc);
            let eff_amp = compute_amplitude(velocity, region, eff_veltrack)
                * amplitude_multiplier(region, cc);
            let eff_pan = effective_pan(region, cc);
            let eff_release = effective_release(region, cc).max(0.1);
            let angle = std::f32::consts::FRAC_PI_4 * (eff_pan / 100.0 + 1.0);
            (
                compute_playback_rate(note, region),
                eff_amp,
                eff_release,
                region.group,
                region.off_by,
                region.rt_decay,
                angle.cos(),
                angle.sin(),
                region.off_time,
            )
        };

        if rt_decay > 0.0 {
            amplitude *= 10.0_f32.powf(-rt_decay * held_secs / 20.0);
        }

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
            started_at: self.frame_count,
            pan_l,
            pan_r,
            off_time,
        });
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Replace the loaded instrument without restarting JACK.
    /// Clears all active voices and resets the sustain pedal and CC/keyswitch state.
    pub fn swap_instrument(
        &mut self,
        regions: Vec<Region>,
        samples: Vec<Option<LoadedSample>>,
        cc_defaults: [u8; 128],
        sw_lokey: u8,
        sw_hikey: u8,
        sw_default: Option<u8>,
    ) {
        for slot in self.voices.iter_mut() {
            *slot = None;
        }
        self.regions = regions;
        self.samples = samples;
        self.sustain_pedal = false;
        self.cc_values = cc_defaults;
        self.sw_lokey = sw_lokey;
        self.sw_hikey = sw_hikey;
        self.active_keyswitch = sw_default;
    }
}

// ── CC condition check ────────────────────────────────────────────────────────

#[inline]
fn cc_conds_met(conds: &[(u8, u8, u8)], cc: &[u8; 128]) -> bool {
    conds.iter().all(|&(cc_num, lo, hi)| {
        let v = cc[cc_num as usize];
        v >= lo && v <= hi
    })
}

// ── CC-modulated parameter helpers ───────────────────────────────────────────

/// Effective amplitude multiplier: 1.0 + Σ(cc/127 * max_pct/100).
#[inline]
fn amplitude_multiplier(region: &Region, cc: &[u8; 128]) -> f32 {
    if region.amplitude_oncc.is_empty() {
        return 1.0;
    }
    let mut pct = 100.0f32;
    for &(cc_num, max_pct) in &region.amplitude_oncc {
        pct += cc[cc_num as usize] as f32 / 127.0 * max_pct;
    }
    (pct / 100.0).max(0.0)
}

/// Effective pan with CC contributions using center-pan curve (CC=64 → no change).
#[inline]
fn effective_pan(region: &Region, cc: &[u8; 128]) -> f32 {
    let mut pan = region.pan;
    for &(cc_num, max_delta) in &region.pan_oncc {
        pan += (cc[cc_num as usize] as f32 / 64.0 - 1.0) * max_delta;
    }
    pan.clamp(-100.0, 100.0)
}

/// Effective amp_veltrack with CC contributions.
#[inline]
fn effective_veltrack(region: &Region, cc: &[u8; 128]) -> f32 {
    let mut vt = region.amp_veltrack;
    for &(cc_num, max_mod) in &region.amp_veltrack_oncc {
        vt += cc[cc_num as usize] as f32 / 127.0 * max_mod;
    }
    vt.clamp(0.0, 200.0)
}

/// Effective ampeg_release with CC additions.
#[inline]
fn effective_release(region: &Region, cc: &[u8; 128]) -> f32 {
    let mut rel = region.ampeg_release;
    for &(cc_num, max_add) in &region.ampeg_release_oncc {
        rel += cc[cc_num as usize] as f32 / 127.0 * max_add;
    }
    rel
}

/// Effective sample start offset: fixed offset + CC-controlled addition.
#[inline]
fn effective_offset(region: &Region, cc: &[u8; 128]) -> u64 {
    let cc_add = region.offset_oncc.map_or(0u64, |(cc_num, max_off)| {
        (cc[cc_num as usize] as u64 * max_off) / 127
    });
    region.offset + cc_add
}

// ── Audio helpers ─────────────────────────────────────────────────────────────

/// Catmull-Rom / Hermite cubic interpolation between p1 and p2.
/// p0 and p3 are the surrounding control points; t in [0, 1).
#[inline]
fn hermite(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b =        p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0             + 0.5 * p2;
    ((a * t + b) * t + c) * t + p1
}

/// Read one stereo frame from interleaved PCM data, clamping to the last valid frame.
#[inline]
fn sample_at(data: &[f32], frames: usize, i: usize) -> (f32, f32) {
    let i = i.min(frames.saturating_sub(1));
    (data[i * 2], data[i * 2 + 1])
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
        * (region.pitch_keytrack as f64 / 100.0)
        + region.tune_cents as f64 / 100.0;
    2.0_f64.powf(semitones / 12.0)
}

fn compute_amplitude(velocity: u8, region: &Region, veltrack: f32) -> f32 {
    let vel_norm = velocity as f32 / 127.0;
    let vel_gain = vel_norm.powf(veltrack / 100.0);
    let linear_gain = 10.0_f32.powf(region.volume / 20.0);
    vel_gain * linear_gain
}

/// Simple xorshift-based pseudo-random f32 in [0, 1). Not cryptographic, but
/// safe for round-robin selection in a real-time context (no allocation).
/// Seeded from the system clock on first call.
fn rand_f32() -> f32 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
                | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x & 0xFFFFFF) as f32 / (0x1000000 as f32)
    })
}
