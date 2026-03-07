use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::instruments;
use crate::midi_player;
use crate::sampler::SamplerState;
use crate::types::{MenuAction, MenuItem, Settings};

/// Apply one of the 6 settings `MenuAction` variants to `settings` and `state`.
/// Returns `true` if the action was handled (it was a settings action).
pub fn apply_settings_action(
    action: &MenuAction,
    settings: &mut Settings,
    auto_tune: f32,
    state: &Mutex<SamplerState>,
) -> bool {
    match action {
        MenuAction::CycleVeltrack => {
            settings.veltrack = settings.veltrack.cycle();
            if let Ok(ref mut s) = state.lock() {
                s.veltrack_override = settings.veltrack.override_pct();
            }
        }
        MenuAction::CycleTune => {
            settings.tune = settings.tune.cycle();
            if let Ok(ref mut s) = state.lock() {
                s.master_tune_semitones = auto_tune + settings.tune.semitones();
            }
        }
        MenuAction::ToggleRelease => {
            settings.release_enabled = !settings.release_enabled;
            if let Ok(ref mut s) = state.lock() {
                s.release_enabled = settings.release_enabled;
            }
        }
        MenuAction::VolumeChange(delta) => {
            settings.volume_db = (settings.volume_db + *delta as f32 * 3.0).clamp(-12.0, 12.0);
            if let Ok(ref mut s) = state.lock() {
                s.master_volume = 10.0_f32.powf(settings.volume_db / 20.0);
            }
        }
        MenuAction::TransposeChange(delta) => {
            settings.transpose = (settings.transpose + *delta as i32).clamp(-12, 12);
            if let Ok(ref mut s) = state.lock() {
                s.transpose = settings.transpose;
            }
        }
        MenuAction::ToggleResonance => {
            settings.resonance_enabled = !settings.resonance_enabled;
            if let Ok(ref mut s) = state.lock() {
                s.resonance_enabled = settings.resonance_enabled;
            }
        }
        _ => return false,
    }
    true
}

/// Rebuild the menu by re-discovering instruments and MIDI files.
/// Returns `(new_menu, new_current, new_cursor, new_current_path)`.
pub fn rebuild_menu(
    samples_dir: &Path,
    midi_dir: Option<&Path>,
    old_menu: &[MenuItem],
    current: usize,
    cursor: usize,
) -> (Vec<MenuItem>, usize, usize, PathBuf) {
    let saved_path = match old_menu.get(current) {
        Some(MenuItem::Instrument(i)) => Some(i.path().clone()),
        _ => None,
    };
    let new_instr = instruments::discover(samples_dir);
    let new_midi  = midi_dir.map(|d| midi_player::discover(d)).unwrap_or_default();
    let mut new_menu: Vec<MenuItem> =
        new_instr.into_iter().map(MenuItem::Instrument).collect();
    for (name, path) in new_midi {
        new_menu.push(MenuItem::MidiFile { name, path });
    }
    let new_current = saved_path
        .and_then(|p| new_menu.iter().position(|m|
            matches!(m, MenuItem::Instrument(i) if i.path() == &p)))
        .unwrap_or(0);
    let new_cursor = cursor.min(new_menu.len().saturating_sub(1));
    let new_current_path = new_menu.get(new_current)
        .and_then(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
        .unwrap_or_default();
    (new_menu, new_current, new_cursor, new_current_path)
}
