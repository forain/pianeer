use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

use crate::instruments;
use crate::loader::{LoadingJob, poll_loading_job, save_last_instrument_path};
use crate::midi_recorder::{self, RecordHandle};
use crate::midi_player;
use crate::sampler::{MidiEvent, SamplerState};
use crate::snapshot::build_snapshot;
use crate::sys_stats::StatsCollector;
use crate::types::{
    MenuAction, MenuItem, PlaybackState, ProcStats, Settings,
    pause_resume, playing_idx, seek_relative, start_midi_playback, stop_playback,
};

// ── Settings actions ──────────────────────────────────────────────────────────

/// Apply one of the 6 settings [`MenuAction`] variants to `settings` and `state`.
/// Returns `true` if the action was handled (it was a settings action).
pub fn apply_settings_action(
    action:    &MenuAction,
    settings:  &mut Settings,
    auto_tune: f32,
    state:     &Mutex<SamplerState>,
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

// ── Navigation actions ────────────────────────────────────────────────────────

/// Apply cursor movement or variant cycling from `action` to `cursor` / `menu`.
/// Returns `true` if the action was consumed.
pub fn apply_nav_action(
    action: &MenuAction,
    cursor: &mut usize,
    menu:   &mut Vec<MenuItem>,
) -> bool {
    match action {
        MenuAction::CursorUp => {
            if *cursor > 0 { *cursor -= 1; }
            true
        }
        MenuAction::CursorDown => {
            if *cursor + 1 < menu.len() { *cursor += 1; }
            true
        }
        MenuAction::CycleVariant(dir) => {
            if let Some(MenuItem::Instrument(inst)) = menu.get_mut(*cursor) {
                inst.cycle_variant(*dir);
            }
            true
        }
        MenuAction::CycleVariantAt { idx, dir } => {
            if let Some(MenuItem::Instrument(inst)) = menu.get_mut(*idx) {
                inst.cycle_variant(*dir);
            }
            true
        }
        _ => false,
    }
}

// ── Snapshot publishing ───────────────────────────────────────────────────────

/// Build a JSON snapshot and write it into `web_snapshot`.
pub fn publish_snapshot(
    web_snapshot: &Mutex<String>,
    menu:         &[MenuItem],
    cursor:       usize,
    current:      usize,
    playback:     &Option<PlaybackState>,
    settings:     &Settings,
    sustain:      bool,
    stats:        &ProcStats,
    record_handle: &RecordHandle,
    loading_info:  Option<(usize, u8)>,
) {
    let pb_info = playback.as_ref()
        .filter(|p| !p.done.load(Ordering::Relaxed))
        .map(|p| p.info());
    let rec_elapsed = midi_recorder::elapsed(record_handle);
    let rec_active  = midi_recorder::is_recording(record_handle);
    let snap = build_snapshot(
        menu, cursor, current, playing_idx(playback),
        settings, sustain, stats,
        rec_active,
        rec_elapsed.map(|d| d.as_micros() as u64),
        pb_info.as_ref(),
        loading_info,
    );
    if let Ok(json) = serde_json::to_string(&snap) {
        if let Ok(mut s) = web_snapshot.lock() { *s = json; }
    }
}

// ── Menu rebuilding ───────────────────────────────────────────────────────────

/// Rebuild the menu by re-discovering instruments and MIDI files.
/// Returns `(new_menu, new_current, new_cursor, new_current_path)`.
pub fn rebuild_menu(
    samples_dir: &Path,
    midi_dir:    Option<&Path>,
    old_menu:    &[MenuItem],
    current:     usize,
    cursor:      usize,
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

// ── Shared egui/DRM dispatch loop ─────────────────────────────────────────────

/// Background action-dispatch + snapshot loop used by both the native-UI and
/// DRM/KMS UI paths.  Runs until `quit` is set, then exits.
pub fn run_dispatch_loop(
    mut menu:            Vec<MenuItem>,
    state:               Arc<Mutex<SamplerState>>,
    midi_tx:             crossbeam_channel::Sender<MidiEvent>,
    record_handle:       RecordHandle,
    midi_dir:            Option<PathBuf>,
    samples_dir:         PathBuf,
    web_snapshot:        Arc<Mutex<String>>,
    action_rx:           crossbeam_channel::Receiver<MenuAction>,
    auto_tune_init:      f32,
    quit:                Arc<AtomicBool>,
    reload:              Arc<AtomicBool>,
    initial_loading_job: Option<(usize, LoadingJob)>,
) {
    let state_file = samples_dir.join(".last_instrument");
    let mut cursor = initial_loading_job.as_ref().map(|(idx, _)| *idx).unwrap_or(0);
    let mut current = 0usize;
    let mut current_path = if initial_loading_job.is_some() {
        PathBuf::new()
    } else {
        menu.iter()
            .find_map(|m| if let MenuItem::Instrument(i) = m { Some(i.path().clone()) } else { None })
            .unwrap_or_default()
    };
    let mut playback: Option<PlaybackState> = None;
    let mut settings = Settings::default();
    let mut auto_tune = auto_tune_init;
    let mut stats_collector = StatsCollector::new();
    let mut loading_job = initial_loading_job;

    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));

        if quit.load(Ordering::Relaxed) {
            stop_playback(&mut playback);
            break;
        }

        let (stats, sustain) = stats_collector.tick(&state);

        if let Some((new_idx, new_path, new_at)) =
            poll_loading_job(&mut loading_job, &state, &settings, &menu)
        {
            current = new_idx;
            save_last_instrument_path(&state_file, &new_path);
            current_path = new_path;
            auto_tune = new_at;
        }

        let loading_info = loading_job.as_ref().map(|(idx, job)| (*idx, job.pct()));
        publish_snapshot(
            &web_snapshot, &menu, cursor, current, &playback,
            &settings, sustain, &stats, &record_handle, loading_info,
        );

        if reload.swap(false, Ordering::SeqCst) {
            (menu, current, cursor, current_path) = rebuild_menu(
                &samples_dir, midi_dir.as_deref(), &menu, current, cursor,
            );
            continue;
        }

        if let Some(ref p) = playback {
            if p.done.load(Ordering::Relaxed) { playback = None; }
        }

        while let Ok(action) = action_rx.try_recv() {
            if apply_nav_action(&action, &mut cursor, &mut menu) { continue; }
            match action {
                MenuAction::ToggleRecord => {
                    let save_dir = midi_dir.as_deref().unwrap_or(Path::new("midi"));
                    match midi_recorder::stop_and_save(&record_handle, save_dir) {
                        None        => { midi_recorder::start(&record_handle); }
                        Some(Ok(p)) => {
                            eprintln!("Recording saved: {}", p.display());
                            reload.store(true, Ordering::SeqCst);
                        }
                        Some(Err(e)) => eprintln!("Failed to save recording: {e}"),
                    }
                }
                MenuAction::PauseResume => { pause_resume(&playback); }
                MenuAction::SeekRelative(secs) => { seek_relative(&playback, secs); }
                MenuAction::Select => {
                    dispatch_select(cursor, &menu, current, &current_path,
                                    &mut loading_job, &midi_tx, &mut playback);
                }
                MenuAction::SelectAt(target) => {
                    cursor = target.min(menu.len().saturating_sub(1));
                    dispatch_select(cursor, &menu, current, &current_path,
                                    &mut loading_job, &midi_tx, &mut playback);
                }
                MenuAction::ShowQr | MenuAction::Rescan => {
                    reload.store(true, Ordering::SeqCst);
                }
                _ => { apply_settings_action(&action, &mut settings, auto_tune, &state); }
            }
        }
    }
}

fn dispatch_select(
    idx:          usize,
    menu:         &[MenuItem],
    current:      usize,
    current_path: &Path,
    loading_job:  &mut Option<(usize, LoadingJob)>,
    midi_tx:      &crossbeam_channel::Sender<MidiEvent>,
    playback:     &mut Option<PlaybackState>,
) {
    if idx >= menu.len() { return; }
    match &menu[idx] {
        MenuItem::Instrument(inst) if idx != current || inst.path() != current_path => {
            *loading_job = Some((idx, LoadingJob::start(inst.clone())));
        }
        MenuItem::MidiFile { path, .. } => {
            start_midi_playback(idx, path.clone(), midi_tx, playback);
        }
        _ => {}
    }
}
