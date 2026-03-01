# pianeer

A low-latency JACK audio sampler for keyboard instruments. Plays SFZ and Grand Orgue ODF instruments in response to MIDI input.

## Features

- Real-time stereo audio via JACK/PipeWire
- SFZ format support (`<global>`, `<master>`, `<group>`, `<region>` hierarchy)
- Grand Orgue ODF format support (`.organ` files with WAV/FLAC samples)
- In-app instrument menu — press 1–9 to switch instruments live without restarting
- Parallel sample loading
- Voice engine: up to 128 voices, linear interpolation, looping, sustain pedal, release triggers
- `note_polyphony` enforcement, `off_by` group muting, round-robin (`lorand`/`hirand`)

## Requirements

- PipeWire with JACK compatibility (`pipewire-jack`)
- ALSA MIDI (for keyboard input via midir)
- A MIDI keyboard (looks for "Keystation" by default, falls back to first available port)
- Rust toolchain for building

## Building

```bash
cargo build --release
```

## Running

```bash
./target/release/pianeer
```

Instruments are discovered automatically from `samples/` subdirectories. Place each instrument in its own subdirectory:

```
samples/
├── My Piano/
│   └── instrument.sfz
└── My Organ/
    └── instrument.organ
```

Press **1–9** to switch instruments, **Q** or **Ctrl+C** to quit.

## SFZ support

The parser handles the opcodes present in the bundled instruments:

| Opcode | Notes |
|--------|-------|
| `sample`, `lokey`, `hikey`, `lovel`, `hivel`, `pitch_keycenter` | Basic region mapping |
| `amp_veltrack`, `volume`, `ampeg_release` | Amplitude and envelope |
| `trigger` | `attack` or `release` |
| `lorand`, `hirand` | Round-robin / random selection |
| `group`, `off_by` | Voice muting |
| `note_polyphony` | Per-note voice limit |
| `on_locc64`, `on_hicc64` | Sustain pedal regions |
| `pitch_keytrack`, `rt_decay` | Pitch and release decay |

Headers: `<global>`, `<master>`, `<group>`, `<region>` (full inheritance chain). `<control>` is parsed and ignored.

## Architecture

| File | Role |
|------|------|
| `src/main.rs` | Entry point: instrument discovery, JACK setup, terminal UI |
| `src/instruments.rs` | `samples/` directory scanning |
| `src/sampler.rs` | Real-time JACK voice engine |
| `src/midi.rs` | ALSA MIDI input thread (midir) |
| `src/sfz.rs` | SFZ parser |
| `src/organ.rs` | Grand Orgue ODF parser |
| `src/region.rs` | Shared `Region` type |
