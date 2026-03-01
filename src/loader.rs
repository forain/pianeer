use std::path::Path;
use std::sync::Arc;
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
    Ok(LoadedSample {
        data: std::sync::Arc::new(pcm),
        frames,
        loop_start: loop_start.map(|v| v as usize),
        loop_end: loop_end.map(|v| v as usize),
    })
}

pub fn load_all_samples_parallel(regions: &[Region]) -> Vec<Option<LoadedSample>> {
    let total = regions.len();
    println!("Loading {} samples...", total);

    let entries: Vec<_> = regions.iter()
        .map(|r| (r.sample.clone(), r.loop_start, r.loop_end))
        .collect();

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let chunk_size = (total + num_threads - 1) / num_threads;
    let entries = Arc::new(entries);

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let entries = Arc::clone(&entries);
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                let start = t * chunk_size;
                let end = (start + chunk_size).min(total);
                let mut results: Vec<(usize, Option<LoadedSample>)> = Vec::new();
                for i in start..end {
                    let (ref path, loop_start, loop_end) = entries[i];
                    let loaded = match load_sample(path, loop_start, loop_end) {
                        Ok(s) => {
                            let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if done % 50 == 0 || done == total {
                                println!("  Loaded {}/{}", done, total);
                            }
                            Some(s)
                        }
                        Err(e) => {
                            eprintln!("Warning: failed to load {:?}: {}", path, e);
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            None
                        }
                    };
                    results.push((i, loaded));
                }
                results
            })
        })
        .collect();

    let mut all_results: Vec<(usize, Option<LoadedSample>)> = Vec::with_capacity(total);
    for handle in handles {
        if let Ok(chunk) = handle.join() {
            all_results.extend(chunk);
        }
    }

    all_results.sort_by_key(|(i, _)| *i);
    all_results.into_iter().map(|(_, s)| s).collect()
}

pub fn load_instrument_data(inst: &Instrument) -> Result<LoadedInstrument, String> {
    let ext = inst.path().extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (regions, cc_defaults, sw_lokey, sw_hikey, sw_default) = match ext.as_str() {
        "sfz" => {
            println!("Parsing SFZ: {}", inst.path().display());
            let (regions, meta) = crate::sfz::parse_sfz(inst.path())
                .map_err(|e| format!("Error parsing SFZ: {}", e))?;
            (regions, meta.cc_defaults, meta.sw_lokey, meta.sw_hikey, meta.sw_default)
        }
        "organ" => {
            println!("Parsing Grand Orgue ODF: {}", inst.path().display());
            let regions = crate::organ::parse_organ(inst.path())
                .map_err(|e| format!("Error parsing ODF: {}", e))?;
            (regions, [0u8; 128], 0u8, 0u8, None)
        }
        "nki" | "nkm" => {
            println!("Parsing Kontakt instrument: {}", inst.path().display());
            let regions = crate::kontakt::parse_kontakt(inst.path())
                .map_err(|e| format!("Error parsing NKI/NKM: {}", e))?;
            (regions, [0u8; 128], 0u8, 0u8, None)
        }
        "gig" => {
            let (regions, samples) = crate::gig::parse_gig(inst.path())
                .map_err(|e| format!("Error parsing GIG: {}", e))?;
            println!("All samples loaded.");
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
    let samples = load_all_samples_parallel(&regions);
    println!("All samples loaded.");
    Ok(LoadedInstrument { regions, samples, cc_defaults, sw_lokey, sw_hikey, sw_default })
}
