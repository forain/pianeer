/// GIG (RIFF/DLS) binary format parser.
///
/// Parses attack and release regions from a .gig file, decoding embedded
/// 16-bit PCM samples to interleaved stereo f32.  Loop points are read from
/// the per-wave wsmp chunk.  Multiple regions that share the same pool entry
/// (velocity-zone compression in GIG) share the same underlying PCM buffer
/// via Arc, so the full wave pool is decoded only once.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::region::{Region, Trigger};
use crate::sampler::LoadedSample;

// ── Low-level integer helpers ─────────────────────────────────────────────────

#[inline] fn u16le(d: &[u8], o: usize) -> u16 { u16::from_le_bytes(d[o..o+2].try_into().unwrap()) }
#[inline] fn u32le(d: &[u8], o: usize) -> u32 { u32::from_le_bytes(d[o..o+4].try_into().unwrap()) }

// ── RIFF chunk iterator ───────────────────────────────────────────────────────

/// Iterate RIFF chunks in the byte range [start, end), yielding
/// (chunk_id [u8;4], data_start, data_size).
fn chunks_in(d: &[u8], start: usize, end: usize) -> impl Iterator<Item=([u8;4], usize, usize)> + '_ {
    let end = end.min(d.len());
    let mut pos = start;
    std::iter::from_fn(move || {
        if pos + 8 > end { return None; }
        let id: [u8; 4] = d[pos..pos+4].try_into().ok()?;
        let size = u32le(d, pos + 4) as usize;
        let data_start = pos + 8;
        if data_start + size > d.len() { return None; }
        pos += 8 + size + (size & 1); // advance, respecting 2-byte alignment
        Some((id, data_start, size))
    })
}

/// Find the first chunk with the given 4-byte ID. Returns (data_start, data_size).
fn find_chunk(d: &[u8], start: usize, end: usize, id: &[u8; 4]) -> Option<(usize, usize)> {
    chunks_in(d, start, end)
        .find(|(cid, _, _)| cid == id)
        .map(|(_, ds, sz)| (ds, sz))
}

/// Find the first LIST chunk with the given list-type.
/// Returns (children_start, children_size) — the span of the LIST's child chunks.
fn find_list(d: &[u8], start: usize, end: usize, list_type: &[u8; 4]) -> Option<(usize, usize)> {
    for (id, ds, sz) in chunks_in(d, start, end) {
        if &id == b"LIST" && sz >= 4 && &d[ds..ds+4] == list_type {
            return Some((ds + 4, sz - 4));
        }
    }
    None
}

/// Read the INAM string from a LIST(INFO) if one exists within the given span.
fn read_inam(d: &[u8], cs: usize, csz: usize) -> Option<String> {
    let (info_cs, info_sz) = find_list(d, cs, cs + csz, b"INFO")?;
    let (inam_ds, inam_sz) = find_chunk(d, info_cs, info_cs + info_sz, b"INAM")?;
    let bytes: Vec<u8> = d[inam_ds..inam_ds+inam_sz]
        .iter().take_while(|&&b| b != 0).cloned().collect();
    String::from_utf8(bytes).ok()
}

// ── Wave decoding ─────────────────────────────────────────────────────────────

struct WaveInfo {
    pcm: Arc<Vec<i16>>,
    frames: usize,
    loop_start: Option<usize>,
    loop_end: Option<usize>,
}

/// Decode one LIST(wave) starting at absolute offset `wave_pos` in `d`.
/// Returns the interleaved stereo i16 PCM and loop points (exclusive end).
fn decode_wave(d: &[u8], wave_pos: usize) -> Option<WaveInfo> {
    if wave_pos + 12 > d.len() { return None; }
    if &d[wave_pos..wave_pos+4] != b"LIST" { return None; }
    let wave_size = u32le(d, wave_pos + 4) as usize;
    if &d[wave_pos+8..wave_pos+12] != b"wave" { return None; }

    let children_start = wave_pos + 12;
    let children_end   = (wave_pos + 8 + wave_size).min(d.len());

    let mut channels:        u16  = 2;
    let mut bits_per_sample: u16  = 16;
    let mut pcm_start = 0usize;
    let mut pcm_size  = 0usize;
    let mut found_data = false;
    let mut loop_start: Option<usize> = None;
    let mut loop_end:   Option<usize> = None;

    for (id, ds, sz) in chunks_in(d, children_start, children_end) {
        match &id {
            b"fmt " if sz >= 16 => {
                // WAVEFORMATEX: format(u16), channels(u16), srate(u32),
                //               byterate(u32), blockalign(u16), bits(u16)
                channels        = u16le(d, ds + 2);
                bits_per_sample = u16le(d, ds + 14);
            }
            b"data" => {
                pcm_start  = ds;
                pcm_size   = sz;
                found_data = true;
            }
            b"wsmp" if sz >= 20 => {
                // cbSize(u32) UnityNote(u16) FineTune(i16) Gain(i32)
                // Options(u32) SampleLoops(u32)
                // SampleLoop[0] at offset cbSize:
                //   cbSize(u32) LoopType(u32) LoopStart(u32) LoopLength(u32)
                let cb    = u32le(d, ds) as usize;
                let nloops = u32le(d, ds + 16) as usize;
                if nloops > 0 && sz >= cb + 16 {
                    let lp = ds + cb;
                    if lp + 16 <= ds + sz {
                        let ls = u32le(d, lp + 8) as usize;
                        let ll = u32le(d, lp + 12) as usize;
                        if ll > 0 {
                            loop_start = Some(ls);
                            loop_end   = Some(ls + ll); // exclusive end
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if !found_data || bits_per_sample != 16 { return None; }

    let frame_bytes = channels as usize * 2;
    if frame_bytes == 0 { return None; }
    let n_frames = pcm_size / frame_bytes;

    let mut pcm: Vec<i16> = Vec::with_capacity(n_frames * 2);

    if channels == 2 {
        let n_samples = n_frames * 2;
        for i in 0..n_samples {
            let off = pcm_start + i * 2;
            if off + 2 > d.len() { break; }
            pcm.push(i16::from_le_bytes([d[off], d[off+1]]));
        }
    } else {
        // Mono → duplicate to stereo
        for i in 0..n_frames {
            let off = pcm_start + i * 2;
            if off + 2 > d.len() { break; }
            let s = i16::from_le_bytes([d[off], d[off+1]]);
            pcm.push(s);
            pcm.push(s);
        }
    }

    Some(WaveInfo { pcm: Arc::new(pcm), frames: n_frames, loop_start, loop_end })
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Return true if the first region in LIST(lrgn) has a RELEASETRIGGER (0x84)
/// dimension in its 3lnk chunk, indicating a combined attack+release instrument.
fn has_release_trigger_dim(d: &[u8], lrgn_cs: usize, lrgn_csz: usize) -> bool {
    for (rcid, rds, rsz) in chunks_in(d, lrgn_cs, lrgn_cs + lrgn_csz) {
        if &rcid != b"LIST" || rsz < 4 || &d[rds..rds+4] != b"rgn " { continue; }
        let (lnk_ds, lnk_sz) = match find_chunk(d, rds+4, rds+rsz, b"3lnk") {
            Some(x) => x,
            None    => return false,
        };
        if lnk_sz < 44 { return false; }
        for slot in 0..5usize {
            let off = lnk_ds + 4 + slot * 8;
            if d[off] == 0x84 && d[off+1] > 0 { return true; }
        }
        return false; // only inspect the first region
    }
    false
}

/// Parse a GIG file and return aligned (regions, samples) vectors.
///
/// Instrument selection:
///   - If the file contains a combined instrument (RELEASETRIGGER dimension,
///     encoding both attack and release within one instrument), the first such
///     instrument is loaded.  This typically gives more velocity detail than
///     the separate pure-attack / pure-release instruments in the same file.
///   - Otherwise the first pure-attack + first pure-release instruments are used.
///
/// The `sample` field of each Region is left empty (PathBuf::new()); the
/// loaded audio is returned in the parallel `samples` vector.
pub fn parse_gig(path: &Path) -> Result<(Vec<Region>, Vec<Option<LoadedSample>>), String> {
    let mb = std::fs::metadata(path).ok().map_or(0, |m| m.len()) / 1_048_576;
    println!("Reading GIG file ({} MB)...", mb);
    let d = std::fs::read(path).map_err(|e| format!("read {:?}: {}", path, e))?;

    if d.len() < 12 || &d[0..4] != b"RIFF" || &d[8..12] != b"DLS " {
        return Err("not a DLS/GIG file".to_string());
    }

    let file_end = d.len();

    // ── 1. Locate top-level chunks ────────────────────────────────────────────
    let ptbl = find_chunk(&d, 12, file_end, b"ptbl")
        .ok_or("no ptbl chunk")?;
    let (wvpl_cs, _) = find_list(&d, 12, file_end, b"wvpl")
        .ok_or("no wvpl chunk")?;
    let (lins_cs, lins_csz) = find_list(&d, 12, file_end, b"lins")
        .ok_or("no lins chunk")?;

    let wvpl_data_start = wvpl_cs;

    // ── 2. Parse ptbl: cbSize(u32) + n_pool(u32) + n_pool×offset(u32) ─────────
    let (ptbl_ds, ptbl_sz) = ptbl;
    if ptbl_sz < 8 { return Err("ptbl too small".to_string()); }
    let n_pool = u32le(&d, ptbl_ds + 4) as usize;
    if ptbl_sz < 8 + n_pool * 4 {
        return Err(format!("ptbl size mismatch: sz={} n_pool={}", ptbl_sz, n_pool));
    }
    let pool_table: Vec<usize> = (0..n_pool)
        .map(|i| u32le(&d, ptbl_ds + 8 + i * 4) as usize)
        .collect();

    // ── 3a. Classify instruments ──────────────────────────────────────────────
    struct InstInfo {
        lrgn_cs:      usize,
        lrgn_csz:     usize,
        is_combined:  bool,    // has RELEASETRIGGER dim → encodes attack + release
        pure_trigger: Trigger, // only meaningful when !is_combined
    }

    let mut infos: Vec<InstInfo> = Vec::new();
    for (ins_id, ins_ds, ins_sz) in chunks_in(&d, lins_cs, lins_cs + lins_csz) {
        if &ins_id != b"LIST" || ins_sz < 4 || &d[ins_ds..ins_ds+4] != b"ins " { continue; }
        let ins_cs   = ins_ds + 4;
        let ins_cend = ins_ds + ins_sz;
        let (lrgn_cs, lrgn_csz) = match find_list(&d, ins_cs, ins_cend, b"lrgn") {
            Some(x) => x, None => continue,
        };
        let is_combined = has_release_trigger_dim(&d, lrgn_cs, lrgn_csz);
        let pure_trigger = if is_combined {
            Trigger::Attack // placeholder — not used for combined instruments
        } else {
            match read_inam(&d, ins_cs, ins_sz.saturating_sub(4)) {
                Some(ref n) if n.to_lowercase().contains("release") => Trigger::Release,
                _ => Trigger::Attack,
            }
        };
        infos.push(InstInfo { lrgn_cs, lrgn_csz, is_combined, pure_trigger });
    }

    // ── 3b. Select instruments to parse ───────────────────────────────────────
    // Prefer the first combined instrument (provides matching attack + release
    // velocity layers, typically 8 each vs only 4 in the separate release inst).
    // Fall back to first pure-attack + first pure-release if none exists.
    let selected: Vec<usize> = {
        if let Some(i) = infos.iter().position(|x| x.is_combined) {
            vec![i]
        } else {
            let mut v = Vec::new();
            if let Some(i) = infos.iter().position(|x| !x.is_combined && x.pure_trigger == Trigger::Attack) {
                v.push(i);
            }
            if let Some(i) = infos.iter().position(|x| !x.is_combined && x.pure_trigger == Trigger::Release) {
                v.push(i);
            }
            v
        }
    };

    // ── 3c. Parse selected instruments ────────────────────────────────────────
    let mut regions: Vec<Region> = Vec::new();
    let mut samples: Vec<Option<LoadedSample>> = Vec::new();
    let mut wave_cache: HashMap<usize, Arc<WaveInfo>> = HashMap::new();

    for &inst_idx in &selected {
        let inst = &infos[inst_idx];

        for (rgn_id, rgn_ds, rgn_sz) in chunks_in(&d, inst.lrgn_cs, inst.lrgn_cs + inst.lrgn_csz) {
            if &rgn_id != b"LIST" || rgn_sz < 4 || &d[rgn_ds..rgn_ds+4] != b"rgn " { continue; }
            let rgn_cs   = rgn_ds + 4;
            let rgn_cend = rgn_ds + rgn_sz;

            // rgnh: KeyRange.lo(u16) KeyRange.hi(u16) ...
            let (rgnh_ds, rgnh_sz) = match find_chunk(&d, rgn_cs, rgn_cend, b"rgnh") {
                Some(x) => x, None => continue,
            };
            if rgnh_sz < 8 { continue; }
            let key_lo = u16le(&d, rgnh_ds)     as u8;
            let key_hi = u16le(&d, rgnh_ds + 2) as u8;

            // 3lnk: DimCount(u32) + 5×DimDef(8B) + 32×WPI(u32).
            // WPI starts at byte offset 44 from 3lnk data start.
            let (lnk_ds, lnk_sz) = match find_chunk(&d, rgn_cs, rgn_cend, b"3lnk") {
                Some(x) => x, None => continue,
            };
            if lnk_sz < 172 { continue; }

            let dim_count = u32le(&d, lnk_ds) as usize;
            if dim_count == 0 || dim_count > 32 { continue; }

            // Parse dimension bit-shifts.
            // Each slot contributes `bits` bits to the DimRegion index starting
            // at `cumulative`.  The dr_idx for (sc=0, rel=r, vel=v) is:
            //   r << rel_shift  |  v << vel_shift
            let mut cumulative: u32 = 0;
            let mut vel_shift:  u32 = 0;
            let mut vel_bits:   u32 = 0;
            let mut rel_shift:  u32 = 0;
            for slot in 0..5usize {
                let off      = lnk_ds + 4 + slot * 8;
                let dim_type = d[off];
                let bits     = d[off + 1] as u32;
                if bits == 0 { continue; }
                match dim_type {
                    0x84 => rel_shift = cumulative, // RELEASETRIGGER
                    0x82 => { vel_shift = cumulative; vel_bits = bits; } // VELOCITY
                    _ => {} // SAMPLECHANNEL and others consume bits but need no special handling
                }
                cumulative += bits;
            }

            let vel_zones = if vel_bits > 0 { 1usize << vel_bits } else { 1 };
            // For combined instruments iterate rel=0 (attack) then rel=1 (release).
            // For pure instruments rel_count=1 and rel_shift=0, so dr_base stays 0.
            let rel_count = if inst.is_combined { 2usize } else { 1 };
            let wpi_base  = lnk_ds + 44;

            // Collect VelocityUpperLimit for every DimRegion from 3ewa[dr_idx][+124].
            let mut vel_upper: Vec<u8> = vec![127u8; dim_count];
            if let Some((p3_cs, p3_csz)) = find_list(&d, rgn_cs, rgn_cend, b"3prg") {
                if let Some((ewl_cs, ewl_csz)) = find_list(&d, p3_cs, p3_cs + p3_csz, b"3ewl") {
                    let mut idx = 0usize;
                    for (ewa_id, ewa_ds, ewa_sz) in chunks_in(&d, ewl_cs, ewl_cs + ewl_csz) {
                        if &ewa_id == b"3ewa" {
                            if idx < dim_count && ewa_sz > 124 {
                                vel_upper[idx] = d[ewa_ds + 124];
                            }
                            idx += 1;
                            if idx >= dim_count { break; }
                        }
                    }
                }
            }

            // ── One Region per (rel_zone, vel_zone) — SC=0 side only ──────────
            for rel_zone in 0..rel_count {
                let trigger = if inst.is_combined {
                    if rel_zone == 0 { Trigger::Attack } else { Trigger::Release }
                } else {
                    inst.pure_trigger.clone()
                };

                let dr_base = rel_zone << rel_shift;
                let mut prev_vel_hi: u8 = 0;

                for vel_zone in 0..vel_zones {
                    // SC=0: bit 0 of dr_idx is 0, so sc contributes nothing.
                    let dr_idx = dr_base + (vel_zone << vel_shift);

                    if dr_idx >= 32 { break; }
                    let wpi_off = wpi_base + dr_idx * 4;
                    if wpi_off + 4 > d.len() { break; }

                    let pool_idx_raw = u32le(&d, wpi_off);
                    if pool_idx_raw == 0xFFFF_FFFF { continue; }
                    let pool_idx = pool_idx_raw as usize;
                    if pool_idx >= pool_table.len() { continue; }

                    let pool_offset = pool_table[pool_idx];

                    let vel_hi = if dr_idx < dim_count { vel_upper[dr_idx] } else { 127 };
                    let vel_lo = if vel_zone == 0 { 0u8 } else { prev_vel_hi.saturating_add(1) };
                    prev_vel_hi = vel_hi;

                    let wi = wave_cache.entry(pool_offset)
                        .or_insert_with(|| Arc::new(
                            decode_wave(&d, wvpl_data_start + pool_offset)
                                .unwrap_or(WaveInfo {
                                    pcm: Arc::new(Vec::new()),
                                    frames: 0,
                                    loop_start: None,
                                    loop_end: None,
                                })
                        ));

                    samples.push(Some(LoadedSample {
                        data: Arc::clone(&wi.pcm),
                        frames: wi.frames,
                        loop_start: wi.loop_start,
                        loop_end: wi.loop_end,
                    }));

                    regions.push(Region {
                        lokey: key_lo,
                        hikey: key_hi,
                        lovel: vel_lo,
                        hivel: vel_hi,
                        pitch_keycenter: key_lo,
                        trigger: trigger.clone(),
                        amp_veltrack: 100.0,
                        ampeg_release: match trigger {
                            Trigger::Attack  => 0.5,
                            Trigger::Release => 0.0,
                        },
                        ..Region::default()
                    });
                }
            }
        }
    }

    println!("GIG: {} regions, {} unique waves decoded.", regions.len(), wave_cache.len());
    Ok((regions, samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_maestro_gig() {
        let path = Path::new(
            "/run/media/forain/samsung970pro512/pianeer/samples/\
             MaestroGrandPiano/maestro_concert_grand_v2.gig"
        );
        if !path.exists() {
            eprintln!("GIG file not found, skipping test");
            return;
        }
        let (regions, samples) = parse_gig(path).expect("parse_gig failed");
        assert_eq!(regions.len(), samples.len(), "regions/samples length mismatch");
        assert!(!regions.is_empty(), "no regions parsed");

        // Every sample slot must be Some (we always push Some in the parser).
        let missing = samples.iter().filter(|s| s.is_none()).count();
        assert_eq!(missing, 0, "{} sample slots are None", missing);

        // Check attack and release regions exist.
        let n_attack  = regions.iter().filter(|r| r.trigger == crate::region::Trigger::Attack).count();
        let n_release = regions.iter().filter(|r| r.trigger == crate::region::Trigger::Release).count();
        println!("attack={} release={} total={}", n_attack, n_release, regions.len());
        assert!(n_attack  > 0, "no attack regions");
        assert!(n_release > 0, "no release regions");

        // Attack velocity ranges for key 21 (A0): first zone starts at 0, last ends at 127.
        let mut atk_zones: Vec<_> = regions.iter()
            .filter(|r| r.lokey == 21 && r.trigger == crate::region::Trigger::Attack)
            .collect();
        assert!(!atk_zones.is_empty(), "no attack regions for key 21");
        atk_zones.sort_by_key(|r| r.lovel);
        assert_eq!(atk_zones[0].lovel, 0, "attack vel zone should start at 0");
        assert_eq!(atk_zones.last().unwrap().hivel, 127, "attack vel zone should end at 127");

        // Release velocity ranges for key 21: same contiguity requirement.
        let mut rel_zones: Vec<_> = regions.iter()
            .filter(|r| r.lokey == 21 && r.trigger == crate::region::Trigger::Release)
            .collect();
        assert!(!rel_zones.is_empty(), "no release regions for key 21");
        rel_zones.sort_by_key(|r| r.lovel);
        assert_eq!(rel_zones[0].lovel, 0, "release vel zone should start at 0");
        assert_eq!(rel_zones.last().unwrap().hivel, 127, "release vel zone should end at 127");

        // No attack region should have a zero-frame sample (empty decode).
        let silent_atk = regions.iter().enumerate()
            .filter(|(_, r)| r.trigger == crate::region::Trigger::Attack)
            .filter(|(i, _)| samples[*i].as_ref().map_or(true, |s| s.frames == 0))
            .count();
        assert_eq!(silent_atk, 0, "{} attack regions have empty samples", silent_atk);
    }
}
