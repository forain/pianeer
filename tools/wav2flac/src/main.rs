use std::io::Write as _;
use std::path::{Path, PathBuf};

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use flacenc::source::MemSource;
use hound::{SampleFormat, WavReader};
use rayon::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let mut level: u32 = 5;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-l" | "--level" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--level requires a number 0-8");
                    std::process::exit(1);
                }
                level = args[i].parse().unwrap_or(5).min(8);
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {arg}");
                std::process::exit(1);
            }
            arg => dirs.push(PathBuf::from(arg)),
        }
        i += 1;
    }

    if dirs.is_empty() {
        eprintln!("No instrument directories specified.");
        std::process::exit(1);
    }

    let mut any_error = false;
    for dir in &dirs {
        if !dir.is_dir() {
            eprintln!("Not a directory: {}", dir.display());
            any_error = true;
            continue;
        }

        let src_name = dir.file_name().unwrap_or_default().to_string_lossy();
        let dst_name = format!("{src_name} (FLAC converted)");
        let dst = dir.parent().unwrap_or(Path::new(".")).join(&dst_name);

        println!("Source:      {}", dir.display());
        println!("Destination: {}", dst.display());

        match convert_instrument(dir, &dst, level) {
            Ok(stats) => {
                println!(
                    "Done: {} wav converted, {} files copied, {} metadata updated{}",
                    stats.converted,
                    stats.copied,
                    stats.metadata_updated,
                    if stats.binary_skipped > 0 {
                        format!(
                            ", {} binary metadata unchanged (paths not updatable)",
                            stats.binary_skipped
                        )
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => {
                eprintln!("Error: {e}");
                any_error = true;
            }
        }
        println!();
    }

    if any_error {
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!("wav2flac - Convert instrument sample library WAV files to FLAC");
    eprintln!();
    eprintln!("Usage: wav2flac [OPTIONS] <instrument_directory>...");
    eprintln!();
    eprintln!("Creates '<dir> (FLAC converted)/' next to the source.");
    eprintln!("All WAV files are converted to FLAC in parallel. Text metadata");
    eprintln!("files have .wav path references replaced with .flac. Source is never modified.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -l, --level N    FLAC compression level 0-8 (default: 5)");
    eprintln!("  -h, --help       Show this help");
    eprintln!();
    eprintln!("Metadata updated (.wav → .flac):  .sfz  .organ  .txt  .xml  .ini  .cfg");
    eprintln!("Binary metadata (copied as-is):   .nki  .nkm  .gig  (paths cannot be updated)");
    eprintln!("Input: 8/16/24/32-bit int PCM, 32-bit float PCM (float → 24-bit int)");
}

// ── Stats ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    converted: usize,
    copied: usize,
    metadata_updated: usize,
    binary_skipped: usize,
}

// ── Work items collected during the tree walk ─────────────────────────────────

struct WavJob {
    src: PathBuf,
    dst: PathBuf, // destination .flac path
    rel: PathBuf, // relative path for display
}

struct TextJob {
    src: PathBuf,
    dst: PathBuf,
    rel: PathBuf,
}

// ── Instrument conversion ─────────────────────────────────────────────────────

fn convert_instrument(
    src: &Path,
    dst: &Path,
    level: u32,
) -> Result<Stats, Box<dyn std::error::Error>> {
    if dst.exists() {
        return Err(format!("destination already exists: {}", dst.display()).into());
    }

    let mut stats = Stats::default();
    let mut wav_jobs: Vec<WavJob> = Vec::new();
    let mut text_jobs: Vec<TextJob> = Vec::new();

    // Phase 1: walk the tree — create directories, copy non-WAV files immediately,
    // collect WAV and text-metadata jobs for deferred processing.
    collect_work(src, src, dst, &mut stats, &mut wav_jobs, &mut text_jobs)?;

    // Phase 2: update text metadata files (fast, serial).
    for job in &text_jobs {
        match update_and_copy(&job.src, &job.dst) {
            Ok(true) => {
                println!("  [updated]  {}", job.rel.display());
                stats.metadata_updated += 1;
            }
            Ok(false) => {
                stats.copied += 1;
            }
            Err(e) => {
                eprintln!("  Warning: could not update {}: {e}", job.rel.display());
                std::fs::copy(&job.src, &job.dst)?;
                stats.copied += 1;
            }
        }
    }

    // Phase 3: convert WAV → FLAC in parallel using rayon.
    if !wav_jobs.is_empty() {
        println!("  Converting {} wav files...", wav_jobs.len());
        std::io::stdout().flush().ok();

        let results: Vec<(&WavJob, Result<(), String>)> = wav_jobs
            .par_iter()
            .map(|job| {
                let r = convert_wav(&job.src, &job.dst, level).map_err(|e| e.to_string());
                (job, r)
            })
            .collect();

        for (job, result) in &results {
            match result {
                Ok(()) => {
                    println!("  [wav→flac] {} ... ok", job.rel.display());
                    stats.converted += 1;
                }
                Err(e) => {
                    println!("  [wav→flac] {} ... FAILED: {e}", job.rel.display());
                }
            }
        }
    }

    Ok(stats)
}

/// Walk `src` recursively: create output directories, copy non-WAV files
/// immediately, and push WAV and text-metadata files onto their job queues.
fn collect_work(
    root: &Path,
    src: &Path,
    dst: &Path,
    stats: &mut Stats,
    wav_jobs: &mut Vec<WavJob>,
    text_jobs: &mut Vec<TextJob>,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dst)?;

    let mut entries: Vec<_> = std::fs::read_dir(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let src_path = entry.path();
        let fname = entry.file_name();
        let dst_path = dst.join(&fname);
        let rel = src_path.strip_prefix(root).unwrap_or(&src_path).to_path_buf();

        if src_path.is_dir() {
            collect_work(root, &src_path, &dst_path, stats, wav_jobs, text_jobs)?;
            continue;
        }
        if !src_path.is_file() {
            continue;
        }

        let ext_os = src_path.extension().map(|e| e.to_ascii_lowercase());
        let ext = ext_os.as_deref().and_then(|e| e.to_str()).unwrap_or("");

        match ext {
            "wav" => {
                let flac_dst = dst
                    .join(src_path.file_stem().unwrap_or_default())
                    .with_extension("flac");
                wav_jobs.push(WavJob { src: src_path, dst: flac_dst, rel });
            }
            "nki" | "nkm" | "gig" => {
                std::fs::copy(&src_path, &dst_path)?;
                stats.binary_skipped += 1;
            }
            ext if is_text_metadata(ext) => {
                text_jobs.push(TextJob { src: src_path, dst: dst_path, rel });
            }
            _ => {
                std::fs::copy(&src_path, &dst_path)?;
                stats.copied += 1;
            }
        }
    }
    Ok(())
}

fn is_text_metadata(ext: &str) -> bool {
    matches!(ext, "sfz" | "organ" | "txt" | "xml" | "ini" | "cfg" | "odf")
}

// ── Metadata text replacement ─────────────────────────────────────────────────

/// Read a text metadata file, replace .wav references, write to dst.
/// Returns true if the content changed.
fn update_and_copy(src: &Path, dst: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(src)?;
    let updated = replace_wav_refs(&content);
    let changed = updated != content;
    std::fs::write(dst, updated.as_bytes())?;
    Ok(changed)
}

/// Replace all `.wav` occurrences (case-insensitive, at extension boundaries) with `.flac`.
/// Also fixes SFZ-style `#define $VAR wav` bare-word extension macros.
fn replace_wav_refs(text: &str) -> String {
    // Pass 1: replace .wav → .flac
    let mut out = String::with_capacity(text.len() + 32);
    let mut remaining = text;
    loop {
        match find_dot_wav(remaining) {
            None => {
                out.push_str(remaining);
                break;
            }
            Some(pos) => {
                out.push_str(&remaining[..pos]);
                out.push_str(".flac");
                remaining = &remaining[pos + 4..];
            }
        }
    }
    // Pass 2: fix bare `#define $VAR wav` SFZ extension macros
    fix_sfz_defines(out)
}

/// Find the byte position of the next `.wav` extension (case-insensitive).
/// Only matches when followed by a non-identifier character or end of string.
fn find_dot_wav(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    for i in 0..b.len().saturating_sub(3) {
        if b[i] == b'.'
            && b[i + 1].to_ascii_lowercase() == b'w'
            && b[i + 2].to_ascii_lowercase() == b'a'
            && b[i + 3].to_ascii_lowercase() == b'v'
        {
            let after = b.get(i + 4).copied().unwrap_or(0);
            if !after.is_ascii_alphanumeric() && after != b'_' && after != b'-' {
                return Some(i);
            }
        }
    }
    None
}

/// On lines matching `#define <VAR> wav` (SFZ bare-word extension convention),
/// replace the `wav` value token with `flac`.
fn fix_sfz_defines(text: String) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&fix_define_line(line));
    }
    out
}

fn fix_define_line(line: &str) -> String {
    let (core, ending) = if line.ends_with("\r\n") {
        (&line[..line.len() - 2], "\r\n")
    } else if line.ends_with('\n') {
        (&line[..line.len() - 1], "\n")
    } else {
        (line, "")
    };

    let trimmed = core.trim();
    if !trimmed.to_ascii_lowercase().starts_with("#define") {
        return line.to_string();
    }

    // Match only the simple form: #define VARNAME wav
    let mut tokens = trimmed.split_whitespace();
    let _ = tokens.next(); // "#define"
    let _ = tokens.next(); // variable name
    if tokens.next().map(|v| v.to_ascii_lowercase()) != Some("wav".into()) {
        return line.to_string();
    }

    if let Some(pos) = find_last_word(core, "wav") {
        let mut result = core.to_string();
        result.replace_range(pos..pos + 3, "flac");
        result.push_str(ending);
        result
    } else {
        line.to_string()
    }
}

/// Find the byte position of the last word-boundary occurrence of `word`
/// (case-insensitive) in `text`.
fn find_last_word(text: &str, word: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let wlen = word.len();
    let mut result = None;
    for i in 0..b.len().saturating_sub(wlen - 1) {
        if &lower[i..i + wlen] == word {
            let before_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            let after_ok = i + wlen >= b.len() || !b[i + wlen].is_ascii_alphanumeric();
            if before_ok && after_ok {
                result = Some(i);
            }
        }
    }
    result
}

// ── WAV → FLAC conversion ─────────────────────────────────────────────────────

fn convert_wav(
    input: &Path,
    output: &Path,
    compression: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let reader = WavReader::open(input)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let sample_rate = spec.sample_rate;

    if channels > 8 {
        return Err(format!("FLAC supports max 8 channels, got {channels}").into());
    }

    let (samples, bits_per_sample): (Vec<i32>, usize) =
        match (spec.sample_format, spec.bits_per_sample) {
            (SampleFormat::Int, 8) => {
                let s = reader
                    .into_samples::<i8>()
                    .map(|s| s.map(|x| x as i32))
                    .collect::<Result<Vec<_>, _>>()?;
                (s, 8)
            }
            (SampleFormat::Int, 16) => {
                let s = reader
                    .into_samples::<i16>()
                    .map(|s| s.map(|x| x as i32))
                    .collect::<Result<Vec<_>, _>>()?;
                (s, 16)
            }
            (SampleFormat::Int, 24) | (SampleFormat::Int, 32) => {
                let bps = spec.bits_per_sample as usize;
                let s = reader
                    .into_samples::<i32>()
                    .collect::<Result<Vec<_>, _>>()?;
                (s, bps)
            }
            (SampleFormat::Float, 32) => {
                let s = reader
                    .into_samples::<f32>()
                    .map(|s| s.map(|x| (x.clamp(-1.0, 1.0) * 8_388_607.0) as i32))
                    .collect::<Result<Vec<_>, _>>()?;
                (s, 24)
            }
            (fmt, bps) => {
                return Err(format!("Unsupported format: {fmt:?} {bps}-bit").into());
            }
        };

    let mut config = flacenc::config::Encoder::default();
    config.block_size = block_size_for_level(compression);
    let config = config
        .into_verified()
        .map_err(|e| format!("encoder config error: {e:?}"))?;

    let source = MemSource::from_samples(&samples, channels, bits_per_sample, sample_rate as usize);
    let flac_stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| format!("encode error: {e:?}"))?;

    let mut sink = ByteSink::new();
    flac_stream
        .write(&mut sink)
        .map_err(|e| format!("write error: {e:?}"))?;

    std::fs::write(output, sink.as_slice())?;
    Ok(())
}

fn block_size_for_level(level: u32) -> usize {
    match level {
        0 => 1152,
        1 => 2304,
        2 => 2304,
        3 => 4096,
        4 => 4096,
        5 => 4096,
        6 => 8192,
        7 => 16384,
        _ => 65536,
    }
}
