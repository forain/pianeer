use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use crate::instruments::Instrument;
use crate::region::Region;
use crate::sampler::LoadedSample;

pub struct LoadedInstrument {
    pub regions: Vec<Region>,
    pub samples: Vec<Option<LoadedSample>>,
    pub cc_defaults: [u8; 128],
    pub sw_lokey: u8,
    pub sw_hikey: u8,
    pub sw_default: Option<u8>,
}

/// Handle for an in-progress async instrument load.
/// Drop to detach the loader thread (it finishes in the background).
pub struct LoadingJob {
    pub progress: Arc<AtomicU32>, // 0..=100
    result: Arc<Mutex<Option<Result<LoadedInstrument, String>>>>,
    _thread: thread::JoinHandle<()>,
}

impl LoadingJob {
    /// Spawn a background thread to load `inst`, returning a handle to track progress.
    pub fn start(inst: Instrument) -> Self {
        let progress = Arc::new(AtomicU32::new(0));
        let result: Arc<Mutex<Option<Result<LoadedInstrument, String>>>> =
            Arc::new(Mutex::new(None));
        let p = Arc::clone(&progress);
        let r = Arc::clone(&result);
        let _thread = thread::spawn(move || {
            let res = load_instrument_data(&inst, &p);
            *r.lock().unwrap() = Some(res);
        });
        LoadingJob { progress, result, _thread }
    }

    /// Current loading progress, 0..=100.
    pub fn pct(&self) -> u8 {
        self.progress.load(Ordering::Relaxed).min(100) as u8
    }

    /// Returns `Some(result)` once loading is complete, `None` if still in progress.
    pub fn try_take(&self) -> Option<Result<LoadedInstrument, String>> {
        self.result.lock().ok()?.take()
    }
}

/// Fix a malformed WAV file where WAVE_FORMAT_PCM (fmt type 0x0001) has a
/// non-16-byte fmt chunk. Symphonia rejects these; we trim the extra bytes
/// and patch the sizes in place. No-op for non-WAV or already-valid files.
fn fix_wav_fmt(data: &mut Vec<u8>) {
    if data.len() < 28 { return; }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" { return; }
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let chunk_size = u32::from_le_bytes([data[pos+4], data[pos+5], data[pos+6], data[pos+7]]) as usize;
        if &data[pos..pos+4] == b"fmt " {
            if chunk_size > 16 && pos + 8 + chunk_size <= data.len() {
                let format = u16::from_le_bytes([data[pos+8], data[pos+9]]);
                if format == 0x0001 {
                    // Remove bytes beyond the standard 16-byte PCM fmt body.
                    let remove_start = pos + 8 + 16;
                    let remove_end = (pos + 8 + chunk_size + (chunk_size & 1)).min(data.len());
                    data.drain(remove_start..remove_end);
                    data[pos+4..pos+8].copy_from_slice(&16u32.to_le_bytes());
                    let riff_size = (data.len() - 8) as u32;
                    data[4..8].copy_from_slice(&riff_size.to_le_bytes());
                }
            }
            return;
        }
        if chunk_size == 0 { break; }
        pos += 8 + chunk_size + (chunk_size & 1);
    }
}

/// Wrapper so a `Cursor<Vec<u8>>` can be used as a Symphonia `MediaSource`.
struct ByteSource(std::io::Cursor<Vec<u8>>);

impl std::io::Read for ByteSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.0.read(buf) }
}
impl std::io::Seek for ByteSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> { self.0.seek(pos) }
}
impl symphonia::core::io::MediaSource for ByteSource {
    fn is_seekable(&self) -> bool { true }
    fn byte_len(&self) -> Option<u64> { Some(self.0.get_ref().len() as u64) }
}

/// Persist the last-loaded instrument path so the next startup can restore it.
pub fn save_last_instrument_path(state_file: &std::path::Path, inst_path: &std::path::Path) {
    let _ = std::fs::write(state_file, inst_path.to_string_lossy().as_bytes());
}

/// Read the persisted instrument path. Returns `None` if the file doesn't exist or the
/// path it contains no longer exists on disk.
pub fn last_instrument_path(state_file: &std::path::Path) -> Option<std::path::PathBuf> {
    let s = std::fs::read_to_string(state_file).ok()?;
    let p = std::path::PathBuf::from(s.trim());
    if p.exists() { Some(p) } else { None }
}

/// Quickly parse the instrument definition to find the first sample path, then probe
/// that file for its sample rate.  Much faster than a full `load_instrument_data`.
/// Returns `None` for GIG instruments (where PCM is embedded in the file) or on errors.
pub fn probe_instrument_rate(inst: &Instrument) -> Option<u32> {
    let ext = inst.path().extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let first_sample = match ext.as_str() {
        "sfz" => crate::parsers::sfz::parse_sfz(inst.path()).ok()
            .and_then(|(r, _)| r.into_iter().find(|r| !r.sample.as_os_str().is_empty()))
            .map(|r| r.sample),
        "organ" => crate::parsers::organ::parse_organ(inst.path()).ok()
            .and_then(|r| r.into_iter().find(|r| !r.sample.as_os_str().is_empty()))
            .map(|r| r.sample),
        "nki" | "nkm" => crate::parsers::kontakt::parse_kontakt(inst.path()).ok()
            .and_then(|r| r.into_iter().find(|r| !r.sample.as_os_str().is_empty()))
            .map(|r| r.sample),
        _ => None,
    }?;
    probe_sample_rate(&first_sample)
}

/// Open a file with Symphonia and read the codec sample rate without decoding audio.
pub fn probe_sample_rate(path: &Path) -> Option<u32> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mut raw = std::fs::read(path).ok()?;
    if path.extension().and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("wav")).unwrap_or(false)
    {
        fix_wav_fmt(&mut raw);
    }
    let mss = MediaSourceStream::new(Box::new(ByteSource(std::io::Cursor::new(raw))), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    probed.format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .and_then(|t| t.codec_params.sample_rate)
}

fn load_sample(path: &Path, loop_start: Option<u64>, loop_end: Option<u64>) -> Result<LoadedSample, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mut raw = std::fs::read(path)
        .map_err(|e| format!("open {:?}: {}", path, e))?;
    if path.extension().and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("wav")).unwrap_or(false)
    {
        fix_wav_fmt(&mut raw);
    }
    let mss = MediaSourceStream::new(Box::new(ByteSource(std::io::Cursor::new(raw))), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("probe {:?}: {}", path, e))?;

    let mut format = probed.format;
    let track = format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| format!("no audio track in {:?}", path))?;

    let n_channels = track.codec_params.channels
        .map(|c| c.count())
        .unwrap_or(2);
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder {:?}: {}", path, e))?;

    let mut pcm: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(format!("packet {:?}: {}", path, e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode {:?}: {}", path, e)),
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        if n_channels == 2 {
            pcm.extend_from_slice(samples);
        } else {
            for &s in samples {
                pcm.push(s);
                pcm.push(s);
            }
        }
    }

    let frames = pcm.len() / 2;
    let pcm_i16: Vec<i16> = pcm.iter()
        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
        .collect();
    Ok(LoadedSample {
        data: std::sync::Arc::new(pcm_i16),
        frames,
        loop_start: loop_start.map(|v| v as usize),
        loop_end: loop_end.map(|v| v as usize),
    })
}

pub fn load_all_samples_parallel(regions: &[Region], progress: &Arc<AtomicU32>) -> Vec<Option<LoadedSample>> {
    use std::collections::HashMap;

    let total = regions.len();
    if total == 0 {
        progress.store(100, Ordering::Relaxed);
        return vec![];
    }

    // Deduplicate: build a list of unique (path, loop_start, loop_end) keys.
    // Multiple regions sharing the same file+loop will share one Arc<Vec<i16>>.
    let mut key_to_unique: HashMap<(std::path::PathBuf, Option<u64>, Option<u64>), usize> =
        HashMap::new();
    let mut unique_entries: Vec<(std::path::PathBuf, Option<u64>, Option<u64>)> = Vec::new();
    let mut region_to_unique: Vec<usize> = Vec::with_capacity(total);

    for r in regions {
        let key = (r.sample.clone(), r.loop_start, r.loop_end);
        let idx = *key_to_unique.entry(key.clone()).or_insert_with(|| {
            let i = unique_entries.len();
            unique_entries.push(key);
            i
        });
        region_to_unique.push(idx);
    }

    let n_unique = unique_entries.len();
    println!("Loading {} samples ({} unique files)...", total, n_unique);

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let chunk_size = (n_unique + num_threads - 1) / num_threads;
    let unique_entries = Arc::new(unique_entries);

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let entries = Arc::clone(&unique_entries);
            let counter = Arc::clone(&counter);
            let prog    = Arc::clone(progress);
            thread::spawn(move || {
                let start = t * chunk_size;
                let end = (start + chunk_size).min(n_unique);
                let mut results: Vec<(usize, Option<Arc<Vec<i16>>>, usize)> = Vec::new();
                for i in start..end {
                    let (ref path, loop_start, loop_end) = entries[i];
                    let (arc_data, frames) = match load_sample(path, loop_start, loop_end) {
                        Ok(s) => {
                            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == n_unique {
                                println!("  Loaded {}/{}", done, n_unique);
                            }
                            prog.store((done * 100 / n_unique) as u32, Ordering::Relaxed);
                            (Some(s.data), s.frames)
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to load {:?}: {}", path, e);
                            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            prog.store((done * 100 / n_unique) as u32, Ordering::Relaxed);
                            (None, 0)
                        }
                    };
                    results.push((i, arc_data, frames));
                }
                results
            })
        })
        .collect();

    // Collect results indexed by unique entry index.
    let mut unique_results: Vec<(Option<Arc<Vec<i16>>>, usize)> = vec![(None, 0); n_unique];
    for handle in handles {
        if let Ok(chunk) = handle.join() {
            for (i, arc_data, frames) in chunk {
                unique_results[i] = (arc_data, frames);
            }
        }
    }

    // Map each region back to its unique result, sharing the Arc.
    region_to_unique.into_iter().zip(regions.iter()).map(|(uid, r)| {
        let (ref arc_opt, frames) = unique_results[uid];
        arc_opt.as_ref().map(|arc| LoadedSample {
            data: Arc::clone(arc),
            frames,
            loop_start: r.loop_start.map(|v| v as usize),
            loop_end:   r.loop_end.map(|v| v as usize),
        })
    }).collect()
}

/// Poll an in-progress loading job once. On completion, swaps the instrument
/// into `state`, updates tune, and returns `(job_idx, new_current_path, auto_tune)`.
/// Sets `loading_job` to `None` when done (success or error).
pub fn poll_loading_job(
    loading_job: &mut Option<(usize, LoadingJob)>,
    state: &std::sync::Mutex<crate::sampler::SamplerState>,
    settings: &crate::types::Settings,
    menu: &[crate::types::MenuItem],
) -> Option<(usize, std::path::PathBuf, f32)> {
    let result = loading_job.as_ref().and_then(|(_, job)| job.try_take())?;
    let job_idx = loading_job.take().unwrap().0;
    match result {
        Ok(loaded) => {
            let auto_tune = state.lock().ok().map(|mut s| {
                s.swap_instrument(
                    loaded.regions, loaded.samples, loaded.cc_defaults,
                    loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
                );
                let at = s.detect_a4_tune_correction();
                s.master_tune_semitones = at + settings.tune.semitones();
                at
            }).unwrap_or(0.0);
            let current_path = menu.get(job_idx)
                .and_then(|m| if let crate::types::MenuItem::Instrument(i) = m {
                    Some(i.path().clone())
                } else { None })
                .unwrap_or_default();
            Some((job_idx, current_path, auto_tune))
        }
        Err(e) => {
            eprintln!("Error loading instrument: {}", e);
            None
        }
    }
}

pub fn load_instrument_data(inst: &Instrument, progress: &Arc<AtomicU32>) -> Result<LoadedInstrument, String> {
    let ext = inst.path().extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (regions, cc_defaults, sw_lokey, sw_hikey, sw_default) = match ext.as_str() {
        "sfz" => {
            println!("Parsing SFZ: {}", inst.path().display());
            let (regions, meta) = crate::parsers::sfz::parse_sfz(inst.path())
                .map_err(|e| format!("Error parsing SFZ: {}", e))?;
            (regions, meta.cc_defaults, meta.sw_lokey, meta.sw_hikey, meta.sw_default)
        }
        "organ" => {
            println!("Parsing Grand Orgue ODF: {}", inst.path().display());
            let regions = crate::parsers::organ::parse_organ(inst.path())
                .map_err(|e| format!("Error parsing ODF: {}", e))?;
            (regions, [0u8; 128], 0u8, 0u8, None)
        }
        "nki" | "nkm" => {
            println!("Parsing Kontakt instrument: {}", inst.path().display());
            let regions = crate::parsers::kontakt::parse_kontakt(inst.path())
                .map_err(|e| format!("Error parsing NKI/NKM: {}", e))?;
            (regions, [0u8; 128], 0u8, 0u8, None)
        }
        "gig" => {
            let (regions, samples) = crate::parsers::gig::parse_gig(inst.path())
                .map_err(|e| format!("Error parsing GIG: {}", e))?;
            println!("All samples loaded.");
            progress.store(100, Ordering::Relaxed);
            return Ok(LoadedInstrument {
                regions,
                samples,
                cc_defaults: [0u8; 128],
                sw_lokey: 0,
                sw_hikey: 0,
                sw_default: None,
            });
        }
        other => return Err(format!("Unknown file extension '{}'", other)),
    };
    println!("Parsed {} regions.", regions.len());
    let samples = load_all_samples_parallel(&regions, progress);
    println!("All samples loaded.");
    Ok(LoadedInstrument { regions, samples, cc_defaults, sw_lokey, sw_hikey, sw_default })
}
