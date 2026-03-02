# pianeer

A low-latency piano sampler for keyboard instruments. Plays SFZ, Grand Orgue ODF, Kontakt 2, and GIG instruments in response to MIDI input.

## Download

Pre-built binaries are attached to each [GitHub Release](https://github.com/forain/pianeer/releases):

| Binary | Platform |
|--------|----------|
| `pianeer-macos-universal` | macOS — Intel and Apple Silicon (universal binary) |
| `pianeer-linux-amd64` | Linux x86_64 |
| `pianeer-linux-arm64` | Linux arm64 (Raspberry Pi OS 64-bit) |

## Features

- Real-time stereo audio via JACK/PipeWire (Linux) or CoreAudio (macOS)
- SFZ v2/ARIA format with full `<global>` → `<master>` → `<group>` → `<region>` inheritance
- Grand Orgue ODF format (`.organ` files)
- Kontakt 2 format (`.nki` / `.nkm` files)
- GIG format (`.gig` files — RIFF/DLS with embedded PCM)
- Instrument menu — arrow keys + Enter to switch, R to rescan
- MIDI file playback (navigate to a `.mid` file and press Enter)
- MIDI recording (W to start/stop; saves to `midi/`)
- Parallel sample loading
- Voice engine: up to 128 voices, Hermite cubic interpolation, looping, sustain pedal, release triggers
- `note_polyphony` enforcement, `off_by` group muting, round-robin (`lorand`/`hirand`)
- CC-triggered regions (`on_locc$N`/`on_hicc$N`) for pedal and effect sounds
- 8 real-time playback controls: velocity tracking, tuning, release, volume, transpose, variant, resonance, sustain
- VU meter with clip detection and CPU/memory stats
- Web UI with WebTransport (falls back to WebSocket) — reachable at `https://localhost:4000`

## Requirements

### Linux
- PipeWire with JACK compatibility (`pipewire-jack`) or a JACK daemon
- ALSA MIDI (keyboard input via midir)
- A MIDI keyboard (looks for "Keystation" by default, falls back to first available port)
- Rust toolchain + `libjack-jackd2-dev` for building

### macOS
- macOS 11 or later (CoreAudio, no external audio daemon needed)
- A MIDI keyboard connected via USB or Core MIDI
- Rust toolchain + Xcode Command Line Tools for building

## Building

```bash
cargo build --release
```

## Running

**Linux** — route through the JACK/PipeWire shim:
```bash
pw-jack ./target/release/pianeer
```

**macOS** — run directly:
```bash
./target/release/pianeer
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

### Keyboard controls

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate menu |
| Enter | Load instrument / play MIDI file |
| ← / → | Cycle instrument variant |
| R | Rescan samples directory |
| V | Cycle velocity tracking |
| T | Cycle tuning |
| E | Toggle release samples |
| + / - | Volume ±3 dB |
| [ / ] | Transpose ±1 semitone |
| H | Toggle resonance |
| W | Start / stop MIDI recording |
| Space | Pause / resume MIDI playback |
| , / . | Seek MIDI playback ±10 s |
| Q / Ctrl+C | Quit |

## Format support

| Feature | SFZ | ODF | GIG | Kontakt 2 |
|---------|:---:|:---:|:---:|:---------:|
| **File types** | `.sfz` | `.organ` | `.gig` | `.nki` `.nkm` |
| **Velocity layers** | ✓ | ✓ | ✓ | ✓ |
| **Loop points** | ✓ | ✓ | ✓ | — |
| **Release triggers** | ✓ | ✓ | ✓ | ✓ |
| **Timed release** (varies by key-hold duration) | — | ✓ | — | — |
| **Round-robin / random selection** | ✓ | — | — | — |
| **Keyswitches** | ✓ | — | — | — |
| **CC-triggered regions** | ✓ | — | — | — |
| **CC range filtering** (note-on conditions) | ✓ | — | — | — |
| **CC-modulated parameters** (amp, release, pan…) | ✓ | — | — | — |
| **Voice groups & off_by muting** | ✓ | — | — | — |
| **Per-region polyphony limit** | ✓ | — | — | — |
| **Stereo panning** | ✓ | — | — | — |
| **Tuning / pitch control** | ✓ | ✓ | — | — |
| **Sample start offset** | ✓ | — | — | — |
| **Amplitude envelope release** | ✓ | — | — | ✓ |
| **Nested parameter inheritance** | ✓ (4 levels) | — | — | ✓ (3 levels) |
| **Multiple instruments per file** | — | — | ✓ | ✓ (`.nkm`) |

### SFZ

INI-like text format. Full `<global>` → `<master>` → `<group>` → `<region>` inheritance with `#include` / `#define` macro expansion. The most feature-complete format supported: CC modulation, round-robin, keyswitches, voice groups, panning, and sample offsets are all SFZ-only capabilities.

### ODF (Grand Orgue)

INI-style `.organ` files with `[Organ]` and `[RankNNN]` sections. Unique feature: **timed release samples** — each pipe can define multiple release samples selected by how long the key was held (`MaxKeyPressTime`). Tuning via `HarmonicNumber` and `MidiPitchFraction`. `DUMMY` and `REF:` pipe entries are skipped.

### GIG

RIFF/DLS Level 2 container (`.gig`) with embedded 16-bit PCM, used by GigaSampler and LinuxSampler. PCM data is decoded at load time; regions sharing the same wave-pool entry share a reference-counted buffer. Prefers a combined attack+release instrument (encoded via the `RELEASETRIGGER` dimension) when present; falls back to separate attack and release instruments otherwise.

### Kontakt 2

Binary `.nki` / `.nkm` files with a zlib-compressed XML payload. Three-level hierarchy: program → group → zone. `.nkm` multi-program banks are suppressed in the menu when `.nki` files exist in the same directory.

## Architecture

| File | Role |
|------|------|
| `src/main.rs` | Entry point: instrument discovery, audio setup, input loop, system stats |
| `src/audio.rs` | Platform audio dispatch: JACK/PipeWire (Linux) and CoreAudio via cpal (macOS) |
| `src/sampler.rs` | Real-time voice engine |
| `src/loader.rs` | Parallel sample loading, WAV fixup, sample-rate probing |
| `src/instruments.rs` | `samples/` directory scanning (1–2 levels deep) |
| `src/ui.rs` | Terminal menu rendering, VU meter, playback controls |
| `src/web.rs` | Web UI server — WebTransport (QUIC) with WebSocket fallback |
| `src/midi.rs` | MIDI input thread (midir — CoreMIDI on macOS, ALSA on Linux) |
| `src/midi_player.rs` | MIDI file playback thread (midly) |
| `src/midi_recorder.rs` | MIDI recording to `.mid` file |
| `src/sfz.rs` | SFZ parser with `#include`/`#define` expansion |
| `src/organ.rs` | Grand Orgue ODF parser |
| `src/kontakt.rs` | Kontakt 2 NKI/NKM parser |
| `src/gig.rs` | GIG/DLS binary parser |
| `src/region.rs` | Shared `Region` type |
