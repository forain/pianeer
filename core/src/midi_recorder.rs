//! MIDI recording: capture live MIDI events to an SMF Type-0 file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::sampler::MidiEvent;

// ─── Public types ─────────────────────────────────────────────────────────────

pub struct RecordBuf {
    pub start: Instant,
    pub events: Vec<(Duration, MidiEvent)>,
}

/// Shared recording state — cloned into the MIDI input callback.
pub type RecordHandle = Arc<Mutex<Option<RecordBuf>>>;

pub fn new_handle() -> RecordHandle {
    Arc::new(Mutex::new(None))
}

pub fn start(handle: &RecordHandle) {
    if let Ok(mut g) = handle.lock() {
        *g = Some(RecordBuf { start: Instant::now(), events: Vec::new() });
    }
}

/// Stop recording; returns the buffer or None if not recording.
pub fn stop(handle: &RecordHandle) -> Option<RecordBuf> {
    handle.lock().ok()?.take()
}

pub fn is_recording(handle: &RecordHandle) -> bool {
    handle.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Duration since recording started, or None.
pub fn elapsed(handle: &RecordHandle) -> Option<Duration> {
    handle.lock().ok()?.as_ref().map(|b| b.start.elapsed())
}

// ─── Save ─────────────────────────────────────────────────────────────────────

/// Write an SMF Type-0 file to `dir` and return its path.
/// Uses 480 PPQ at 120 BPM (500 000 µs/beat).
pub fn save(buf: RecordBuf, dir: &Path) -> Result<PathBuf, String> {
    const PPQ: u64 = 480;
    const TEMPO_US: u64 = 500_000; // 120 BPM

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("rec_{}.mid", ts));

    let mut sorted = buf.events;
    sorted.sort_by_key(|(d, _)| *d);

    // Build track data
    let mut track: Vec<u8> = Vec::new();

    // Tempo meta-event at tick 0: delta=0, FF 51 03 tt tt tt
    push_vlq(&mut track, 0);
    track.extend_from_slice(&[0xFF, 0x51, 0x03]);
    track.push(((TEMPO_US >> 16) & 0xFF) as u8);
    track.push(((TEMPO_US >>  8) & 0xFF) as u8);
    track.push(( TEMPO_US        & 0xFF) as u8);

    let us_per_tick = TEMPO_US / PPQ; // 1041 µs/tick
    let mut prev_tick: u64 = 0;

    for (offset, event) in &sorted {
        let tick = offset.as_micros() as u64 / us_per_tick;
        let delta = tick.saturating_sub(prev_tick);
        prev_tick = tick;

        match event {
            MidiEvent::NoteOn { channel, note, velocity } => {
                push_vlq(&mut track, delta);
                track.push(0x90 | (channel & 0x0F));
                track.push(note     & 0x7F);
                track.push(velocity & 0x7F);
            }
            MidiEvent::NoteOff { channel, note } => {
                push_vlq(&mut track, delta);
                track.push(0x80 | (channel & 0x0F));
                track.push(note & 0x7F);
                track.push(0);
            }
            MidiEvent::ControlChange { channel, controller, value } => {
                push_vlq(&mut track, delta);
                track.push(0xB0 | (channel & 0x0F));
                track.push(controller & 0x7F);
                track.push(value      & 0x7F);
            }
            MidiEvent::AllNotesOff => {} // synthetic — skip
        }
    }

    // End of track
    push_vlq(&mut track, 0);
    track.extend_from_slice(&[0xFF, 0x2F, 0x00]);

    // Assemble SMF
    let mut smf: Vec<u8> = Vec::new();
    smf.extend_from_slice(b"MThd");
    smf.extend_from_slice(&[0, 0, 0, 6]);   // header length = 6
    smf.extend_from_slice(&[0, 0]);          // format 0
    smf.extend_from_slice(&[0, 1]);          // 1 track
    smf.push((PPQ >> 8) as u8);
    smf.push((PPQ & 0xFF) as u8);
    smf.extend_from_slice(b"MTrk");
    let tlen = track.len() as u32;
    smf.push((tlen >> 24) as u8);
    smf.push((tlen >> 16) as u8);
    smf.push((tlen >>  8) as u8);
    smf.push( tlen        as u8);
    smf.extend_from_slice(&track);

    std::fs::write(&path, &smf).map_err(|e| e.to_string())?;
    Ok(path)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn push_vlq(buf: &mut Vec<u8>, mut val: u64) {
    let mut bytes = [0u8; 9];
    let mut n = 0;
    bytes[n] = (val & 0x7F) as u8;
    n += 1;
    val >>= 7;
    while val > 0 {
        bytes[n] = 0x80 | (val & 0x7F) as u8;
        n += 1;
        val >>= 7;
    }
    for i in (0..n).rev() {
        buf.push(bytes[i]);
    }
}
