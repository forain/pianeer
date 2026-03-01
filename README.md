# pianeer

A low-latency JACK audio sampler for keyboard instruments. Plays SFZ, Grand Orgue ODF, Kontakt 2, and GIG instruments in response to MIDI input.

## Features

- Real-time stereo audio via JACK/PipeWire
- SFZ v2/ARIA format support with full `<global>` → `<master>` → `<group>` → `<region>` inheritance
- Grand Orgue ODF format support (`.organ` files)
- Kontakt 2 format support (`.nki` / `.nkm` files)
- GIG format support (`.gig` files — RIFF/DLS with embedded PCM)
- In-app instrument menu — press 1–9 to switch instruments live, R to rescan
- MIDI file playback (press 1–9 on a `.mid` file)
- Parallel sample loading
- Voice engine: up to 128 voices, Hermite cubic interpolation, looping, sustain pedal, release triggers
- `note_polyphony` enforcement, `off_by` group muting, round-robin (`lorand`/`hirand`)
- CC-triggered regions (`on_locc$N`/`on_hicc$N`) for pedal and effect sounds

## Requirements

- PipeWire with JACK compatibility (`pipewire-jack`)
- ALSA MIDI (keyboard input via midir)
- A MIDI keyboard (looks for "Keystation" by default, falls back to first available port)
- Rust toolchain for building

## Building

```bash
cargo build --release
```

## Running

```bash
pw-jack ./target/release/pianeer
```

Instruments are discovered automatically from a `samples/` directory next to the binary (or in the working directory). Place each instrument in its own subdirectory:

```
samples/
├── My Piano/
│   └── instrument.sfz
├── My Organ/
│   └── instrument.organ
├── My Harpsichord/
│   └── instrument.nki
└── My GIG Piano/
    └── instrument.gig
```

Press **1–9** to load an instrument or play a MIDI file, **R** to rescan the samples directory, **Q** or **Ctrl+C** to quit.

## SFZ support

Full `<global>` → `<master>` → `<group>` → `<region>` inheritance chain. `#include` and `#define` macro expansion. `<control>` for `default_path` and `set_cc`/`set_hdcc` initial CC values.

| Opcode | Notes |
|--------|-------|
| `sample`, `lokey`, `hikey`, `key`, `lovel`, `hivel`, `pitch_keycenter` | Region mapping |
| `amp_veltrack`, `volume`, `ampeg_release`, `off_time` | Amplitude and envelope |
| `trigger` | `attack` or `release` |
| `rt_decay` | Release-trigger amplitude decay (dB/s) |
| `lorand`, `hirand` | Round-robin / random selection |
| `group`, `off_by`, `note_polyphony` | Voice management |
| `on_locc$N`, `on_hicc$N` | CC-event-triggered regions (any CC number) |
| `locc$N`, `hicc$N` | CC range conditions for note-on filtering |
| `pitch_keytrack`, `tune` | Pitch modulation |
| `pan`, `pan_oncc$N` | Stereo panning |
| `sw_last`, `sw_lokey`, `sw_hikey`, `sw_default` | Keyswitches |
| `amplitude_oncc$N`, `amp_veltrack_oncc$N` | CC-modulated amplitude and velocity tracking |
| `ampeg_release_oncc$N` | CC-modulated release time |
| `offset`, `offset_oncc$N` | Sample start offset |
| `loop_start`, `loop_end` | Loop points |

## ODF support

Grand Orgue ODF (`.organ`) files use an INI-style format with `[Organ]` and `[RankNNN]` sections.

| Key | Notes |
|-----|-------|
| `NumberOfRanks` | Number of `[RankNNN]` sections to load |
| `FirstMidiNoteNumber`, `NumberOfLogicalPipes` | MIDI range for the rank |
| `AmplitudeLevel`, `Gain` | Rank gain (% and dB) |
| `HarmonicNumber`, `MidiPitchFraction` | Tuning |
| `PipeNNN`, `PipeNNNAttackCount`, `PipeNNNAttack001`… | Attack samples with optional velocity layers |
| `PipeNNNLoopCount`, `PipeNNNLoop001Start/End` | Loop points |
| `PipeNNNReleaseCount`, `PipeNNNRelease001`…, `PipeNNNReleaseNNNMaxKeyPressTime` | Timed release samples |

`DUMMY` and `REF:` pipe entries are skipped.

## GIG support

GIG (`.gig`) is a RIFF/DLS Level 2 container with embedded 16-bit PCM audio, used by GigaSampler and LinuxSampler. Samples are decoded in-memory at load time; regions sharing the same pool entry share the PCM buffer via reference counting.

The first pure-attack instrument (no `RELEASETRIGGER` dimension, name does not contain "release") and the first pure-release instrument (no `RELEASETRIGGER` dimension, name contains "release") in the file are loaded. Instruments that encode both attack and release via a `RELEASETRIGGER` dimension are skipped because the same file already provides equivalent separate instruments, and loading both would stack redundant voice layers.

| Concept | Notes |
|---------|-------|
| Wave pool (`ptbl` + `wvpl`) | Up to 32-bit pool index table; offsets relative to `wvpl` data start |
| Dimension regions (`3lnk`) | `SAMPLECHANNEL` (0x80) and `VELOCITY` (0x82) dimensions; left-channel (SC=0) side only |
| Velocity ranges | `VelocityUpperLimit` read from `3ewa` chunk at byte offset +124, one per DimRegion |
| Loop points | Read from wave-level `wsmp` chunk: `LoopStart` / `LoopStart + LoopLength` (exclusive end) |
| Trigger detection | Instrument name containing "release" → `Trigger::Release`; otherwise `Trigger::Attack` |
| Pitch | `pitch_keycenter = key_range.low` (one sample per key, plays at 1× speed on its mapped key) |

## Kontakt 2 support

Reads `.nki` (single program) and `.nkm` (multi-program bank) files. The binary format contains a zlib-compressed XML payload. `.nkm` files are suppressed in the menu when `.nki` files are present in the same directory.

| Element | Attribute | Notes |
|---------|-----------|-------|
| `K2_Program` | `volume` | Program-level linear gain, converted to dB |
| `K2_Group` | `releaseTrigger` | `"yes"` → `trigger=release`, otherwise `trigger=attack` |
| `K2_Group` | `volume` | Group linear gain, added to program gain |
| `K2_Group` > `Envelope` | `release` | Amplitude envelope release time in seconds |
| `K2_Zone` | `groupIdx` | Links zone to its parent group |
| `K2_Zone` | `lowKey`, `highKey` | MIDI note range |
| `K2_Zone` | `rootKey` | Pitch centre for transposition |
| `K2_Zone` | `lowVelocity`, `highVelocity` | Velocity range (1-based, converted to 0-based) |
| `K2_Zone` | `zoneVolume` | Zone linear gain, added to group+program gain |
| `K2_Zone` > `Sample` | `file_ex2` | Encoded sample path: `@d<len><dir>…F-<10flags><file>` |

## Architecture

| File | Role |
|------|------|
| `src/main.rs` | Entry point: instrument discovery, JACK setup, terminal UI, MIDI file playback |
| `src/instruments.rs` | `samples/` directory scanning (1–2 levels deep) |
| `src/sampler.rs` | Real-time JACK voice engine |
| `src/midi.rs` | ALSA MIDI input thread (midir) |
| `src/midi_player.rs` | MIDI file playback thread (midly) |
| `src/sfz.rs` | SFZ parser with `#include`/`#define` expansion |
| `src/organ.rs` | Grand Orgue ODF parser |
| `src/kontakt.rs` | Kontakt 2 NKI/NKM parser |
| `src/gig.rs` | GIG/DLS binary parser |
| `src/region.rs` | Shared `Region` type |
