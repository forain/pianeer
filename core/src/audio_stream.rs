// PCM audio streaming for the web UI.
//
// Architecture:
//   JACK/Oboe callback → lock-free ring buffer → encoder thread → broadcast channel
//   Web handlers subscribe to the broadcast and forward to browser clients.
//
// Transport (browser chooses based on availability):
//   Primary:  WebTransport unreliable datagrams  (~30–60 ms latency on LAN)
//   Fallback: /audio-ws WebSocket binary messages (~80–150 ms, reliable)
//
// Wire format: raw i16 stereo little-endian PCM, 64 frames per block (256 bytes).

use std::sync::Arc;
use ringbuf::HeapConsumer;
use tokio::sync::broadcast;

// ── Public constants ────────────────────────────────────────────────────────

/// Samples per PCM block (stereo frames). 64 frames = 1.3 ms at 48 kHz.
pub const BLOCK_SAMPLES: usize = 64;

const CHANNELS: usize = 2;

// ── Public handle ───────────────────────────────────────────────────────────

pub struct AudioStreamHandle {
    /// Broadcast channel. Each value is one block of raw i16 stereo little-endian PCM.
    /// 64 stereo frames × 2 bytes/sample × 2 channels = 256 bytes per message.
    pub frames: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
}

/// Start the encoder thread. `consumer` drains the ring buffer written by the
/// audio callback in real-time. Returns a handle for the web server.
pub fn start(consumer: HeapConsumer<f32>, _sample_rate: u32) -> AudioStreamHandle {
    let (tx, _) = broadcast::channel::<Arc<Vec<u8>>>(64);
    let tx = Arc::new(tx);

    let tx2 = Arc::clone(&tx);
    std::thread::Builder::new()
        .name("pcm-enc".into())
        .spawn(move || run_encoder(consumer, tx2))
        .expect("failed to spawn pcm encoder thread");

    AudioStreamHandle { frames: tx }
}

// ── Encoder thread ──────────────────────────────────────────────────────────

fn run_encoder(
    mut consumer: HeapConsumer<f32>,
    tx: Arc<broadcast::Sender<Arc<Vec<u8>>>>,
) {
    let mut buf = vec![0f32; BLOCK_SAMPLES * CHANNELS];
    let mut filled = 0usize;

    loop {
        let n = consumer.pop_slice(&mut buf[filled..]);
        filled += n;

        if filled >= BLOCK_SAMPLES * CHANNELS {
            if tx.receiver_count() > 0 {
                let block = &buf[..BLOCK_SAMPLES * CHANNELS];
                let mut pcm = Vec::with_capacity(BLOCK_SAMPLES * CHANNELS * 2);
                for &s in block {
                    let i = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm.extend_from_slice(&i.to_le_bytes());
                }
                let _ = tx.send(Arc::new(pcm));
            }

            let rest = filled - BLOCK_SAMPLES * CHANNELS;
            buf.copy_within(BLOCK_SAMPLES * CHANNELS..filled, 0);
            filled = rest;
        } else if n == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
