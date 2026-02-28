/// Grand Orgue ODF (.organ) parser.
///
/// Parses the INI-style organ definition file and produces a flat list of
/// Region values compatible with the sampler engine.
///
/// Supported ODF features:
///   - Multiple ranks with FirstMidiNoteNumber + NumberOfLogicalPipes
///   - Base attack sample (Pipe001=filename)
///   - Multiple velocity-layered attacks (Pipe001AttackCount / Pipe001Attack001…)
///   - Per-attack loop points (Loop001Start / Loop001End)
///   - Separate release samples (Pipe001ReleaseCount / Pipe001Release001…)
///   - AmplitudeLevel / Gain (rank-level dB gain)
///   - DUMMY and REF: pipe entries are skipped

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::region::{Region, Trigger};

// ─── INI parser ──────────────────────────────────────────────────────────────

type IniMap = HashMap<String, HashMap<String, String>>;

fn parse_ini(content: &str) -> IniMap {
    let mut sections: IniMap = HashMap::new();
    let mut current: Option<String> = None;

    for raw_line in content.lines() {
        // Strip ';' and '#' comments.
        let line = raw_line
            .find(|c| c == ';' || c == '#')
            .map(|i| &raw_line[..i])
            .unwrap_or(raw_line)
            .trim();

        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') {
            if let Some(end) = line.find(']') {
                let name = line[1..end].trim().to_string();
                sections.entry(name.clone()).or_default();
                current = Some(name);
            }
        } else if let Some(eq) = line.find('=') {
            if let Some(ref sec) = current {
                let key = line[..eq].trim().to_string();
                let value = line[eq + 1..].trim().to_string();
                sections.entry(sec.clone()).or_default().insert(key, value);
            }
        }
    }

    sections
}

// ─── Path resolution ─────────────────────────────────────────────────────────

/// Resolve a raw pipe filename relative to the ODF's base directory.
/// Tries the path as-is, then with common audio extensions appended.
fn resolve_path(base_dir: &Path, raw: &str) -> PathBuf {
    let normalized = raw.replace('\\', "/");
    let base = base_dir.join(&normalized);

    if base.exists() {
        return base;
    }
    for ext in &["wav", "flac", "ogg"] {
        let candidate = base.with_extension(ext);
        if candidate.exists() {
            return candidate;
        }
    }
    // Return with .wav suffix as a best guess even if missing (will error at load).
    base.with_extension("wav")
}

// ─── Helper accessors ────────────────────────────────────────────────────────

fn get_u32(sec: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    sec.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn get_f32(sec: &HashMap<String, String>, key: &str, default: f32) -> f32 {
    sec.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Convert an AmplitudeLevel percentage (100 = unity) to dB.
fn amplitude_level_to_db(level: f32) -> f32 {
    if level <= 0.0 {
        -144.0
    } else {
        20.0 * (level / 100.0).log10()
    }
}

/// Read explicit loop points for an attack whose INI prefix is `prefix`
/// (e.g. "Pipe001" or "Pipe001Attack002").
fn read_loop_points(
    sec: &HashMap<String, String>,
    prefix: &str,
) -> (Option<u64>, Option<u64>) {
    let loop_count = get_u32(sec, &format!("{}LoopCount", prefix), 0);
    if loop_count == 0 {
        return (None, None);
    }
    // Use the first loop region.
    let start_key = format!("{}Loop001Start", prefix);
    let end_key = format!("{}Loop001End", prefix);
    let start = sec.get(&start_key).and_then(|v| v.parse::<u64>().ok());
    let end = sec.get(&end_key).and_then(|v| v.parse::<u64>().ok());
    (start, end)
}

// ─── Region builder helpers ──────────────────────────────────────────────────

fn attack_region(
    sample: PathBuf,
    midi_note: u8,
    lovel: u8,
    hivel: u8,
    volume_db: f32,
    loop_start: Option<u64>,
    loop_end: Option<u64>,
) -> Region {
    Region {
        sample,
        lokey: midi_note,
        hikey: midi_note,
        lovel,
        hivel,
        pitch_keycenter: midi_note,
        // Organs are not velocity-sensitive by default.
        amp_veltrack: 0.0,
        // Short release: the separate release sample takes over on note-off.
        ampeg_release: 0.03,
        volume: volume_db,
        trigger: Trigger::Attack,
        rt_decay: 0.0,
        lorand: 0.0,
        hirand: 1.0,
        on_locc64: None,
        on_hicc64: None,
        group: None,
        off_by: None,
        pitch_keytrack: 100.0,
        loop_start,
        loop_end,
    }
}

fn release_region(sample: PathBuf, midi_note: u8, volume_db: f32) -> Region {
    Region {
        sample,
        lokey: midi_note,
        hikey: midi_note,
        lovel: 0,
        hivel: 127,
        pitch_keycenter: midi_note,
        amp_veltrack: 0.0,
        ampeg_release: 0.5,
        volume: volume_db,
        trigger: Trigger::Release,
        rt_decay: 0.0,
        lorand: 0.0,
        hirand: 1.0,
        on_locc64: None,
        on_hicc64: None,
        group: None,
        off_by: None,
        pitch_keytrack: 100.0,
        loop_start: None,
        loop_end: None,
    }
}

// ─── Main parser ─────────────────────────────────────────────────────────────

pub fn parse_organ(organ_path: &Path) -> Result<Vec<Region>, String> {
    let content = std::fs::read_to_string(organ_path)
        .map_err(|e| format!("Failed to read ODF: {}", e))?;

    let base_dir = organ_path.parent().unwrap_or(Path::new("."));
    let sections = parse_ini(&content);

    let organ_sec = sections
        .get("Organ")
        .ok_or_else(|| "ODF missing [Organ] section".to_string())?;

    let n_ranks = get_u32(organ_sec, "NumberOfRanks", 0) as usize;
    if n_ranks == 0 {
        return Err("ODF has NumberOfRanks=0".to_string());
    }

    let mut regions = Vec::new();

    for rank_idx in 1..=n_ranks {
        let rank_key = format!("Rank{:03}", rank_idx);
        let rank_sec = match sections.get(&rank_key) {
            Some(s) => s,
            None => {
                eprintln!("Warning: ODF section [{}] not found, skipping.", rank_key);
                continue;
            }
        };

        let first_midi: u8 = rank_sec
            .get("FirstMidiNoteNumber")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v.min(127) as u8)
            .unwrap_or(36);

        let n_pipes = get_u32(rank_sec, "NumberOfLogicalPipes", 0) as usize;

        // Rank-level amplitude: AmplitudeLevel (%) or Gain (dB).
        let amplitude_level = get_f32(rank_sec, "AmplitudeLevel", 100.0);
        let gain_db = get_f32(rank_sec, "Gain", 0.0);
        let volume_db = amplitude_level_to_db(amplitude_level) + gain_db;

        for pipe_idx in 1..=n_pipes {
            let pipe_prefix = format!("Pipe{:03}", pipe_idx);
            let midi_note = first_midi.saturating_add((pipe_idx - 1) as u8);

            let base_filename = match rank_sec.get(&pipe_prefix) {
                Some(v) => v.trim().to_string(),
                None => continue,
            };

            // Skip DUMMY and REF: entries.
            if base_filename.eq_ignore_ascii_case("DUMMY")
                || base_filename.starts_with("REF:")
            {
                continue;
            }

            let attack_count = get_u32(rank_sec, &format!("{}AttackCount", pipe_prefix), 0);

            if attack_count == 0 {
                // Simple case: the Pipe key value is the sole attack sample.
                let (ls, le) = read_loop_points(rank_sec, &pipe_prefix);
                let path = resolve_path(base_dir, &base_filename);
                regions.push(attack_region(path, midi_note, 0, 127, volume_db, ls, le));
            } else {
                // Multiple velocity-layered attacks.
                // The base pipe entry is attack #0; Attack001..N are additional.
                // Collect all (velocity_threshold, filename, loop) tuples.
                let base_vel = get_u32(rank_sec, &format!("{}AttackVelocity", pipe_prefix), 0) as u8;
                let (base_ls, base_le) = read_loop_points(rank_sec, &pipe_prefix);
                let mut attacks: Vec<(u8, String, Option<u64>, Option<u64>)> =
                    vec![(base_vel, base_filename.clone(), base_ls, base_le)];

                for ai in 1..=attack_count {
                    let ap = format!("{}Attack{:03}", pipe_prefix, ai);
                    let filename = match rank_sec.get(&ap) {
                        Some(v) => v.trim().to_string(),
                        None => continue,
                    };
                    let vel = get_u32(rank_sec, &format!("{}AttackVelocity", ap), 0) as u8;
                    let (ls, le) = read_loop_points(rank_sec, &ap);
                    attacks.push((vel, filename, ls, le));
                }

                // Sort by velocity threshold ascending.
                attacks.sort_by_key(|(v, _, _, _)| *v);

                for (i, (lovel, filename, ls, le)) in attacks.iter().enumerate() {
                    let hivel: u8 = if i + 1 < attacks.len() {
                        attacks[i + 1].0.saturating_sub(1)
                    } else {
                        127
                    };
                    let path = resolve_path(base_dir, filename);
                    regions.push(attack_region(path, midi_note, *lovel, hivel, volume_db, *ls, *le));
                }
            }

            // Release samples.
            let release_count = get_u32(rank_sec, &format!("{}ReleaseCount", pipe_prefix), 0);
            for ri in 1..=release_count {
                let rp = format!("{}Release{:03}", pipe_prefix, ri);
                let filename = match rank_sec.get(&rp) {
                    Some(v) => v.trim().to_string(),
                    None => continue,
                };
                let path = resolve_path(base_dir, &filename);
                regions.push(release_region(path, midi_note, volume_db));
            }
        }
    }

    Ok(regions)
}
