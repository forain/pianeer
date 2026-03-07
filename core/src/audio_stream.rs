//! FLAC audio streaming for the web UI.
//!
//! Architecture:
//!   JACK callback → lock-free ring buffer → encoder thread → broadcast channel
//!   Web handlers subscribe to the broadcast and forward to browser clients.
//!
//! Transport (browser chooses based on availability):
//!   Primary:  WebTransport unreliable datagrams  (~30–60 ms latency on LAN)
//!   Fallback: /audio-ws WebSocket binary messages (~80–150 ms, reliable)
//!
//! FLAC encoding uses a minimal FIXED-2 predictor + Rice residual coder.
//! Block size = 256 samples (5.3 ms at 48 kHz) fits comfortably in one datagram.

use std::sync::Arc;
use ringbuf::HeapConsumer;
use tokio::sync::broadcast;

// ── Public constants ────────────────────────────────────────────────────────

/// Samples per FLAC/PCM block (stereo frames). Smaller = lower latency.
/// 64 frames = 1.3 ms at 48 kHz.
pub const BLOCK_SAMPLES: usize = 64;

const CHANNELS: usize = 2;
const BPS: u8 = 16; // bits per sample sent to browser

// ── Public handle ───────────────────────────────────────────────────────────

pub struct AudioStreamHandle {
    /// 42-byte FLAC stream header: b"fLaC" + metadata block header + STREAMINFO.
    /// The raw STREAMINFO payload (for WebCodecs description) is bytes [8..42].
    pub stream_header: Arc<Vec<u8>>,
    /// Broadcast channel. Each value is one FLAC audio frame (no stream header).
    /// Requires WebCodecs + AudioWorklet (secure context only).
    pub frames: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
    /// Broadcast channel. Each value is one block of raw i16 stereo little-endian PCM.
    /// Works in any context — used as the Android / HTTP fallback path.
    pub pcm_frames: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
}

/// Start the encoder thread. `consumer` drains the ring buffer written by the
/// JACK callback in real-time. Returns a handle for the web server.
pub fn start(consumer: HeapConsumer<f32>, sample_rate: u32) -> AudioStreamHandle {
    let (tx_flac, _) = broadcast::channel::<Arc<Vec<u8>>>(64);
    let (tx_pcm, _)  = broadcast::channel::<Arc<Vec<u8>>>(64);
    let tx_flac = Arc::new(tx_flac);
    let tx_pcm  = Arc::new(tx_pcm);

    let enc = FlacEncoder::new(sample_rate);
    let stream_header = Arc::new(enc.make_stream_header());

    let tx_flac2 = Arc::clone(&tx_flac);
    let tx_pcm2  = Arc::clone(&tx_pcm);
    std::thread::Builder::new()
        .name("flac-enc".into())
        .spawn(move || run_encoder(consumer, enc, tx_flac2, tx_pcm2))
        .expect("failed to spawn flac encoder thread");

    AudioStreamHandle { stream_header, frames: tx_flac, pcm_frames: tx_pcm }
}

// ── Encoder thread ──────────────────────────────────────────────────────────

fn run_encoder(
    mut consumer: HeapConsumer<f32>,
    mut enc: FlacEncoder,
    tx_flac: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
    tx_pcm: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
) {
    let mut buf = vec![0f32; BLOCK_SAMPLES * CHANNELS];
    let mut filled = 0usize;

    loop {
        let n = consumer.pop_slice(&mut buf[filled..]);
        filled += n;

        if filled >= BLOCK_SAMPLES * CHANNELS {
            let block = &buf[..BLOCK_SAMPLES * CHANNELS];
            let any_listener = tx_flac.receiver_count() > 0 || tx_pcm.receiver_count() > 0;

            if any_listener {
                if tx_flac.receiver_count() > 0 {
                    let frame = enc.encode_frame(block);
                    let _ = tx_flac.send(Arc::new(frame));
                } else {
                    enc.frame_number += 1;
                }
                if tx_pcm.receiver_count() > 0 {
                    // Raw i16 stereo little-endian PCM — no decoder needed on the browser
                    let mut pcm = Vec::with_capacity(BLOCK_SAMPLES * CHANNELS * 2);
                    for &s in block {
                        let i = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        pcm.extend_from_slice(&i.to_le_bytes());
                    }
                    let _ = tx_pcm.send(Arc::new(pcm));
                }
            } else {
                enc.frame_number += 1;
            }

            let rest = filled - BLOCK_SAMPLES * CHANNELS;
            buf.copy_within(BLOCK_SAMPLES * CHANNELS..filled, 0);
            filled = rest;
        } else if n == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

// ── FLAC encoder ───────────────────────────────────────────────────────────

struct FlacEncoder {
    sample_rate: u32,
    pub frame_number: u64,
}

impl FlacEncoder {
    fn new(sample_rate: u32) -> Self {
        Self { sample_rate, frame_number: 0 }
    }

    /// 42-byte stream header: b"fLaC" + metadata block header + 34-byte STREAMINFO.
    /// Send this once per client. The STREAMINFO payload is at bytes [8..42].
    fn make_stream_header(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(42);
        h.extend_from_slice(b"fLaC");
        // Metadata block header: last_block=1, type=0 (STREAMINFO), length=34
        h.push(0x80); // bit7=last, bits6-0=type(0)
        h.push(0x00);
        h.push(0x00);
        h.push(0x22); // length = 34
        h.extend_from_slice(&self.make_streaminfo());
        debug_assert_eq!(h.len(), 42);
        h
    }

    /// 34-byte STREAMINFO metadata block payload.
    fn make_streaminfo(&self) -> [u8; 34] {
        let mut bw = BitWriter::new();
        bw.write_bits(BLOCK_SAMPLES as u64, 16); // min block size
        bw.write_bits(BLOCK_SAMPLES as u64, 16); // max block size
        bw.write_bits(0, 24); // min frame size: 0 = unknown
        bw.write_bits(0, 24); // max frame size: 0 = unknown
        bw.write_bits(self.sample_rate as u64, 20);
        bw.write_bits((CHANNELS - 1) as u64, 3); // channels - 1
        bw.write_bits((BPS - 1) as u64, 5);       // bits-per-sample - 1
        bw.write_bits(0, 36); // total samples: 0 = streaming / unknown
        for _ in 0..16 { bw.write_bits(0, 8); } // MD5 = all zeros
        let v = bw.finish();
        debug_assert_eq!(v.len(), 34);
        v.try_into().expect("STREAMINFO must be 34 bytes")
    }

    /// Encode one block of `BLOCK_SAMPLES` stereo interleaved f32 samples.
    fn encode_frame(&mut self, samples: &[f32]) -> Vec<u8> {
        debug_assert_eq!(samples.len(), BLOCK_SAMPLES * CHANNELS);

        // f32 → i16 (clip to [-1, 1])
        let mut left  = [0i32; BLOCK_SAMPLES];
        let mut right = [0i32; BLOCK_SAMPLES];
        for i in 0..BLOCK_SAMPLES {
            left[i]  = (samples[i * 2    ].clamp(-1.0, 1.0) * 32767.0) as i32;
            right[i] = (samples[i * 2 + 1].clamp(-1.0, 1.0) * 32767.0) as i32;
        }

        // Frame header bytes (up to and including the CRC-8)
        //   0xFF 0xF8: sync(14) + reserved(1) + blocking_strategy(1)
        //   0x80: block_size=1000(256 samples) + sample_rate=0000(from STREAMINFO)
        //   0x18: channel=0001(LR stereo) + sample_size=100(16-bit) + reserved(1)
        //   UTF-8 coded frame number
        //   CRC-8
        let mut hdr = Vec::with_capacity(8);
        hdr.push(0xFF);
        hdr.push(0xF8);
        hdr.push(0x80); // block size 1000 = 256, sample rate 0000 = from STREAMINFO
        hdr.push(0x18); // channel 0001 = stereo, sample size 100 = 16-bit
        write_utf8_int(&mut hdr, self.frame_number);
        hdr.push(crc8(&hdr));

        // Subframes (FIXED order 2 for both channels)
        let mut bw = BitWriter::new();
        write_fixed2_subframe(&mut bw, &left);
        write_fixed2_subframe(&mut bw, &right);
        bw.flush_to_byte();
        let subframes = bw.finish();

        // Assemble: header + subframes + CRC-16 of the whole frame
        let mut frame = hdr;
        frame.extend_from_slice(&subframes);
        let c = crc16(&frame);
        frame.push((c >> 8) as u8);
        frame.push(c as u8);

        self.frame_number += 1;
        frame
    }
}

// ── FIXED-2 subframe ────────────────────────────────────────────────────────

fn write_fixed2_subframe(bw: &mut BitWriter, samples: &[i32; BLOCK_SAMPLES]) {
    // Subframe header: zero(1) + type(6) + wasted_bits_flag(1)
    // FIXED order 2: type = 0b001010 → byte = 0b0_001010_0 = 0x14
    bw.write_bits(0x14, 8);

    // Warm-up: first 2 samples encoded as raw BPS-bit signed integers
    bw.write_bits(samples[0] as u64, BPS);
    bw.write_bits(samples[1] as u64, BPS);

    // Residuals: r[i] = s[i] - 2·s[i-1] + s[i-2]
    let mut residuals = [0i32; BLOCK_SAMPLES - 2];
    for i in 0..BLOCK_SAMPLES - 2 {
        residuals[i] = samples[i + 2] - 2 * samples[i + 1] + samples[i];
    }

    write_rice_residuals(bw, &residuals);
}

// ── Rice residual coding ────────────────────────────────────────────────────

fn write_rice_residuals(bw: &mut BitWriter, residuals: &[i32]) {
    // Zigzag-map signed → unsigned: 0, -1, 1, -2, 2, …
    let mut uvals = [0u32; BLOCK_SAMPLES - 2];
    let mut max_uval = 0u32;
    for (i, &r) in residuals.iter().enumerate() {
        let u = if r >= 0 { (r as u32) << 1 } else { ((-r - 1) as u32) << 1 | 1 };
        uvals[i] = u;
        if u > max_uval { max_uval = u; }
    }
    let n = residuals.len();
    let k = best_rice_param(&uvals[..n], max_uval);

    // Residual coding header: method=00 (4-bit Rice params), partition_order=0000
    bw.write_bits(0, 2); // coding method = 0
    bw.write_bits(0, 4); // partition order = 0 (one partition covers all residuals)

    if k >= 15 {
        // Escape: fixed-width encoding
        bw.write_bits(15, 4);         // escape sentinel
        bw.write_bits(BPS as u64, 5); // bits per escaped sample
        for &u in &uvals[..n] {
            bw.write_bits(u as u64, BPS);
        }
    } else {
        bw.write_bits(k as u64, 4);
        let m = 1u32 << k;
        for &u in &uvals[..n] {
            let q = u >> k;
            let r = u & (m - 1);
            for _ in 0..q { bw.write_bits(1, 1); } // unary quotient: q ones
            bw.write_bits(0, 1);                    // unary terminator
            if k > 0 { bw.write_bits(r as u64, k as u8); }
        }
    }
}

fn best_rice_param(vals: &[u32], max_val: u32) -> u32 {
    if vals.is_empty() { return 0; }
    let mut best_k = 0u32;
    let mut best_bits = u64::MAX;
    for k in 0u32..=14 {
        // Estimate total bits: sum of (quotient + 1 + k) per sample, plus header
        let bits: u64 = vals.iter()
            .map(|&v| (v >> k) as u64 + 1 + k as u64)
            .sum::<u64>() + 10; // +10 for method(2) + partition_order(4) + rice_param(4)
        if bits < best_bits { best_bits = bits; best_k = k; }
    }
    // Use escape if the largest quotient would produce impractically long unary runs
    if max_val >> best_k > 16384 { 15 } else { best_k }
}

// ── CRC ─────────────────────────────────────────────────────────────────────

fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

// ── Bit writer ───────────────────────────────────────────────────────────────

struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    bits: u8, // bits accumulated in `cur` (0–7)
}

impl BitWriter {
    fn new() -> Self { Self { buf: Vec::new(), cur: 0, bits: 0 } }

    /// Write the `n` least-significant bits of `val`, MSB first.
    fn write_bits(&mut self, val: u64, n: u8) {
        for i in (0..n).rev() {
            self.cur = (self.cur << 1) | (((val >> i) & 1) as u8);
            self.bits += 1;
            if self.bits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.bits = 0;
            }
        }
    }

    /// Zero-pad to the next byte boundary (required between subframes and before CRC).
    fn flush_to_byte(&mut self) {
        if self.bits > 0 {
            self.buf.push(self.cur << (8 - self.bits));
            self.cur = 0;
            self.bits = 0;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.flush_to_byte();
        self.buf
    }
}

// ── UTF-8 integer encoding (FLAC frame-number coding) ───────────────────────

fn write_utf8_int(buf: &mut Vec<u8>, v: u64) {
    match v {
        0x000_0000..=0x00_007F => buf.push(v as u8),
        0x000_0080..=0x00_07FF => {
            buf.push(0xC0 | (v >> 6) as u8);
            buf.push(0x80 | (v & 0x3F) as u8);
        }
        0x000_0800..=0x00_FFFF => {
            buf.push(0xE0 | (v >> 12) as u8);
            buf.push(0x80 | ((v >> 6) & 0x3F) as u8);
            buf.push(0x80 | (v & 0x3F) as u8);
        }
        0x001_0000..=0x1F_FFFF => {
            buf.push(0xF0 | (v >> 18) as u8);
            buf.push(0x80 | ((v >> 12) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 6) & 0x3F) as u8);
            buf.push(0x80 | (v & 0x3F) as u8);
        }
        0x020_0000..=0x3FF_FFFF => {
            buf.push(0xF8 | (v >> 24) as u8);
            buf.push(0x80 | ((v >> 18) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 12) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 6) & 0x3F) as u8);
            buf.push(0x80 | (v & 0x3F) as u8);
        }
        _ => {
            buf.push(0xFC | (v >> 30) as u8);
            buf.push(0x80 | ((v >> 24) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 18) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 12) & 0x3F) as u8);
            buf.push(0x80 | ((v >> 6) & 0x3F) as u8);
            buf.push(0x80 | (v & 0x3F) as u8);
        }
    }
}
