use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

    pub fn type_label(&self) -> &'static str {
        match self {
            MenuItem::Instrument(i) => match i.path().extension().and_then(|e| e.to_str()) {
                Some("sfz")          => "SFZ",
                Some("organ")        => "ODF",
                Some("nki") | Some("nkm") => "NKI",
                Some("gig")          => "GIG",
                _                    => "   ",
            },
            MenuItem::MidiFile { .. } => "MID",
        }
    }
}

// ── Playback state ─────────────────────────────────────────────────────────────

pub struct PlaybackState {
    pub stop: Arc<AtomicBool>,
    pub done: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub current_us: Arc<AtomicU64>,
    pub total_us: u64,
    /// Write a µs position here to seek; midi_player uses u64::MAX as "no seek".
    pub seek_to: Arc<AtomicU64>,
    pub _handle: thread::JoinHandle<()>,
    pub menu_idx: usize,
}

/// Snapshot passed to print_menu / web snapshot (no Arc needed — just plain values).
#[derive(Clone, serde::Serialize)]
pub struct PlaybackInfo {
    pub current_us: u64,
    pub total_us: u64,
    pub paused: bool,
    pub menu_idx: usize,
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

// ── Runtime stats ─────────────────────────────────────────────────────────────

#[derive(Default, PartialEq, Clone, serde::Serialize)]
pub struct ProcStats {
    pub cpu_pct: u8,   // 0–100 %
    pub mem_mb: u32,   // resident set in MiB
    pub voices: usize, // active voice count
    pub peak_l: f32,   // smoothed peak, linear (0.0–1.0+)
    pub peak_r: f32,
    pub clip: bool,    // clip hold indicator (managed by main)
}

/// Render a coloured VU bar for `peak` (linear amplitude).
/// Returns a string: 20-char coloured bar + dB value.
fn vu_bar(peak: f32) -> String {
    const WIDTH: usize = 20;
    const DB_FLOOR: f32 = -48.0;
    const GREEN_END: usize  = 15;
    const YELLOW_END: usize = 19;

    let db = if peak > 1e-10 { 20.0 * peak.log10() } else { -96.0_f32 };
    let frac = ((db - DB_FLOOR) / (-DB_FLOOR)).clamp(0.0, 1.0);
    let filled = (frac * WIDTH as f32).round() as usize;
    let empty   = WIDTH - filled;

    let green_n  = filled.min(GREEN_END);
    let yellow_n = filled.saturating_sub(GREEN_END).min(YELLOW_END - GREEN_END);
    let red_n    = filled.saturating_sub(YELLOW_END);

    let green:  String = std::iter::repeat('█').take(green_n).collect();
    let yellow: String = std::iter::repeat('█').take(yellow_n).collect();
    let red:    String = std::iter::repeat('█').take(red_n).collect();
    let empty:  String = std::iter::repeat('░').take(empty).collect();

    let db_display = db.clamp(DB_FLOOR, 6.0);
    format!(
        "\x1b[32m{}\x1b[33m{}\x1b[31m{}\x1b[0m{} {:>+6.1}dB",
        green, yellow, red, empty, db_display,
    )
}

// ── Settings ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, serde::Serialize)]
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

#[derive(Clone, Copy, PartialEq, serde::Serialize)]
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
    /// Semitone offset relative to A440.
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

#[derive(Clone, serde::Serialize)]
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
    SelectAt(usize),                          // web: single click
    CycleVariant(i8),                         // ← / →
    CycleVariantAt { idx: usize, dir: i8 },  // web: per-row ◄/► buttons
    CycleVeltrack,                            // V
    CycleTune,                                // T
    ToggleRelease,                            // E
    VolumeChange(i8),                         // + / -
    TransposeChange(i8),                      // [ / ]
    ToggleResonance,                          // H
    ToggleRecord,                             // W
    PauseResume,                              // Space
    SeekRelative(i64),                        // , / .  (seconds)
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

// ── Seekbar ───────────────────────────────────────────────────────────────────

fn fmt_time(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn seekbar_line(info: &PlaybackInfo) -> String {
    const W: usize = 32;
    let frac = if info.total_us > 0 {
        (info.current_us as f64 / info.total_us as f64).clamp(0.0, 1.0)
    } else { 0.0 };
    let filled = (frac * W as f64).round() as usize;
    let bar: String = (0..W).map(|i| if i < filled { '═' } else { '░' }).collect();
    let icon   = if info.paused { "⏸" } else { "▶" };
    let action = if info.paused { "resume" } else { "pause " };
    format!(
        "  {} {} / {}  \x1b[36m[{}]\x1b[0m  Space:{} ,/.:±10s",
        icon,
        fmt_time(info.current_us),
        fmt_time(info.total_us),
        bar,
        action,
    )
}

// ── print_menu ────────────────────────────────────────────────────────────────

pub fn print_menu(
    menu: &[MenuItem],
    cursor: usize,
    loaded: usize,
    playing: Option<usize>,
    settings: &Settings,
    sustain: bool,
    stats: &ProcStats,
    recording: bool,
    rec_elapsed: Option<Duration>,
    pb_info: Option<&PlaybackInfo>,
) {
    print!("\x1b[2J\x1b[H");
    print!("Pianeer  \u{2191}/\u{2193} nav  Enter load  \u{2190}/\u{2192} variant  R rescan  Q quit\r\n");
    print!("  V vel  T tune  E release  +/- vol  [/] trns  H res  W rec\r\n");

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
    let clip_str = if stats.clip { "  \x1b[1;31m[CLIP]\x1b[0m" } else { "" };
    print!(
        "  CPU:{:3}%  Mem:{:4}MB  Voices:{}\r\n  L {}  R {}{}\r\n",
        stats.cpu_pct, stats.mem_mb, stats.voices,
        vu_bar(stats.peak_l), vu_bar(stats.peak_r), clip_str,
    );

    // Recording indicator
    if recording {
        let rec_time = rec_elapsed
            .map(|d| fmt_time(d.as_micros() as u64))
            .unwrap_or_else(|| "00:00".to_string());
        print!("  \x1b[1;31m● REC {}\x1b[0m  (W to stop)\r\n", rec_time);
    }

    // Seekbar (MIDI playback)
    if let Some(info) = pb_info {
        print!("{}\r\n", seekbar_line(info));
    }

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
        let is_paused  = pb_info.map(|p| p.menu_idx == i && p.paused).unwrap_or(false);

        let tag = if is_paused  { "[paused] " }
                  else if is_playing { "[playing]" }
                  else if is_loaded  { "[loaded] " }
                  else               { "         " };

        let type_label = item.type_label();
        let title = item.display_name();

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
            print!("  {}  {}  \x1b[7m{}{}\x1b[27m\r\n", tag, type_label, title, variant_suffix);
        } else {
            print!("  {}  {}  {}{}\r\n", tag, type_label, title, variant_suffix);
        }
    }

    std::io::stdout().flush().ok();
}
