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
    /// Loop start position in sample frames (None = no loop).
    pub loop_start: Option<u64>,
    /// Loop end position in sample frames, inclusive (None = no loop).
    pub loop_end: Option<u64>,
    /// Maximum simultaneous voices for the same MIDI note (None = unlimited).
    pub note_polyphony: Option<u32>,
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
            loop_start: None,
            loop_end: None,
            note_polyphony: None,
        }
    }
}
