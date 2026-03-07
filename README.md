# pianeer

A low-latency piano sampler for keyboard instruments. Plays SFZ, Grand Orgue ODF, Kontakt 2, and GIG instruments in response to MIDI input. Runs on Linux, macOS, Windows, and Android.

## Download

Pre-built binaries are attached to each [GitHub Release](https://github.com/forain/pianeer/releases):

| Binary | Platform |
|--------|----------|
| `pianeer-macos-universal` | macOS — Intel and Apple Silicon (universal binary) |
| `pianeer-linux-amd64` | Linux x86_64 |
| `pianeer-linux-arm64` | Linux arm64 (Raspberry Pi OS 64-bit) |
| `pianeer-windows-amd64.exe` | Windows x86_64 |
| `pianeer-android.apk` | Android arm64 |

## Features

- Real-time stereo audio via JACK/PipeWire (Linux), CoreAudio (macOS), WASAPI (Windows), or Oboe/AAudio (Android)
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
- egui native window (optional — `--features native-ui`)
- egui web interface (WASM, served from the same port)

## Platform support

| Feature | Linux | macOS | Windows | Android |
|---------|:-----:|:-----:|:-------:|:-------:|
| **Audio backend** | JACK / PipeWire | CoreAudio | WASAPI | Oboe / AAudio |
| **MIDI input** | ALSA (midir) | CoreMIDI (midir) | WinMM (midir) | AMidi (NDK) |
| **Terminal UI** | ✓ | ✓ | ✓ | — |
| **Native egui window** | ✓ | ✓ | ✓ | — |
| **DRM/KMS egui** (headless, no display server) | ✓ | — | — | — |
| **Android egui** | — | — | — | ✓ |
| **Web UI** (browser, port 4000) | ✓ | ✓ | ✓ | ✓ |
| **MIDI file playback** | ✓ | ✓ | ✓ | ✓ |
| **MIDI recording** | ✓ | ✓ | ✓ | ✓ |
| **SFZ / ODF / GIG / Kontakt** | ✓ | ✓ | ✓ | ✓ |
| **Audio streaming** (ring buffer → web) | ✓ | ✓ | ✓ | ✓ |

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

### Windows
- Windows 10 or later (WASAPI, no external audio daemon needed)
- A MIDI keyboard connected via USB
- Rust toolchain (MSVC) for building

### Android
- Android 10+ (API 30+) — requires MANAGE_EXTERNAL_STORAGE permission
- Place instruments under `/sdcard/Pianeer/samples/` and MIDI files under `/sdcard/Pianeer/midi/`
- Built with cargo-apk; requires `kotlinc` for the JNI bridge classes (`pacman -S kotlin` / `sdk install kotlin`)

## Building

```bash
# Desktop — default (eframe window + DRM/KMS + terminal fallback)
cargo build --release -p pianeer

# Force raw-mode terminal at runtime
cargo run -p pianeer -- --text

# WASM web UI (trunk required)
cd web-wasm && trunk build --release

# Android APK (NDK r29 required)
cargo apk build --release -p pianeer-android

# WAV → FLAC batch converter
cargo build --release -p wav2flac
```

### Desktop feature flags

The default build compiles all UI backends in; the right one is chosen at runtime.

| Command | What you get |
|---|---|
| `cargo build -p pianeer` | **Default** — eframe window + DRM/KMS + terminal, auto-selected |
| `cargo run -p pianeer -- --text` | Force raw-mode terminal (also `-t`) |
| `cargo build -p pianeer --no-default-features` | Terminal only |
| `cargo build -p pianeer --no-default-features --features native-ui` | eframe + terminal fallback, no DRM |
| `cargo build -p pianeer --no-default-features --features kms` | DRM/KMS + terminal, no eframe |

**Runtime auto-selection** (default build, no `--text`):
1. `WAYLAND_DISPLAY` or `DISPLAY` set → eframe native window
2. Neither set + DRM device found → DRM/KMS egui (Linux headless)
3. Fallback → raw-mode terminal

## Running

**Linux** — route through the JACK/PipeWire shim:
```bash
pw-jack ./target/release/pianeer
```

**macOS** — run directly:
```bash
./target/release/pianeer
```

**Windows** — run directly:
```
target\release\pianeer.exe
```

**Native egui window** (Linux, macOS, or Windows):
```bash
pw-jack ./target/release/pianeer  # Linux, after building with --features native-ui
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
| P | Show QR code for web UI |
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

Pianeer is a Cargo workspace:

| Crate | Role |
|-------|------|
| `core` (`pianeer-core`) | Sampler engine, all parsers, MIDI, types, web server — no platform I/O |
| `desktop` (`pianeer`) | JACK (Linux) / CoreAudio (macOS) / WASAPI (Windows) binary: terminal UI and optional native egui window |
| `egui-app` (`pianeer-egui`) | Shared eframe `App` used by desktop (native-ui) and Android |
| `web-wasm` (`pianeer-wasm`) | WASM build of the egui UI, served by the embedded web server |
| `android` (`pianeer-android`) | Android NDK cdylib: Oboe audio + egui via android-activity |
| `tools/wav2flac` | Standalone WAV → FLAC batch converter |

### Core modules

| Module | Role |
|--------|------|
| `core/src/sampler.rs` | Real-time voice engine (`SamplerState`) |
| `core/src/loader.rs` | Parallel sample loading, WAV fixup, sample-rate probing |
| `core/src/parsers/sfz.rs` | SFZ parser with `#include`/`#define` expansion |
| `core/src/parsers/organ.rs` | Grand Orgue ODF parser |
| `core/src/parsers/kontakt.rs` | Kontakt 2 NKI/NKM parser |
| `core/src/parsers/gig.rs` | GIG/DLS binary parser |
| `core/src/region.rs` | Shared `Region` type output by all parsers |
| `core/src/instruments.rs` | `samples/` directory scanning |
| `core/src/midi_player.rs` | MIDI file playback thread (midly) |
| `core/src/midi_recorder.rs` | MIDI recording to SMF Type-0 `.mid` |
| `core/src/types.rs` | `MenuItem`, `MenuAction`, `Settings`, `PlaybackState`, `ProcStats` |
| `core/src/snapshot.rs` | `WebSnapshot`, `ClientCmd`, `build_snapshot()` |
| `core/src/sys_stats.rs` | CPU and memory stats (Linux `/proc`, macOS `getrusage`) |
| `core/src/web.rs` | Axum HTTP + WebTransport server, serves embedded WASM UI |
| `core/src/audio_stream.rs` | Lock-free ring buffer → FLAC encoder for audio streaming |

### Desktop modules

| Module | Role |
|--------|------|
| `desktop/src/main.rs` | Startup: instrument discovery, audio setup, channels |
| `desktop/src/terminal.rs` | Raw-mode terminal loop (`run_terminal`) |
| `desktop/src/gui.rs` | Native egui window path (`run_native_ui`, `--features native-ui`) |
| `desktop/src/ui.rs` | Terminal rendering: `print_menu`, VU meter, seekbar, QR modal |
| `desktop/src/audio.rs` | Platform audio dispatch: JACK/PipeWire (Linux), CoreAudio (macOS), WASAPI (Windows) |
| `desktop/src/midi.rs` | MIDI input: midir/ALSA (Linux), midir/CoreMIDI (macOS), midir/WinMM (Windows) |

### egui-app modules

| Module | Role |
|--------|------|
| `egui-app/src/app.rs` | `PianeerApp` — eframe `App` impl; renders stats/VU bar, settings panel (landscape) or drawer (portrait), scrollable instrument/MIDI list, QR popup |
| `egui-app/src/backend.rs` | `PianeerBackend` trait; `LocalBackend` (reads `Arc<Mutex<String>>` snapshot, sends `MenuAction` — used by desktop native-ui and Android); `RemoteBackend` (native: ewebsock WS; WASM: WebTransport with WS fallback) |

### web-wasm modules

| Module | Role |
|--------|------|
| `web-wasm/src/main.rs` | WASM entry point; derives WebSocket URL from the page origin, constructs `RemoteBackend`, mounts `PianeerApp` onto `#pianeer_canvas` via `eframe::WebRunner` |

### Android modules

| Module | Role |
|--------|------|
| `android/src/lib.rs` | NDK `cdylib` entry point; Oboe (AAudio/OpenSL ES) audio stream, JNI storage-permission gate, action dispatch + snapshot loop (mirrors `run_native_ui`), MIDI playback, web server, eframe UI via `android-activity` |
