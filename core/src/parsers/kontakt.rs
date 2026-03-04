/// Kontakt 2 (.nki / .nkm) instrument parser.
///
/// The binary file contains a short fixed header followed by a zlib-compressed
/// UTF-8 XML payload.  The XML root is either:
///
///   <K2_Container type="single_program">   — .nki (one K2_Program)
///   <K2_Bank>                              — .nkm (one or more K2_Container blocks)
///
/// Each K2_Program contains:
///   <Groups>  <K2_Group releaseTrigger="yes|no" volume="…">
///               <Envelope type="ahdsr" …>  <V name="release" value="…"/>
///   <Zones>   <K2_Zone groupIdx="N">
///               <V name="lowKey" …/>  <V name="highKey" …/>  <V name="rootKey" …/>
///               <V name="lowVelocity" …/>  <V name="highVelocity" …/>
///               <V name="zoneVolume" …/>
///               <Sample>  <V name="file_ex2" value="@d<len><dir>…F-<10flags><file>"/>
///
/// Sample paths are relative to the .nki/.nkm file's parent directory.

use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use std::io::Read;

use crate::region::{Region, Trigger};

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn parse_kontakt(path: &Path) -> Result<Vec<Region>, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("Cannot read {:?}: {}", path, e))?;

    // Locate zlib magic bytes 0x78 0x9C.
    let zlib_pos = data
        .windows(2)
        .position(|w| w[0] == 0x78 && w[1] == 0x9c)
        .ok_or_else(|| format!("No zlib data in {:?}", path))?;

    let mut decoder = ZlibDecoder::new(&data[zlib_pos..]);
    let mut xml = String::new();
    decoder
        .read_to_string(&mut xml)
        .map_err(|e| format!("Decompression failed for {:?}: {}", path, e))?;

    let base_dir = path.parent().unwrap_or(Path::new("."));
    parse_programs(&xml, base_dir)
}

// ─── XML-level program parser ─────────────────────────────────────────────────

struct GroupInfo {
    release_trigger: bool,
    /// Combined program + group volume in dB.
    vol_db: f32,
    /// Amplitude envelope release time in seconds.
    ampeg_release: f32,
}

fn parse_programs(xml: &str, base_dir: &Path) -> Result<Vec<Region>, String> {
    let mut all_regions = Vec::new();
    let mut pos = 0;

    while let Some((ps, pe)) = find_block(xml, "K2_Program", pos) {
        let prog = &xml[ps..pe];
        let prog_vol_db = linear_to_db(v_f32(prog, "volume").unwrap_or(1.0));

        // ── Groups ──────────────────────────────────────────────────────────
        let mut groups: Vec<GroupInfo> = Vec::new();
        let mut gpos = 0;
        while let Some((gs, ge)) = find_block(prog, "K2_Group", gpos) {
            let grp = &prog[gs..ge];
            let release_trigger = v_str(grp, "releaseTrigger") == Some("yes");
            let vol_db = prog_vol_db + linear_to_db(v_f32(grp, "volume").unwrap_or(1.0));
            let ampeg_release = find_block(grp, "Envelope", 0)
                .and_then(|(es, ee)| v_f32(&grp[es..ee], "release"))
                .unwrap_or(if release_trigger { 0.5 } else { 1.0 });
            groups.push(GroupInfo { release_trigger, vol_db, ampeg_release });
            gpos = ge;
        }

        // ── Zones ────────────────────────────────────────────────────────────
        let mut zpos = 0;
        while let Some((zs, ze)) = find_block(prog, "K2_Zone", zpos) {
            let zone = &prog[zs..ze];
            let group_idx = tag_attr_usize(zone, "K2_Zone", "groupIdx").unwrap_or(0);
            let group = match groups.get(group_idx) {
                Some(g) => g,
                None => {
                    eprintln!("Warning: K2_Zone groupIdx={} out of range, skipping", group_idx);
                    zpos = ze;
                    continue;
                }
            };

            let lokey = v_u8(zone, "lowKey").unwrap_or(0);
            let hikey = v_u8(zone, "highKey").unwrap_or(127);
            let rootkey = v_u8(zone, "rootKey").unwrap_or(lokey);
            // Kontakt velocity range is 1-based; convert to 0-based.
            let lovel = v_u8(zone, "lowVelocity").unwrap_or(1).saturating_sub(1);
            let hivel = v_u8(zone, "highVelocity").unwrap_or(127);
            let vol_db = group.vol_db + linear_to_db(v_f32(zone, "zoneVolume").unwrap_or(1.0));

            let sample_path = match find_block(zone, "Sample", 0)
                .and_then(|(ss, se)| v_str(&zone[ss..se], "file_ex2"))
                .map(|enc| decode_path(enc, base_dir))
            {
                Some(p) => p,
                None => {
                    eprintln!("Warning: K2_Zone missing file_ex2, skipping");
                    zpos = ze;
                    continue;
                }
            };

            all_regions.push(Region {
                sample: sample_path,
                lokey,
                hikey,
                lovel,
                hivel,
                pitch_keycenter: rootkey,
                amp_veltrack: 0.0,
                ampeg_release: group.ampeg_release,
                volume: vol_db,
                trigger: if group.release_trigger { Trigger::Release } else { Trigger::Attack },
                ..Region::default()
            });

            zpos = ze;
        }

        pos = pe;
    }

    if all_regions.is_empty() {
        Err("No zones found in instrument".to_string())
    } else {
        Ok(all_regions)
    }
}

// ─── XML helpers ──────────────────────────────────────────────────────────────

/// Find the byte range `[start, end)` of the first `<tag …>…</tag>` block
/// starting at or after `from`.  Skips any tag that shares only a prefix
/// (e.g. searching for "K2_Zone" won't match "K2_ZoneGroup").
fn find_block(xml: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let open_prefix = format!("<{}", tag);
    let close_tag = format!("</{}>", tag);
    let mut search_from = from;

    loop {
        let rel = xml[search_from..].find(&open_prefix)?;
        let abs = search_from + rel;

        // Verify the character after the tag name is a delimiter, not part of a longer name.
        let after = abs + open_prefix.len();
        if after < xml.len() {
            let b = xml.as_bytes()[after];
            if b != b' ' && b != b'>' && b != b'/' && b != b'\n' && b != b'\r' {
                search_from = abs + 1;
                continue;
            }
        }

        let close_rel = xml[abs..].find(&close_tag)?;
        return Some((abs, abs + close_rel + close_tag.len()));
    }
}

/// Extract the value of `<V name="NAME" value="VALUE"/>` within `xml`.
fn v_str<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("name=\"{}\" value=\"", name);
    let pos = xml.find(&needle)?;
    let after = pos + needle.len();
    let end = xml[after..].find('"')?;
    Some(&xml[after..after + end])
}

fn v_f32(xml: &str, name: &str) -> Option<f32> {
    v_str(xml, name)?.parse().ok()
}

fn v_u8(xml: &str, name: &str) -> Option<u8> {
    v_str(xml, name)?
        .parse::<u32>()
        .ok()
        .map(|v| v.min(255) as u8)
}

/// Extract a numeric XML attribute from the opening tag of a specific element.
/// Searches for `<TAG … ATTR="VALUE"` within the first tag in `xml`.
fn tag_attr_usize(xml: &str, tag: &str, attr: &str) -> Option<usize> {
    let open = format!("<{}", tag);
    let start = xml.find(&open)?;
    let end = xml[start..].find('>')?;
    let tag_str = &xml[start..start + end];
    let needle = format!("{}=\"", attr);
    let pos = tag_str.find(&needle)?;
    let after = pos + needle.len();
    let end_quote = tag_str[after..].find('"')?;
    tag_str[after..after + end_quote].parse().ok()
}

// ─── Path decoding ────────────────────────────────────────────────────────────

/// Decode a Kontakt `file_ex2` encoded path into an absolute `PathBuf`.
///
/// Format:  `@`  ( `d` <3-digit-decimal-len> <dirname> )*  `F-` <10-char-flags> <filename>
///
/// Examples:
///   `@d022Blanchet-Jeu 1 SamplesF-000101000009-G#0.wav`
///     → base_dir / "Blanchet-Jeu 1 Samples" / "09-G#0.wav"
///
///   `@d022Blanchet-Jeu 2 SamplesF-0001013000II-09-G#0.wav`
///     → base_dir / "Blanchet-Jeu 2 Samples" / "II-09-G#0.wav"
fn decode_path(encoded: &str, base_dir: &Path) -> PathBuf {
    let mut path = base_dir.to_path_buf();
    let s = encoded.strip_prefix('@').unwrap_or(encoded);
    let mut i = 0;

    while i < s.len() {
        let b = s.as_bytes()[i];
        if b == b'd' {
            // Directory component: d<3-digit-len><name>
            i += 1;
            if i + 3 > s.len() {
                break;
            }
            let len: usize = match s[i..i + 3].parse() {
                Ok(n) => n,
                Err(_) => break,
            };
            i += 3;
            if i + len > s.len() {
                break;
            }
            path.push(&s[i..i + len]);
            i += len;
        } else if s[i..].starts_with("F-") {
            // File component: F-<10-char-flags><filename>
            i += 2 + 10; // skip "F-" and the 10 flag characters
            path.push(&s[i..]);
            break;
        } else {
            break;
        }
    }

    path
}

// ─── Volume conversion ────────────────────────────────────────────────────────

fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        -144.0
    } else {
        20.0 * linear.log10()
    }
}
