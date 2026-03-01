use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::instruments::Instrument;

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
}

pub struct PlaybackState {
    pub stop: Arc<AtomicBool>,
    pub done: Arc<AtomicBool>,
    pub _handle: thread::JoinHandle<()>,
    pub menu_idx: usize,
}

// ── Settings ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
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
    /// Returns `None` for Normal (per-region), or `Some(override_pct)` for the others.
    pub fn override_pct(self) -> Option<f32> {
        match self {
            Self::Off    => Some(0.0),
            Self::Soft   => Some(50.0),
            Self::Normal => None,
            Self::Hard   => Some(150.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum TunePreset { A415, A440, A442, A444 }

impl TunePreset {
    pub fn cycle(self) -> Self {
        match self {
            Self::A415 => Self::A440,
            Self::A440 => Self::A442,
            Self::A442 => Self::A444,
            Self::A444 => Self::A415,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::A415 => "A415",
            Self::A440 => "A440",
            Self::A442 => "A442",
            Self::A444 => "A444",
        }
    }
    /// Semitone offset relative to A440.
    pub fn semitones(self) -> f32 {
        match self {
            Self::A415 => -1.0022,
            Self::A440 =>  0.0,
            Self::A442 =>  0.0785,
            Self::A444 =>  0.1576,
        }
    }
}

pub struct Settings {
    pub veltrack: VeltrackLevel,
    pub tune: TunePreset,
    pub release_enabled: bool,
    pub volume_db: f32,     // -12..+12 in ±3 dB steps; default 0
    pub transpose: i32,     // -12..+12 semitones; default 0
    pub resonance_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            veltrack: VeltrackLevel::Normal,
            tune: TunePreset::A440,
            release_enabled: true,
            volume_db: 0.0,
            transpose: 0,
            resonance_enabled: true,
        }
    }
}

// ── MenuAction ────────────────────────────────────────────────────────────────

pub enum MenuAction {
    CursorUp,
    CursorDown,
    Select,
    CycleVariant(i8),      // ← / →
    CycleVeltrack,         // V
    CycleTune,             // T
    ToggleRelease,         // E
    VolumeChange(i8),      // + / -
    TransposeChange(i8),   // [ / ]
    ToggleResonance,       // H
}

// ── Playback helpers ──────────────────────────────────────────────────────────

pub fn stop_playback(pb: &mut Option<PlaybackState>) {
    if let Some(p) = pb.take() {
        p.stop.store(true, Ordering::Relaxed);
        // Thread is detached; it sends AllNotesOff before exiting.
    }
}

pub fn playing_idx(pb: &Option<PlaybackState>) -> Option<usize> {
    pb.as_ref()
        .filter(|p| !p.done.load(Ordering::Relaxed))
        .map(|p| p.menu_idx)
}

// ── print_menu ────────────────────────────────────────────────────────────────

pub fn print_menu(
    menu: &[MenuItem],
    cursor: usize,
    loaded: usize,
    playing: Option<usize>,
    settings: &Settings,
    sustain: bool,
) {
    print!("\x1b[2J\x1b[H");
    print!("Pianeer  \u{2191}/\u{2193} nav  Enter load  \u{2190}/\u{2192} variant  R rescan  Q quit\r\n");
    print!("  V vel  T tune  E release  +/- vol  [/] trns  H res\r\n");

    // Settings status bar
    let sus_label = if sustain { "\x1b[1mSus:ON\x1b[0m" } else { "Sus:off" };
    print!(
        "  Vel:{}  Tune:{}  Rel:{}  Vol:{}dB  Trns:{}  Res:{}  {}\r\n",
        settings.veltrack.label(),
        settings.tune.label(),
        if settings.release_enabled { "on" } else { "off" },
        if settings.volume_db == 0.0 {
            "0".to_string()
        } else {
            format!("{:+.0}", settings.volume_db)
        },
        if settings.transpose == 0 {
            "0".to_string()
        } else {
            format!("{:+}", settings.transpose)
        },
        if settings.resonance_enabled { "on" } else { "off" },
        sus_label,
    );

    let has_inst = menu.iter().any(|m| matches!(m, MenuItem::Instrument(_)));
    let mut in_midi = false;

    if has_inst {
        print!("\r\nInstruments:\r\n");
    }

    for (i, item) in menu.iter().enumerate() {
        if !in_midi && matches!(item, MenuItem::MidiFile { .. }) {
            print!("\r\nMIDI files:\r\n");
            in_midi = true;
        }

        let is_loaded = matches!(item, MenuItem::Instrument(_)) && i == loaded;
        let is_playing = playing == Some(i);
        // Fixed-width tag column (9 chars) keeps titles aligned.
        let tag = if is_playing { "[playing]" } else if is_loaded { "[loaded] " } else { "         " };
        let title = item.display_name();

        // Append variant indicator for instruments with multiple variants.
        let variant_suffix = if let MenuItem::Instrument(inst) = item {
            if let Some(vname) = inst.variant_name() {
                format!(" [{} \u{25c4}\u{25ba}]", vname)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if i == cursor {
            print!("  {}  \x1b[7m{}{}\x1b[27m\r\n", tag, title, variant_suffix);
        } else {
            print!("  {}  {}{}\r\n", tag, title, variant_suffix);
        }
    }

    std::io::stdout().flush().ok();
}
