/// SFZ parser for the Salamander Grand Piano instrument.
///
/// Handles the opcodes actually present in the file:
///   sample, lokey, hikey, lovel, hivel, pitch_keycenter,
///   amp_veltrack, ampeg_release, volume, trigger, rt_decay,
///   lorand, hirand, on_locc64, on_hicc64, group, off_by, pitch_keytrack, pan
use std::path::Path;

pub use crate::region::{Region, Trigger};

/// Accumulates opcode state for a <group> header.
#[derive(Debug, Clone, Default)]
struct GroupState {
    amp_veltrack: Option<f32>,
    ampeg_release: Option<f32>,
    volume: Option<f32>,
    trigger: Option<Trigger>,
    rt_decay: Option<f32>,
    on_locc64: Option<u8>,
    on_hicc64: Option<u8>,
    group: Option<u32>,
    off_by: Option<u32>,
    pitch_keytrack: Option<f32>,
    pan: Option<f32>,
    lokey: Option<u8>,
    hikey: Option<u8>,
    lovel: Option<u8>,
    hivel: Option<u8>,
    note_polyphony: Option<u32>,
}

/// Recursively expand `#include "file"` directives in SFZ content.
/// Lines with `//` comments are stripped before checking for `#include`.
/// Depth-limited to 8 levels.
fn expand_includes(content: &str, base_dir: &Path, depth: usize) -> Result<String, String> {
    if depth > 8 {
        return Err("SFZ #include depth limit exceeded".to_string());
    }
    let mut result = String::new();
    for line in content.lines() {
        // Strip // comments to detect #include directives.
        let stripped = if let Some(idx) = line.find("//") {
            line[..idx].trim()
        } else {
            line.trim()
        };

        if let Some(rest) = stripped.strip_prefix("#include") {
            let rest = rest.trim();
            if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
                let filename = &rest[1..rest.len() - 1];
                let include_path = base_dir.join(filename.replace('\\', "/"));
                let include_dir = include_path.parent().unwrap_or(base_dir);
                let include_content = std::fs::read_to_string(&include_path)
                    .map_err(|e| format!("Failed to read #include {:?}: {}", include_path, e))?;
                let expanded = expand_includes(&include_content, include_dir, depth + 1)?;
                result.push_str(&expanded);
                result.push('\n');
                continue;
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    Ok(result)
}

pub fn parse_sfz(sfz_path: &Path) -> Result<Vec<Region>, String> {
    let raw = std::fs::read_to_string(sfz_path)
        .map_err(|e| format!("Failed to read SFZ: {}", e))?;

    let base_dir = sfz_path.parent().unwrap_or(Path::new("."));

    let content = expand_includes(&raw, base_dir, 0)
        .map_err(|e| format!("SFZ include error: {}", e))?;

    let mut regions = Vec::new();
    let mut global = GroupState::default();
    let mut group = GroupState::default();
    let mut in_region = false;
    let mut in_global = false;
    let mut current_region = Region::default();

    for line in content.lines() {
        // Strip line comments (// style)
        let line = if let Some(idx) = line.find("//") {
            &line[..idx]
        } else {
            line
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Tokenise: split on whitespace but handle <header> tokens
        // mixed with opcode=value tokens on the same line.
        let mut tokens: Vec<&str> = Vec::new();
        let mut rest = line;
        while !rest.is_empty() {
            rest = rest.trim_start();
            if rest.starts_with('<') {
                if let Some(end) = rest.find('>') {
                    tokens.push(&rest[..end + 1]);
                    rest = &rest[end + 1..];
                } else {
                    break;
                }
            } else {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                tokens.push(&rest[..end]);
                rest = &rest[end..];
            }
        }

        for token in tokens {
            if token.starts_with('<') && token.ends_with('>') {
                let header = &token[1..token.len() - 1];
                match header {
                    "global" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        global = GroupState::default();
                        in_global = true;
                    }
                    // <master> sits between <global> and <group> in the SFZ v2
                    // hierarchy. Inherit from global; group will inherit from it.
                    "master" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        group = global.clone();
                        in_global = false;
                    }
                    "group" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        // Inherit global defaults; group-level opcodes will override.
                        group = global.clone();
                        current_region = Region::default();
                        in_global = false;
                    }
                    "region" => {
                        if in_region {
                            regions.push(current_region.clone());
                        }
                        current_region = region_from_group(&group);
                        in_region = true;
                        in_global = false;
                    }
                    // <control> contains set_cc / label_cc — no audio impact, skip.
                    "control" => {
                        in_global = false;
                    }
                    _ => {}
                }
            } else if let Some(eq_pos) = token.find('=') {
                let key = &token[..eq_pos];
                let val = &token[eq_pos + 1..];

                if in_region {
                    apply_opcode_to_region(&mut current_region, key, val, base_dir);
                } else if in_global {
                    apply_opcode_to_group(&mut global, key, val);
                } else {
                    apply_opcode_to_group(&mut group, key, val);
                }
            }
        }
    }

    if in_region {
        regions.push(current_region);
    }

    Ok(regions)
}

fn region_from_group(g: &GroupState) -> Region {
    let mut r = Region::default();
    if let Some(v) = g.amp_veltrack { r.amp_veltrack = v; }
    if let Some(v) = g.ampeg_release { r.ampeg_release = v; }
    if let Some(v) = g.volume { r.volume = v; }
    if let Some(ref v) = g.trigger { r.trigger = v.clone(); }
    if let Some(v) = g.rt_decay { r.rt_decay = v; }
    r.on_locc64 = g.on_locc64;
    r.on_hicc64 = g.on_hicc64;
    r.group = g.group;
    r.off_by = g.off_by;
    if let Some(v) = g.pitch_keytrack { r.pitch_keytrack = v; }
    if let Some(v) = g.pan { r.pan = v; }
    if let Some(v) = g.lokey { r.lokey = v; }
    if let Some(v) = g.hikey { r.hikey = v; }
    if let Some(v) = g.lovel { r.lovel = v; }
    if let Some(v) = g.hivel { r.hivel = v; }
    r.note_polyphony = g.note_polyphony;
    r
}

fn apply_opcode_to_group(g: &mut GroupState, key: &str, val: &str) {
    match key {
        "amp_veltrack" => g.amp_veltrack = val.parse().ok(),
        "ampeg_release" => g.ampeg_release = val.parse().ok(),
        "volume" => g.volume = val.parse().ok(),
        "trigger" => g.trigger = parse_trigger(val),
        "rt_decay" => g.rt_decay = val.parse().ok(),
        "on_locc64" => g.on_locc64 = val.parse().ok(),
        "on_hicc64" => g.on_hicc64 = val.parse().ok(),
        "group" => g.group = val.parse().ok(),
        "off_by" => g.off_by = val.parse().ok(),
        "pitch_keytrack" => g.pitch_keytrack = val.parse().ok(),
        "pan" => g.pan = val.parse().ok(),
        "lokey" => g.lokey = parse_key(val),
        "hikey" => g.hikey = parse_key(val),
        "lovel" => g.lovel = val.parse().ok(),
        "hivel" => g.hivel = val.parse().ok(),
        "note_polyphony" => g.note_polyphony = val.parse().ok(),
        _ => {}
    }
}

fn apply_opcode_to_region(r: &mut Region, key: &str, val: &str, base_dir: &Path) {
    match key {
        "sample" => {
            let normalized = val.replace('\\', "/");
            r.sample = base_dir.join(&normalized);
        }
        "lokey" => { if let Some(v) = parse_key(val) { r.lokey = v; } }
        "hikey" => { if let Some(v) = parse_key(val) { r.hikey = v; } }
        "lovel" => { if let Some(v) = val.parse().ok() { r.lovel = v; } }
        "hivel" => { if let Some(v) = val.parse().ok() { r.hivel = v; } }
        "pitch_keycenter" => { if let Some(v) = parse_key(val) { r.pitch_keycenter = v; } }
        "amp_veltrack" => { if let Some(v) = val.parse().ok() { r.amp_veltrack = v; } }
        "ampeg_release" => { if let Some(v) = val.parse().ok() { r.ampeg_release = v; } }
        "volume" => { if let Some(v) = val.parse().ok() { r.volume = v; } }
        "trigger" => { if let Some(v) = parse_trigger(val) { r.trigger = v; } }
        "rt_decay" => { if let Some(v) = val.parse().ok() { r.rt_decay = v; } }
        "lorand" => { if let Some(v) = val.parse().ok() { r.lorand = v; } }
        "hirand" => { if let Some(v) = val.parse().ok() { r.hirand = v; } }
        "on_locc64" => r.on_locc64 = val.parse().ok(),
        "on_hicc64" => r.on_hicc64 = val.parse().ok(),
        "group" => r.group = val.parse().ok(),
        "off_by" => r.off_by = val.parse().ok(),
        "pitch_keytrack" => { if let Some(v) = val.parse().ok() { r.pitch_keytrack = v; } }
        "pan" => { if let Ok(v) = val.parse() { r.pan = v; } }
        "note_polyphony" => r.note_polyphony = val.parse().ok(),
        _ => {}
    }
}

fn parse_trigger(val: &str) -> Option<Trigger> {
    match val {
        "attack" => Some(Trigger::Attack),
        "release" => Some(Trigger::Release),
        _ => None,
    }
}

/// Parse a key that is either a MIDI note number (integer) or a note name
/// like "C4", "D#1", "Bb3".
fn parse_key(val: &str) -> Option<u8> {
    if let Ok(n) = val.parse::<i32>() {
        if n >= 0 && n <= 127 { return Some(n as u8); }
        return None;
    }
    let val = val.trim();
    if val.is_empty() { return None; }
    let mut chars = val.chars().peekable();
    let note_char = chars.next()?.to_ascii_uppercase();
    let base: i32 = match note_char {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let accidental = match chars.peek() {
        Some('#') => { chars.next(); 1 }
        Some('b') => { chars.next(); -1 }
        _ => 0,
    };
    let octave_str: String = chars.collect();
    let octave: i32 = octave_str.trim().parse().ok()?;
    let midi = (octave + 1) * 12 + base + accidental;
    if midi >= 0 && midi <= 127 { Some(midi as u8) } else { None }
}
