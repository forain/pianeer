use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use crate::instruments::Instrument;

// ── MenuItem ──────────────────────────────────────────────────────────────────

pub enum MenuItem {
    Instrument(Instrument),
    MidiFile { name: String, path: PathBuf },
}

impl MenuItem {
    pub fn display_name(&self) -> &str {
        match self {
            MenuItem::Instrument(i) => &i.name,
            MenuItem::MidiFile { name, .. } => name,
        }
    }

    pub fn type_label(&self) -> &'static str {
        match self {
            MenuItem::Instrument(i) => match i.path().extension().and_then(|e| e.to_str()) {
                Some("sfz")               => "SFZ",
                Some("organ")             => "ODF",
                Some("nki") | Some("nkm") => "NKI",
                Some("gig")               => "GIG",
                _                         => "   ",
            },
            MenuItem::MidiFile { .. } => "MID",
        }
    }
}

// ── PlaybackState ─────────────────────────────────────────────────────────────

/// Runtime playback state — owns the JoinHandle and shared atomics.
pub struct PlaybackState {
    pub stop:       Arc<AtomicBool>,
    pub done:       Arc<AtomicBool>,
    pub paused:     Arc<AtomicBool>,
    pub current_us: Arc<AtomicU64>,
    pub total_us:   u64,
    pub seek_to:    Arc<AtomicU64>,
    pub _handle:    thread::JoinHandle<()>,
    pub menu_idx:   usize,
}

impl PlaybackState {
    pub fn info(&self) -> PlaybackInfo {
        PlaybackInfo {
            current_us: self.current_us.load(Ordering::Relaxed),
            total_us:   self.total_us,
            paused:     self.paused.load(Ordering::Relaxed),
            menu_idx:   self.menu_idx,
        }
    }
}

pub fn stop_playback(pb: &mut Option<PlaybackState>) {
    if let Some(p) = pb.take() {
        p.stop.store(true, Ordering::Relaxed);
    }
}

pub fn playing_idx(pb: &Option<PlaybackState>) -> Option<usize> {
    pb.as_ref()
        .filter(|p| !p.done.load(Ordering::Relaxed))
        .map(|p| p.menu_idx)
}

// ── PlaybackInfo ──────────────────────────────────────────────────────────────

/// Plain-value snapshot of playback position (no Arc/JoinHandle — safe to clone).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PlaybackInfo {
    pub current_us: u64,
    pub total_us:   u64,
    pub paused:     bool,
    pub menu_idx:   usize,
}

// ── ProcStats ─────────────────────────────────────────────────────────────────

#[derive(Default, PartialEq, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcStats {
    pub cpu_pct: u8,
    pub mem_mb:  u32,
    pub voices:  usize,
    pub peak_l:  f32,
    pub peak_r:  f32,
    pub clip:    bool,
}

// ── Settings ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum VeltrackLevel { Off, Soft, Normal, Hard }

impl VeltrackLevel {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off    => Self::Soft,
            Self::Soft   => Self::Normal,
            Self::Normal => Self::Hard,
            Self::Hard   => Self::Off,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Off    => "Off",
            Self::Soft   => "Soft",
            Self::Normal => "Normal",
            Self::Hard   => "Hard",
        }
    }
    pub fn override_pct(self) -> Option<f32> {
        match self {
            Self::Off    => Some(0.0),
            Self::Soft   => Some(50.0),
            Self::Normal => None,
            Self::Hard   => Some(150.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TunePreset { A415, A432, A440, A442, A444 }

impl TunePreset {
    pub fn cycle(self) -> Self {
        match self {
            Self::A415 => Self::A432,
            Self::A432 => Self::A440,
            Self::A440 => Self::A442,
            Self::A442 => Self::A444,
            Self::A444 => Self::A415,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::A415 => "A415",
            Self::A432 => "A432",
            Self::A440 => "A440",
            Self::A442 => "A442",
            Self::A444 => "A444",
        }
    }
    pub fn semitones(self) -> f32 {
        match self {
            Self::A415 => -1.0022,
            Self::A432 => -0.3177,
            Self::A440 =>  0.0,
            Self::A442 =>  0.0785,
            Self::A444 =>  0.1576,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub veltrack:          VeltrackLevel,
    pub tune:              TunePreset,
    pub release_enabled:   bool,
    pub volume_db:         f32,
    pub transpose:         i32,
    pub resonance_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            veltrack:          VeltrackLevel::Normal,
            tune:              TunePreset::A440,
            release_enabled:   true,
            volume_db:         -12.0,
            transpose:         0,
            resonance_enabled: true,
        }
    }
}

// ── MenuAction ────────────────────────────────────────────────────────────────

pub enum MenuAction {
    CursorUp,
    CursorDown,
    Select,
    SelectAt(usize),
    CycleVariant(i8),
    CycleVariantAt { idx: usize, dir: i8 },
    CycleVeltrack,
    CycleTune,
    ToggleRelease,
    VolumeChange(i8),
    TransposeChange(i8),
    ToggleResonance,
    ToggleRecord,
    PauseResume,
    SeekRelative(i64),
    ShowQr,
    Rescan,
}
