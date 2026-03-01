//! MIDI file discovery and playback.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::sampler::MidiEvent;

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

/// Spawn a thread that plays `path` through `midi_tx`.
///
/// Set `stop` to abort early. `done` is set to `true` when the thread exits
/// (naturally or via stop). `AllNotesOff` is always sent on exit.
pub fn spawn(
    path: PathBuf,
    midi_tx: Sender<MidiEvent>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(e) = play(&path, &midi_tx, &stop) {
            eprintln!("MIDI playback error: {}", e);
        }
        let _ = midi_tx.send(MidiEvent::AllNotesOff);
        done.store(true, Ordering::Relaxed);
    })
}

// ─── Internal ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum Ev {
    NoteOn  { channel: u8, key: u8, vel: u8 },
    NoteOff { channel: u8, key: u8 },
    Cc      { channel: u8, cc: u8, val: u8 },
    Tempo(u32),
}

fn play(path: &Path, tx: &Sender<MidiEvent>, stop: &AtomicBool) -> Result<(), String> {
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

    // Pre-compute absolute microsecond timestamps, honouring tempo changes.
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

    // Play back with wall-clock scheduling; check stop every 10 ms during sleeps.
    let start = Instant::now();
    let chunk = Duration::from_millis(10);
    for (time_us, ev) in timed {
        if stop.load(Ordering::Relaxed) { break; }

        let target = Duration::from_micros(time_us);
        let elapsed = start.elapsed();
        if target > elapsed {
            let mut remaining = target - elapsed;
            while remaining > chunk {
                thread::sleep(chunk);
                if stop.load(Ordering::Relaxed) { return Ok(()); }
                remaining = target.saturating_sub(start.elapsed());
            }
            thread::sleep(remaining);
        }

        let midi_ev = match ev {
            Ev::NoteOn  { channel, key, vel } => MidiEvent::NoteOn  { channel, note: key, velocity: vel },
            Ev::NoteOff { channel, key }      => MidiEvent::NoteOff { channel, note: key },
            Ev::Cc      { channel, cc, val }  => MidiEvent::ControlChange { channel, controller: cc, value: val },
            Ev::Tempo(_) => continue,
        };
        let _ = tx.send(midi_ev);
    }
    Ok(())
}
