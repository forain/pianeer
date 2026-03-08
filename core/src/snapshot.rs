use serde::{Deserialize, Serialize};

use crate::types::{MenuItem, PlaybackInfo, ProcStats, Settings};

// ── Snapshot types ────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct WebMenuItem {
    pub name:       String,
    pub type_label: String,
    pub loaded:     bool,
    pub playing:    bool,
    pub cursor:     bool,
    pub variant:    Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WebSnapshot {
    pub menu:           Vec<WebMenuItem>,
    pub settings:       Settings,
    pub sustain:        bool,
    pub stats:          ProcStats,
    pub recording:      bool,
    pub rec_elapsed_us: Option<u64>,
    pub playback:       Option<PlaybackInfo>,
    pub loading:        Option<(usize, u8)>,  // (menu_idx, percent 0..=100)
}

impl Default for WebSnapshot {
    fn default() -> Self {
        WebSnapshot {
            menu:           Vec::new(),
            settings:       Settings::default(),
            sustain:        false,
            stats:          ProcStats::default(),
            recording:      false,
            rec_elapsed_us: None,
            playback:       None,
            loading:        None,
        }
    }
}

// ── Command from client ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum ClientCmd {
    CursorUp,
    CursorDown,
    Select,
    SelectAt { idx: usize },
    CycleVariant { dir: i8 },
    CycleVariantAt { idx: usize, dir: i8 },
    CycleVeltrack,
    CycleTune,
    ToggleRelease,
    VolumeChange { delta: i8 },
    TransposeChange { delta: i8 },
    ToggleResonance,
    ToggleRecord,
    PauseResume,
    SeekRelative { secs: i64 },
    Rescan,
}

// ── Snapshot builder ──────────────────────────────────────────────────────────

pub fn build_snapshot(
    menu:           &[MenuItem],
    cursor:         usize,
    loaded:         usize,
    playing:        Option<usize>,
    settings:       &Settings,
    sustain:        bool,
    stats:          &ProcStats,
    recording:      bool,
    rec_elapsed_us: Option<u64>,
    playback:       Option<&PlaybackInfo>,
    loading:        Option<(usize, u8)>,
) -> WebSnapshot {
    let items = menu.iter().enumerate().map(|(i, m)| {
        let variant = if let MenuItem::Instrument(inst) = m {
            inst.variant_name().map(|s| s.to_string())
        } else {
            None
        };
        WebMenuItem {
            name:       m.display_name().to_string(),
            type_label: m.type_label().to_string(),
            loaded:     matches!(m, MenuItem::Instrument(_)) && i == loaded,
            playing:    playing == Some(i),
            cursor:     i == cursor,
            variant,
        }
    }).collect();

    WebSnapshot {
        menu: items,
        settings: settings.clone(),
        sustain,
        stats: stats.clone(),
        recording,
        rec_elapsed_us,
        playback: playback.cloned(),
        loading,
    }
}
