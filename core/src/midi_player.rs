//! MIDI file discovery and playback.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::sampler::MidiEvent;

/// Sentinel: no seek pending.
const NO_SEEK: u64 = u64::MAX;

// ─── Discovery ────────────────────────────────────────────────────────────────

/// Find the `midi/` directory next to `samples/` (same search locations).
pub fn find_midi_dir() -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        let c = cwd.join("midi");
        if c.is_dir() { return Some(c); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("midi");
            if c.is_dir() { return Some(c); }
            let c = dir.join("../../midi");
            if c.is_dir() { return Some(c.canonicalize().unwrap_or(c)); }
        }
    }
    None
}

/// Return `(display_name, path)` for every `.mid`/`.midi` file in `dir`,
/// sorted alphabetically.
pub fn discover(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("mid") | Some("midi") => {}
                _ => continue,
            }
            let name = path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            files.push((name, path));
        }
    }
    files
}

// ─── Playback ─────────────────────────────────────────────────────────────────

/// Spawn a playback thread.
///
/// Returns `(handle, total_duration_us)`.  The MIDI file is pre-parsed on the
/// calling thread so `total_duration_us` is available immediately.
///
/// * `stop`       – set to abort; `AllNotesOff` is sent on exit.
/// * `done`       – set when the thread exits.
/// * `paused`     – set to pause, clear to resume.
/// * `current_us` – updated ~every 10 ms with the playback position.
/// * `seek_to`    – write a position in µs to seek; use NO_SEEK sentinel otherwise.
pub fn spawn(
    path: PathBuf,
    midi_tx: Sender<MidiEvent>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    current_us: Arc<AtomicU64>,
    seek_to: Arc<AtomicU64>,
) -> (JoinHandle<()>, u64) {
    let (timed, total_us) = match parse_file(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("MIDI parse error: {}", e);
            done.store(true, Ordering::Relaxed);
            return (thread::spawn(|| {}), 0);
        }
    };

    let handle = thread::spawn(move || {
        play_events(timed, &midi_tx, &stop, &paused, &current_us, &seek_to);
        let _ = midi_tx.send(MidiEvent::AllNotesOff);
        done.store(true, Ordering::Relaxed);
    });

    (handle, total_us)
}

// ─── Internal ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum Ev {
    NoteOn  { channel: u8, key: u8, vel: u8 },
    NoteOff { channel: u8, key: u8 },
    Cc      { channel: u8, cc: u8, val: u8 },
    Tempo(u32),
}

/// Parse the MIDI file and return `(timed_events, total_duration_us)`.
fn parse_file(path: &Path) -> Result<(Vec<(u64, Ev)>, u64), String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("read {:?}: {}", path, e))?;
    let smf = midly::Smf::parse(&data)
        .map_err(|e| format!("parse {:?}: {}", path, e))?;

    let ticks_per_beat: u64 = match smf.header.timing {
        midly::Timing::Metrical(n) => n.as_int() as u64,
        midly::Timing::Timecode(_, _) => return Err("SMTPE timing not supported".into()),
    };

    // Collect all events from all tracks with absolute tick positions.
    let mut tick_events: Vec<(u64, Ev)> = Vec::new();
    for track in &smf.tracks {
        let mut abs: u64 = 0;
        for ev in track {
            abs += ev.delta.as_int() as u64;
            let event = match ev.kind {
                midly::TrackEventKind::Midi { channel, message } => match message {
                    midly::MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 =>
                        Ev::NoteOn { channel: channel.as_int(), key: key.as_int(), vel: vel.as_int() },
                    midly::MidiMessage::NoteOn { key, .. } =>
                        Ev::NoteOff { channel: channel.as_int(), key: key.as_int() },
                    midly::MidiMessage::NoteOff { key, .. } =>
                        Ev::NoteOff { channel: channel.as_int(), key: key.as_int() },
                    midly::MidiMessage::Controller { controller, value } =>
                        Ev::Cc { channel: channel.as_int(), cc: controller.as_int(), val: value.as_int() },
                    _ => continue,
                },
                midly::TrackEventKind::Meta(midly::MetaMessage::Tempo(t)) => Ev::Tempo(t.as_int()),
                _ => continue,
            };
            tick_events.push((abs, event));
        }
    }

    // Sort by tick; Tempo events sort before MIDI events at the same tick.
    tick_events.sort_by(|(a, ea), (b, eb)| {
        a.cmp(b).then_with(|| {
            let pa: u8 = if matches!(ea, Ev::Tempo(_)) { 0 } else { 1 };
            let pb: u8 = if matches!(eb, Ev::Tempo(_)) { 0 } else { 1 };
            pa.cmp(&pb)
        })
    });

    // Pre-compute absolute µs timestamps, honouring tempo changes.
    let mut us_per_beat: u64 = 500_000; // 120 BPM
    let mut cur_tick: u64 = 0;
    let mut cur_us: u64 = 0;
    let mut timed: Vec<(u64, Ev)> = Vec::with_capacity(tick_events.len());
    for (abs_tick, ev) in tick_events {
        cur_us += (abs_tick - cur_tick) * us_per_beat / ticks_per_beat;
        cur_tick = abs_tick;
        if let Ev::Tempo(t) = &ev {
            us_per_beat = *t as u64;
        }
        timed.push((cur_us, ev));
    }

    let total_us = timed.last().map(|(t, _)| *t).unwrap_or(0);
    Ok((timed, total_us))
}

fn play_events(
    timed: Vec<(u64, Ev)>,
    tx: &Sender<MidiEvent>,
    stop: &AtomicBool,
    paused: &AtomicBool,
    current_us: &AtomicU64,
    seek_to: &AtomicU64,
) {
    let chunk = Duration::from_millis(10);
    let total_us = timed.last().map(|(t, _)| *t).unwrap_or(0);
    let mut start = Instant::now();
    let mut idx = 0usize;

    'outer: loop {
        // 1. Stop
        if stop.load(Ordering::Relaxed) { break; }

        // 2. Seek
        let seek = seek_to.swap(NO_SEEK, Ordering::Relaxed);
        if seek != NO_SEEK {
            let target = seek.min(total_us);
            start = Instant::now()
                .checked_sub(Duration::from_micros(target))
                .unwrap_or_else(Instant::now);
            idx = timed.partition_point(|(t, _)| *t < target);
            current_us.store(target, Ordering::Relaxed);
            let _ = tx.send(MidiEvent::AllNotesOff);
        }

        // 3. Pause – spin until unpaused, handling seeks and stops mid-pause.
        if paused.load(Ordering::Relaxed) {
            // Capture position at the moment of pausing.
            let mut pos = start.elapsed().as_micros() as u64;
            current_us.store(pos, Ordering::Relaxed);

            loop {
                thread::sleep(chunk);
                if stop.load(Ordering::Relaxed) { break 'outer; }

                // Seek while paused: update position + index but stay paused.
                let seek2 = seek_to.swap(NO_SEEK, Ordering::Relaxed);
                if seek2 != NO_SEEK {
                    pos = seek2.min(total_us);
                    idx = timed.partition_point(|(t, _)| *t < pos);
                    current_us.store(pos, Ordering::Relaxed);
                    let _ = tx.send(MidiEvent::AllNotesOff);
                }

                if !paused.load(Ordering::Relaxed) { break; }
            }

            // Resume: set start so start.elapsed() == pos.
            start = Instant::now()
                .checked_sub(Duration::from_micros(pos))
                .unwrap_or_else(Instant::now);
            continue 'outer;
        }

        // 4. Done?
        if idx >= timed.len() { break; }

        // 5. Update position.
        let now_us = start.elapsed().as_micros() as u64;
        current_us.store(now_us, Ordering::Relaxed);

        // 6. Sleep until the next event's timestamp.
        let event_time_us = timed[idx].0;
        if event_time_us > now_us {
            let mut remaining = Duration::from_micros(event_time_us - now_us);
            while remaining > chunk {
                thread::sleep(chunk);
                if stop.load(Ordering::Relaxed) { break 'outer; }
                if seek_to.load(Ordering::Relaxed) != NO_SEEK { continue 'outer; }
                if paused.load(Ordering::Relaxed) { continue 'outer; }
                let new_us = start.elapsed().as_micros() as u64;
                current_us.store(new_us, Ordering::Relaxed);
                if new_us >= event_time_us { break; }
                remaining = Duration::from_micros(event_time_us.saturating_sub(new_us));
            }
            if !remaining.is_zero() && remaining <= chunk {
                thread::sleep(remaining);
            }
        }

        // Re-check after sleep.
        if stop.load(Ordering::Relaxed) { break; }
        if seek_to.load(Ordering::Relaxed) != NO_SEEK { continue; }
        if paused.load(Ordering::Relaxed) { continue; }

        // 7. Send event.
        let midi_ev = match &timed[idx].1 {
            Ev::NoteOn  { channel, key, vel } =>
                MidiEvent::NoteOn  { channel: *channel, note: *key, velocity: *vel },
            Ev::NoteOff { channel, key } =>
                MidiEvent::NoteOff { channel: *channel, note: *key },
            Ev::Cc      { channel, cc, val } =>
                MidiEvent::ControlChange { channel: *channel, controller: *cc, value: *val },
            Ev::Tempo(_) => { idx += 1; continue; }
        };
        let _ = tx.send(midi_ev);
        idx += 1;
    }

    current_us.store(
        start.elapsed().as_micros() as u64,
        Ordering::Relaxed,
    );
}
