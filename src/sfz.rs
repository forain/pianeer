/// SFZ parser — handles SFZ v2 with ARIA extensions as used by the Salamander Grand Piano.
///
/// Features supported:
///   #include (inline and line-start), #define macro expansion, default_path,
///   <global>/<master>/<group>/<region> hierarchy, tune, sw_last/sw_lokey/sw_hikey/sw_default,
///   locc$N/hicc$N, offset/offset_oncc$N, off_time, amplitude_oncc$N, pan_oncc$N,
///   amp_veltrack_oncc$N, ampeg_release_oncc$N, set_cc$N, set_hdcc$N.
use std::path::{Path, PathBuf};

pub use crate::region::{Region, Trigger};

/// Instrument-level metadata returned by the SFZ parser.
pub struct SfzMeta {
    /// Initial CC values from set_cc / set_hdcc directives in <control>.
    pub cc_defaults: [u8; 128],
    /// Keyswitch range low (inclusive). 0 if no keyswitch.
    pub sw_lokey: u8,
    /// Keyswitch range high (inclusive). 0 if no keyswitch (and sw_lokey==0 means disabled).
    pub sw_hikey: u8,
    /// Default active keyswitch note.
    pub sw_default: Option<u8>,
}

impl Default for SfzMeta {
    fn default() -> Self {
        SfzMeta { cc_defaults: [0u8; 128], sw_lokey: 0, sw_hikey: 0, sw_default: None }
    }
}

// ── GroupState ────────────────────────────────────────────────────────────────

/// Inheritable opcode state for <global> / <master> / <group>.
#[derive(Debug, Clone, Default)]
struct GroupState {
    amp_veltrack: Option<f32>,
    ampeg_release: Option<f32>,
    volume: Option<f32>,
    trigger: Option<Trigger>,
    rt_decay: Option<f32>,
    cc_trigger: Vec<(u8, u8, u8)>,
    group: Option<u32>,
    off_by: Option<u32>,
    pitch_keytrack: Option<f32>,
    pan: Option<f32>,
    lokey: Option<u8>,
    hikey: Option<u8>,
    lovel: Option<u8>,
    hivel: Option<u8>,
    note_polyphony: Option<u32>,
    // SFZ v2 / ARIA
    sw_last: Option<u8>,
    sw_lokey: Option<u8>,
    sw_hikey: Option<u8>,
    sw_default: Option<u8>,
    off_time: Option<f32>,
    amplitude_oncc: Vec<(u8, f32)>,
    pan_oncc: Vec<(u8, f32)>,
    amp_veltrack_oncc: Vec<(u8, f32)>,
    ampeg_release_oncc: Vec<(u8, f32)>,
    cc_conds: Vec<(u8, u8, u8)>,
    offset: Option<u64>,
    offset_oncc: Option<(u8, u64)>,
}

// ── #include expansion ────────────────────────────────────────────────────────

/// Expand all `#include "file"` directives in `content`, including those embedded
/// inline on the same line as other tokens (as used by the Salamander notes.txt).
/// Lines with `//` comments are checked so that #include after `//` is ignored.
fn expand_includes(content: &str, base_dir: &Path, depth: usize) -> Result<String, String> {
    if depth > 8 {
        return Err("SFZ #include depth limit exceeded".to_string());
    }
    let mut result = String::with_capacity(content.len() + 64);
    let mut pos = 0;

    while pos < content.len() {
        match content[pos..].find("#include") {
            None => {
                result.push_str(&content[pos..]);
                break;
            }
            Some(rel) => {
                let abs = pos + rel;

                // If the #include falls after a // comment on the same line, skip the line.
                let line_start = content[..abs].rfind('\n').map(|p| p + 1).unwrap_or(0);
                if content[line_start..abs].contains("//") {
                    let next_nl = content[abs..]
                        .find('\n')
                        .map(|p| abs + p + 1)
                        .unwrap_or(content.len());
                    result.push_str(&content[pos..next_nl]);
                    pos = next_nl;
                    continue;
                }

                result.push_str(&content[pos..abs]);
                let after_kw = &content[abs + "#include".len()..];
                let trimmed = after_kw.trim_start();
                let trim_off = after_kw.len() - trimmed.len();

                if trimmed.starts_with('"') {
                    if let Some(end_q) = trimmed[1..].find('"') {
                        let filename = &trimmed[1..end_q + 1];
                        let inc_path = base_dir.join(filename.replace('\\', "/"));
                        let inc_content = std::fs::read_to_string(&inc_path).map_err(|e| {
                            format!("Failed to read #include {:?}: {}", inc_path, e)
                        })?;
                        // ARIA SFZ: all includes are resolved relative to the root SFZ
                        // directory, not relative to the including file's own directory.
                        let expanded = expand_includes(&inc_content, base_dir, depth + 1)?;
                        // Surround with newlines so inline includes don't concatenate tokens.
                        result.push('\n');
                        result.push_str(&expanded);
                        result.push('\n');
                        pos = abs + "#include".len() + trim_off + 1 + end_q + 1;
                        continue;
                    }
                }
                // Unparseable #include — emit literally and advance past keyword.
                result.push_str("#include");
                pos = abs + "#include".len();
            }
        }
    }
    Ok(result)
}

// ── #define expansion ─────────────────────────────────────────────────────────

/// Parse a `#define $NAME VALUE` sequence at the start of `s`.
/// Returns `(name, value, bytes_consumed)` or `None` if not valid.
fn parse_define(s: &str) -> Option<(String, String, usize)> {
    let rest = s.strip_prefix("#define")?;
    let mut i = 0;
    let b = rest.as_bytes();
    // Skip horizontal whitespace.
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') { i += 1; }
    if i >= b.len() || b[i] != b'$' { return None; }
    let name_start = i;
    while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
    let name = rest[name_start..i].to_string();
    if name.len() <= 1 { return None; }
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') { i += 1; }
    let val_start = i;
    while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
    let value = rest[val_start..i].to_string();
    if value.is_empty() { return None; }
    Some((name, value, "#define".len() + i))
}

/// Expand `#define $NAME value` directives throughout the content.
///
/// Handles both line-leading and inline occurrences (e.g. `<region> #define $KEY 021 sample=…`).
/// Each `#define` takes effect from its position downward and may be redefined.
fn expand_defines(content: &str) -> String {
    let mut macros: Vec<(String, String)> = Vec::new();
    let mut result = String::with_capacity(content.len());

    for line in content.lines() {
        // Strip // comments.
        let line = if let Some(idx) = line.find("//") { &line[..idx] } else { line };

        // Scan the line for #define directives anywhere, collect them and build
        // the cleaned line (defines removed).
        let mut out = String::new();
        let mut pos = 0;
        loop {
            match line[pos..].find("#define") {
                None => { out.push_str(&line[pos..]); break; }
                Some(rel) => {
                    let abs = pos + rel;
                    out.push_str(&line[pos..abs]);
                    match parse_define(&line[abs..]) {
                        Some((name, value, consumed)) => {
                            if let Some(e) = macros.iter_mut().find(|(n, _)| n == &name) {
                                e.1 = value;
                            } else {
                                macros.push((name, value));
                            }
                            pos = abs + consumed;
                        }
                        None => {
                            out.push('#');
                            pos = abs + 1;
                        }
                    }
                }
            }
        }

        // Apply current macro map to the cleaned line.
        // Sort by name length descending so longer names match before shorter prefixes.
        if macros.is_empty() {
            result.push_str(&out);
        } else {
            let mut sorted: Vec<_> = macros.iter().collect();
            sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            let mut processed = out;
            for (name, value) in sorted {
                processed = processed.replace(name.as_str(), value.as_str());
            }
            result.push_str(&processed);
        }
        result.push('\n');
    }
    result
}

// ── Public parse entry point ──────────────────────────────────────────────────

pub fn parse_sfz(sfz_path: &Path) -> Result<(Vec<Region>, SfzMeta), String> {
    let raw = std::fs::read_to_string(sfz_path)
        .map_err(|e| format!("Failed to read SFZ: {}", e))?;

    let base_dir = sfz_path.parent().unwrap_or(Path::new("."));

    let included = expand_includes(&raw, base_dir, 0)
        .map_err(|e| format!("SFZ include error: {}", e))?;
    let content = expand_defines(&included);

    let mut regions = Vec::new();
    let mut global = GroupState::default();
    let mut master = GroupState::default();
    let mut group = GroupState::default();
    let mut in_region = false;
    let mut in_global = false;
    let mut in_master = false;
    let mut in_control = false;
    let mut current_region = Region::default();
    let mut default_path = PathBuf::new();
    let mut cc_defaults = [0u8; 128];
    // Keyswitch metadata is instrument-level and must survive <global> resets.
    let mut sw_lokey: Option<u8> = None;
    let mut sw_hikey: Option<u8> = None;
    let mut sw_default: Option<u8> = None;

    for line in content.lines() {
        // Strip line comments.
        let line = if let Some(idx) = line.find("//") { &line[..idx] } else { line };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Tokenise: handle <header> and opcode=value tokens.
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
                        // Preserve sw metadata before resetting (may be set in any global).
                        if global.sw_lokey.is_some() { sw_lokey = global.sw_lokey; }
                        if global.sw_hikey.is_some() { sw_hikey = global.sw_hikey; }
                        if global.sw_default.is_some() { sw_default = global.sw_default; }
                        global = GroupState::default();
                        master = GroupState::default();
                        in_global = true;
                        in_master = false;
                        in_control = false;
                    }
                    "master" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        // master inherits global; opcodes between <master> and <group> go here.
                        master = global.clone();
                        in_master = true;
                        in_global = false;
                        in_control = false;
                    }
                    "group" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        // group inherits from master (which inherits from global).
                        group = master.clone();
                        in_master = false;
                        in_global = false;
                        in_control = false;
                    }
                    "region" => {
                        if in_region {
                            regions.push(current_region.clone());
                        }
                        current_region = region_from_group(&group);
                        in_region = true;
                        in_master = false;
                        in_global = false;
                        in_control = false;
                    }
                    "control" => {
                        if in_region {
                            regions.push(current_region.clone());
                            in_region = false;
                        }
                        in_control = true;
                        in_master = false;
                        in_global = false;
                    }
                    _ => {}
                }
            } else if let Some(eq_pos) = token.find('=') {
                let key = &token[..eq_pos];
                let val = &token[eq_pos + 1..];

                if in_control {
                    apply_opcode_to_control(key, val, &mut default_path, &mut cc_defaults);
                } else if in_region {
                    apply_opcode_to_region(&mut current_region, key, val, base_dir, &default_path);
                } else if in_global {
                    apply_opcode_to_group(&mut global, key, val);
                } else if in_master {
                    apply_opcode_to_group(&mut master, key, val);
                } else {
                    apply_opcode_to_group(&mut group, key, val);
                }
            }
        }
    }

    if in_region {
        regions.push(current_region);
    }

    // Flush sw metadata from the final global section.
    if global.sw_lokey.is_some() { sw_lokey = global.sw_lokey; }
    if global.sw_hikey.is_some() { sw_hikey = global.sw_hikey; }
    if global.sw_default.is_some() { sw_default = global.sw_default; }

    let meta = SfzMeta {
        cc_defaults,
        sw_lokey: sw_lokey.unwrap_or(0),
        sw_hikey: sw_hikey.unwrap_or(0),
        sw_default,
    };

    Ok((regions, meta))
}

// ── <control> opcode handler ──────────────────────────────────────────────────

fn apply_opcode_to_control(
    key: &str,
    val: &str,
    default_path: &mut PathBuf,
    cc_defaults: &mut [u8; 128],
) {
    if key == "default_path" {
        *default_path = PathBuf::from(val.replace('\\', "/"));
    } else if let Some(cc_str) = key.strip_prefix("set_hdcc") {
        if let (Ok(cc_num), Ok(v)) = (cc_str.parse::<usize>(), val.parse::<f32>()) {
            if cc_num < 128 {
                cc_defaults[cc_num] = (v * 127.0).round().clamp(0.0, 127.0) as u8;
            }
        }
    } else if let Some(cc_str) = key.strip_prefix("set_cc") {
        if let (Ok(cc_num), Ok(v)) = (cc_str.parse::<usize>(), val.parse::<u8>()) {
            if cc_num < 128 {
                cc_defaults[cc_num] = v;
            }
        }
    }
    // label_cc, set_hd*cc (ARIA half-double variants) and other control opcodes are cosmetic.
}

// ── region_from_group ─────────────────────────────────────────────────────────

fn region_from_group(g: &GroupState) -> Region {
    let mut r = Region::default();
    if let Some(v) = g.amp_veltrack { r.amp_veltrack = v; }
    if let Some(v) = g.ampeg_release { r.ampeg_release = v; }
    if let Some(v) = g.volume { r.volume = v; }
    if let Some(ref v) = g.trigger { r.trigger = v.clone(); }
    if let Some(v) = g.rt_decay { r.rt_decay = v; }
    r.cc_trigger = g.cc_trigger.clone();
    r.group = g.group;
    r.off_by = g.off_by;
    if let Some(v) = g.pitch_keytrack { r.pitch_keytrack = v; }
    if let Some(v) = g.pan { r.pan = v; }
    if let Some(v) = g.lokey { r.lokey = v; }
    if let Some(v) = g.hikey { r.hikey = v; }
    if let Some(v) = g.lovel { r.lovel = v; }
    if let Some(v) = g.hivel { r.hivel = v; }
    r.note_polyphony = g.note_polyphony;
    r.sw_last = g.sw_last;
    if let Some(v) = g.off_time { r.off_time = v; }
    if let Some(v) = g.offset { r.offset = v; }
    r.offset_oncc = g.offset_oncc;
    r.amplitude_oncc = g.amplitude_oncc.clone();
    r.pan_oncc = g.pan_oncc.clone();
    r.amp_veltrack_oncc = g.amp_veltrack_oncc.clone();
    r.ampeg_release_oncc = g.ampeg_release_oncc.clone();
    r.cc_conds = g.cc_conds.clone();
    r
}

// ── apply_opcode_to_group ─────────────────────────────────────────────────────

fn apply_opcode_to_group(g: &mut GroupState, key: &str, val: &str) {
    match key {
        "amp_veltrack" => g.amp_veltrack = val.parse().ok(),
        "ampeg_release" => g.ampeg_release = val.parse().ok(),
        "volume" => g.volume = val.parse().ok(),
        "trigger" => g.trigger = parse_trigger(val),
        "rt_decay" => g.rt_decay = val.parse().ok(),
        "group" => g.group = val.parse().ok(),
        "off_by" => g.off_by = val.parse().ok(),
        "pitch_keytrack" => g.pitch_keytrack = val.parse().ok(),
        "pan" => g.pan = val.parse().ok(),
        "lokey" => g.lokey = parse_key(val),
        "hikey" => g.hikey = parse_key(val),
        "lovel" => g.lovel = val.parse().ok(),
        "hivel" => g.hivel = val.parse().ok(),
        "note_polyphony" => g.note_polyphony = val.parse().ok(),
        "sw_last" => g.sw_last = parse_key(val),
        "sw_lokey" => g.sw_lokey = parse_key(val),
        "sw_hikey" => g.sw_hikey = parse_key(val),
        "sw_default" => g.sw_default = parse_key(val),
        "off_time" => g.off_time = val.parse().ok(),
        "offset" => g.offset = val.parse().ok(),
        _ => {
            if let Some(v) = parse_oncc_val(key, val, "amplitude_oncc") {
                set_cc_mod(&mut g.amplitude_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "pan_oncc") {
                set_cc_mod(&mut g.pan_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "amp_veltrack_oncc") {
                set_cc_mod(&mut g.amp_veltrack_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "ampeg_release_oncc") {
                set_cc_mod(&mut g.ampeg_release_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "offset_oncc") {
                g.offset_oncc = Some((v.0, v.1 as u64));
            } else if !parse_on_cc_cond(key, val, &mut g.cc_trigger) {
                parse_cc_cond(key, val, &mut g.cc_conds);
            }
        }
    }
}

// ── apply_opcode_to_region ────────────────────────────────────────────────────

fn apply_opcode_to_region(
    r: &mut Region,
    key: &str,
    val: &str,
    base_dir: &Path,
    default_path: &Path,
) {
    match key {
        "sample" => {
            let normalized = val.replace('\\', "/");
            r.sample = base_dir.join(default_path).join(&normalized);
        }
        "lokey" => { if let Some(v) = parse_key(val) { r.lokey = v; } }
        "hikey" => { if let Some(v) = parse_key(val) { r.hikey = v; } }
        "key" => {
            // Shorthand: sets lokey = hikey = pitch_keycenter.
            if let Some(v) = parse_key(val) {
                r.lokey = v;
                r.hikey = v;
                r.pitch_keycenter = v;
            }
        }
        "lovel" => { if let Ok(v) = val.parse() { r.lovel = v; } }
        "hivel" => { if let Ok(v) = val.parse() { r.hivel = v; } }
        "pitch_keycenter" => { if let Some(v) = parse_key(val) { r.pitch_keycenter = v; } }
        "amp_veltrack" => { if let Ok(v) = val.parse() { r.amp_veltrack = v; } }
        "ampeg_release" => { if let Ok(v) = val.parse() { r.ampeg_release = v; } }
        "volume" => { if let Ok(v) = val.parse() { r.volume = v; } }
        "trigger" => { if let Some(v) = parse_trigger(val) { r.trigger = v; } }
        "rt_decay" => { if let Ok(v) = val.parse() { r.rt_decay = v; } }
        "lorand" => { if let Ok(v) = val.parse() { r.lorand = v; } }
        "hirand" => { if let Ok(v) = val.parse() { r.hirand = v; } }
        "group" => r.group = val.parse().ok(),
        "off_by" => r.off_by = val.parse().ok(),
        "pitch_keytrack" => { if let Ok(v) = val.parse() { r.pitch_keytrack = v; } }
        "pan" => { if let Ok(v) = val.parse() { r.pan = v; } }
        "tune" => { if let Ok(v) = val.parse() { r.tune_cents = v; } }
        "note_polyphony" => r.note_polyphony = val.parse().ok(),
        "sw_last" => r.sw_last = parse_key(val),
        "off_time" => { if let Ok(v) = val.parse() { r.off_time = v; } }
        "offset" => { if let Ok(v) = val.parse() { r.offset = v; } }
        _ => {
            if let Some(v) = parse_oncc_val(key, val, "amplitude_oncc") {
                set_cc_mod(&mut r.amplitude_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "pan_oncc") {
                set_cc_mod(&mut r.pan_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "amp_veltrack_oncc") {
                set_cc_mod(&mut r.amp_veltrack_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "ampeg_release_oncc") {
                set_cc_mod(&mut r.ampeg_release_oncc, v.0, v.1);
            } else if let Some(v) = parse_oncc_val(key, val, "offset_oncc") {
                r.offset_oncc = Some((v.0, v.1 as u64));
            } else if !parse_on_cc_cond(key, val, &mut r.cc_trigger) {
                parse_cc_cond(key, val, &mut r.cc_conds);
            }
        }
    }
}

// ── Helper parsers ────────────────────────────────────────────────────────────

/// Parse `prefix$N=value` style opcodes. Returns (cc_num, value) on success.
fn parse_oncc_val(key: &str, val: &str, prefix: &str) -> Option<(u8, f32)> {
    let cc_str = key.strip_prefix(prefix)?;
    let cc_num: u8 = cc_str.parse().ok()?;
    let v: f32 = val.parse().ok()?;
    Some((cc_num, v))
}

/// Parse `locc$N` / `hicc$N` and insert/update a condition in `conds`.
fn parse_cc_cond(key: &str, val: &str, conds: &mut Vec<(u8, u8, u8)>) {
    let (is_lo, cc_str) = if let Some(s) = key.strip_prefix("locc") {
        (true, s)
    } else if let Some(s) = key.strip_prefix("hicc") {
        (false, s)
    } else {
        return;
    };
    let cc_num: u8 = match cc_str.parse() { Ok(v) => v, Err(_) => return };
    let cc_val: u8 = match val.parse() { Ok(v) => v, Err(_) => return };

    if let Some(entry) = conds.iter_mut().find(|(c, _, _)| *c == cc_num) {
        if is_lo { entry.1 = cc_val; } else { entry.2 = cc_val; }
    } else if is_lo {
        conds.push((cc_num, cc_val, 127));
    } else {
        conds.push((cc_num, 0, cc_val));
    }
}

/// Parse `on_locc$N` / `on_hicc$N` and insert/update a condition in `cc_trigger`.
/// Returns true if the key was recognised (whether or not parsing succeeded).
fn parse_on_cc_cond(key: &str, val: &str, conds: &mut Vec<(u8, u8, u8)>) -> bool {
    let (is_lo, cc_str) = if let Some(s) = key.strip_prefix("on_locc") {
        (true, s)
    } else if let Some(s) = key.strip_prefix("on_hicc") {
        (false, s)
    } else {
        return false;
    };
    let cc_num: u8 = match cc_str.parse() { Ok(v) => v, Err(_) => return true };
    let cc_val: u8 = match val.parse() { Ok(v) => v, Err(_) => return true };

    if let Some(entry) = conds.iter_mut().find(|(c, _, _)| *c == cc_num) {
        if is_lo { entry.1 = cc_val; } else { entry.2 = cc_val; }
    } else if is_lo {
        conds.push((cc_num, cc_val, 127));
    } else {
        conds.push((cc_num, 0, cc_val));
    }
    true
}

/// Update or insert a (cc_num, value) entry in a CC-mod list.
fn set_cc_mod(list: &mut Vec<(u8, f32)>, cc: u8, val: f32) {
    if let Some(entry) = list.iter_mut().find(|(c, _)| *c == cc) {
        entry.1 = val;
    } else {
        list.push((cc, val));
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
        'C' => 0, 'D' => 2, 'E' => 4, 'F' => 5,
        'G' => 7, 'A' => 9, 'B' => 11, _ => return None,
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
