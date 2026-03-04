/// MIDI input watcher.
///
/// Uses midir (ALSA on Linux, CoreMIDI on macOS, WinMM on Windows).
/// Polls the port list every 2 seconds; reconnects automatically on unplug/replug.
use crossbeam_channel::Sender;

use pianeer_core::midi_recorder::RecordHandle;
use pianeer_core::sampler::MidiEvent;

use midir::{MidiInput, MidiInputConnection};

const PREFERRED_PORT: &str = "Keystation";

use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Spawn the MIDI watcher thread.  Returns immediately; the thread runs for
/// the lifetime of the process.
pub fn spawn_midi_watcher(tx: Sender<MidiEvent>, record: RecordHandle) {
    std::thread::spawn(move || {
        let mut conn: Option<MidiInputConnection<()>> = None;
        let mut connected_name: Option<String> = None;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            let mi = match MidiInput::new("pianeer") {
                Ok(m) => m,
                Err(_) => continue,
            };
            let ports = mi.ports();

            // ── Step 1: check whether our current port is still present ───────
            if conn.is_some() {
                let still_there = ports.iter().any(|p| {
                    mi.port_name(p).ok().as_deref() == connected_name.as_deref()
                });
                if !still_there {
                    conn = None;
                    println!(
                        "MIDI port '{}' disconnected.",
                        connected_name.as_deref().unwrap_or("?")
                    );
                    connected_name = None;
                }
            }

            // ── Step 2: try to connect if not connected ────────────────────────
            if conn.is_none() && !ports.is_empty() {
                let port = ports
                    .iter()
                    .find(|p| {
                        mi.port_name(p)
                            .map(|n| n.contains(PREFERRED_PORT))
                            .unwrap_or(false)
                    })
                    .or_else(|| ports.first());

                if let Some(port) = port {
                    let name = mi.port_name(port).unwrap_or_default();
                    let tx2  = tx.clone();
                    let rec2 = record.clone();
                    match mi.connect(
                        port,
                        "pianeer-input",
                        move |_, raw, _| {
                            if let Some(ev) = parse_midi(raw) {
                                if let Ok(mut g) = rec2.lock() {
                                    if let Some(ref mut buf) = *g {
                                        buf.events.push((buf.start.elapsed(), ev.clone()));
                                    }
                                }
                                let _ = tx2.send(ev);
                            }
                        },
                        (),
                    ) {
                        Ok(c) => {
                            println!("MIDI connected: {}", name);
                            conn = Some(c);
                            connected_name = Some(name);
                        }
                        Err(_) => {} // port not ready yet — will retry next cycle
                    }
                }
            }
        }
    });
}

fn parse_midi(data: &[u8]) -> Option<MidiEvent> {
    if data.is_empty() {
        return None;
    }
    let status   = data[0];
    let msg_type = status & 0xF0;
    let channel  = status & 0x0F;

    match msg_type {
        0x80 => {
            if data.len() >= 3 {
                Some(MidiEvent::NoteOff { channel, note: data[1] & 0x7F })
            } else {
                None
            }
        }
        0x90 => {
            if data.len() >= 3 {
                Some(MidiEvent::NoteOn {
                    channel,
                    note:     data[1] & 0x7F,
                    velocity: data[2] & 0x7F,
                })
            } else {
                None
            }
        }
        0xB0 => {
            if data.len() >= 3 {
                let controller = data[1] & 0x7F;
                let value      = data[2] & 0x7F;
                if controller == 1 {
                    None // mod wheel filtered
                } else if controller == 123 || controller == 120 {
                    Some(MidiEvent::AllNotesOff)
                } else {
                    Some(MidiEvent::ControlChange { channel, controller, value })
                }
            } else {
                None
            }
        }
        0xE0 => None, // pitch bend filtered
        _ => None,
    }
}
