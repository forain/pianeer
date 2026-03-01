/// MIDI input thread using midir (ALSA backend).
///
/// Connects to a port named "midifilter output port" or any available port.
/// Forwards MIDI events through a crossbeam channel to the audio thread.
use crossbeam_channel::Sender;
use midir::{MidiInput, MidiInputConnection};

use crate::sampler::MidiEvent;

/// Hold the active MIDI connection alive. Drop to disconnect.
pub struct MidiConnection {
    _conn: MidiInputConnection<()>,
}

pub fn start_midi(tx: Sender<MidiEvent>) -> Result<MidiConnection, String> {
    let midi_in = MidiInput::new("pianeer").map_err(|e| e.to_string())?;

    let ports = midi_in.ports();
    if ports.is_empty() {
        return Err("No MIDI input ports found".to_string());
    }

    // Connect directly to the Keystation.
    let preferred_name = "Keystation";
    let port = ports
        .iter()
        .find(|p| {
            midi_in
                .port_name(p)
                .map(|n| n.contains(preferred_name))
                .unwrap_or(false)
        })
        .or_else(|| ports.first())
        .ok_or("No MIDI port available")?;

    let port_name = midi_in.port_name(port).unwrap_or_else(|_| "unknown".to_string());
    println!("Connecting to MIDI port: {}", port_name);

    let conn = midi_in
        .connect(
            port,
            "pianeer-input",
            move |_timestamp_us, raw, _| {
                if let Some(event) = parse_midi(raw) {
                    let _ = tx.send(event);
                }
            },
            (),
        )
        .map_err(|e| e.to_string())?;

    Ok(MidiConnection { _conn: conn })
}

fn parse_midi(data: &[u8]) -> Option<MidiEvent> {
    if data.is_empty() {
        return None;
    }
    let status = data[0];
    let msg_type = status & 0xF0;
    let channel = status & 0x0F;

    match msg_type {
        0x80 => {
            // Note Off
            if data.len() >= 3 {
                Some(MidiEvent::NoteOff {
                    channel,
                    note: data[1] & 0x7F,
                })
            } else {
                None
            }
        }
        0x90 => {
            // Note On (velocity 0 = note off)
            if data.len() >= 3 {
                Some(MidiEvent::NoteOn {
                    channel,
                    note: data[1] & 0x7F,
                    velocity: data[2] & 0x7F,
                })
            } else {
                None
            }
        }
        0xB0 => {
            // Control Change — filter out mod wheel (CC#1)
            if data.len() >= 3 {
                let controller = data[1] & 0x7F;
                let value = data[2] & 0x7F;
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
