/// Shared region type used by both the SFZ and Grand Orgue ODF parsers.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    Attack,
    Release,
}

impl Default for Trigger {
    fn default() -> Self {
        Trigger::Attack
    }
}

/// One playable sample region, normalised from either SFZ or ODF source.
#[derive(Debug, Clone)]
pub struct Region {
    /// Absolute path to the sample file.
    pub sample: PathBuf,
    pub lokey: u8,
    pub hikey: u8,
    pub lovel: u8,
    pub hivel: u8,
    /// MIDI note the sample was recorded at (pitch centre for transposition).
    pub pitch_keycenter: u8,
    /// Velocity sensitivity as a percentage (0–100+). 0 = not velocity sensitive.
    pub amp_veltrack: f32,
    /// Release time in seconds for the amplitude envelope.
    pub ampeg_release: f32,
    /// Gain in dB. Default 0.
    pub volume: f32,
    pub trigger: Trigger,
    /// Release trigger decay in dB/s.
    pub rt_decay: f32,
    /// Random range low (0.0–1.0).
    pub lorand: f32,
    /// Random range high (0.0–1.0).
    pub hirand: f32,
    /// CC64 (sustain pedal) low value for trigger. None = not a pedal region.
    pub on_locc64: Option<u8>,
    /// CC64 (sustain pedal) high value for trigger.
    pub on_hicc64: Option<u8>,
    /// Voice group number (for exclusion).
    pub group: Option<u32>,
    /// Group that mutes this voice when it starts.
    pub off_by: Option<u32>,
    /// Pitch tracking in cents per semitone. Default 100.
    pub pitch_keytrack: f32,
    /// Pan position (-100 = full left, 0 = centre, 100 = full right).
    pub pan: f32,
    /// Pitch offset in cents (positive = sharper). Used for ODF HarmonicNumber tuning.
    pub tune_cents: f32,
    /// Loop start position in sample frames (None = no loop).
    pub loop_start: Option<u64>,
    /// Loop end position in sample frames, inclusive (None = no loop).
    pub loop_end: Option<u64>,
    /// Maximum simultaneous voices for the same MIDI note (None = unlimited).
    pub note_polyphony: Option<u32>,
    /// Maximum key-press duration (ms) for which this release sample is eligible.
    /// -1 = no limit (always eligible, used as long-hold fallback in organs).
    /// Matches Grand Orgue's PipeNNNReleaseNNNMaxKeyPressTime.
    pub max_key_press_ms: i32,

    // ── SFZ v2 / ARIA extensions ──────────────────────────────────────────────

    /// Keyswitch note that activates this region (sw_last). None = always active.
    pub sw_last: Option<u8>,
    /// CC range conditions for this region: (cc_num, lo, hi).
    /// Region only plays if all conditions hold against current CC values.
    pub cc_conds: Vec<(u8, u8, u8)>,
    /// Fixed sample start offset in frames.
    pub offset: u64,
    /// CC-controlled start offset: (cc_num, max_frames). Adds cc/127 * max to offset.
    pub offset_oncc: Option<(u8, u64)>,
    /// Voice fade-out time (seconds) when killed by off_by group exclusion.
    pub off_time: f32,
    /// CC-modulated amplitude additions: (cc_num, max_pct).
    /// effective_amplitude_pct = 100 + Σ(cc/127 * max_pct)
    pub amplitude_oncc: Vec<(u8, f32)>,
    /// CC-modulated pan additions using center-pan curve: (cc_num, max_delta).
    /// Contribution = (cc/64 − 1) * max_delta, so CC=64 → 0, CC=0 → −max, CC=127 → +max.
    pub pan_oncc: Vec<(u8, f32)>,
    /// CC-modulated amp_veltrack additions: (cc_num, max_mod).
    pub amp_veltrack_oncc: Vec<(u8, f32)>,
    /// CC-modulated ampeg_release additions: (cc_num, max_add_seconds).
    pub ampeg_release_oncc: Vec<(u8, f32)>,
}

impl Default for Region {
    fn default() -> Self {
        Region {
            sample: PathBuf::new(),
            lokey: 0,
            hikey: 127,
            lovel: 0,
            hivel: 127,
            pitch_keycenter: 60,
            amp_veltrack: 100.0,
            ampeg_release: 0.0,
            volume: 0.0,
            trigger: Trigger::Attack,
            rt_decay: 0.0,
            lorand: 0.0,
            hirand: 1.0,
            on_locc64: None,
            on_hicc64: None,
            group: None,
            off_by: None,
            pitch_keytrack: 100.0,
            pan: 0.0,
            tune_cents: 0.0,
            loop_start: None,
            loop_end: None,
            note_polyphony: None,
            max_key_press_ms: -1,
            sw_last: None,
            cc_conds: Vec::new(),
            offset: 0,
            offset_oncc: None,
            off_time: 0.01,
            amplitude_oncc: Vec::new(),
            pan_oncc: Vec::new(),
            amp_veltrack_oncc: Vec::new(),
            ampeg_release_oncc: Vec::new(),
        }
    }
}
