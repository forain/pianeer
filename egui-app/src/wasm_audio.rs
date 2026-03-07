use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{AudioContext, AudioContextOptions, MessageEvent, WebSocket};

const SAMPLE_RATE: f32 = 48000.0;
/// JS timer interval driving the scheduler, in ms (~60 fps).
const POLL_INTERVAL_MS: i32 = 16;

// ── Inner state shared between the WS callback and the setInterval timer ──────

struct Inner {
    ctx:       AudioContext,
    queue:     VecDeque<js_sys::ArrayBuffer>,
    next_time: f64,
}

impl Inner {
    fn poll(&mut self) {
        let now = self.ctx.current_time();

        // Dynamic look-ahead: AudioContext hardware latency + timer interval + 10ms margin.
        // Read via JS reflection so it works regardless of web_sys feature flags
        // and gracefully returns 0 on browsers that don't expose these properties.
        let output_latency = js_sys::Reflect::get(self.ctx.as_ref(), &"outputLatency".into())
            .ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let base_latency = js_sys::Reflect::get(self.ctx.as_ref(), &"baseLatency".into())
            .ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let look_ahead = (output_latency
            + base_latency
            + (POLL_INTERVAL_MS as f64 / 1000.0)
            + 0.010)
            .max(0.040);

        // If the queue has grown beyond one look-ahead window's worth of packets,
        // we're falling behind real-time. Drop the excess and resync the clock so
        // latency doesn't accumulate without bound.
        // At 64 frames/48kHz each packet covers ~1.33ms; look_ahead/0.00133 ≈ packet count.
        let max_queue = ((look_ahead / 0.00134) as usize).max(8) + 4;
        if self.queue.len() > max_queue {
            self.queue.drain(..self.queue.len() - max_queue);
            self.next_time = 0.0; // trigger clock resync below
        }

        // Recover from starvation or resync: schedule from just ahead of now.
        if self.next_time < now {
            self.next_time = now + 0.010;
        }

        while self.next_time < now + look_ahead {
            let ab = match self.queue.pop_front() {
                Some(ab) => ab,
                None => break,
            };

            // Each PCM message: N stereo frames as i16 LE.
            // Frame count is derived from message length so any block size works.
            let int16 = js_sys::Int16Array::new(&ab);
            let frame_count = (int16.length() / 2) as usize;
            if frame_count == 0 { continue; }

            let mut left  = vec![0.0f32; frame_count];
            let mut right = vec![0.0f32; frame_count];
            for i in 0..frame_count {
                left[i]  = int16.get_index((i * 2)     as u32) as f32 / 32768.0;
                right[i] = int16.get_index((i * 2 + 1) as u32) as f32 / 32768.0;
            }

            let audio_buf = match self.ctx.create_buffer(2, frame_count as u32, SAMPLE_RATE) {
                Ok(b) => b,
                Err(_) => break,
            };
            let _ = audio_buf.copy_to_channel(&left,  0);
            let _ = audio_buf.copy_to_channel(&right, 1);

            let src = match self.ctx.create_buffer_source() {
                Ok(s) => s,
                Err(_) => break,
            };
            src.set_buffer(Some(&audio_buf));
            let dest = self.ctx.destination();
            let _ = src.connect_with_audio_node(&dest);
            let _ = src.start_with_when(self.next_time);

            self.next_time += frame_count as f64 / SAMPLE_RATE as f64;
        }
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

pub struct AudioPlayer {
    _ws:           WebSocket,
    _onmessage:    Closure<dyn FnMut(MessageEvent)>,
    _poll_closure: Closure<dyn Fn()>,
    interval_id:   i32,
    // Keep the Rc alive so Inner is not dropped while the closures reference it.
    _inner:        Rc<RefCell<Inner>>,
}

impl AudioPlayer {
    /// Connect to `pcm_ws_url` (e.g. `ws://host:4000/audio-pcm-ws`).
    /// Scheduling runs on an independent JS timer — no need to call `poll()`.
    pub fn new(pcm_ws_url: &str) -> Result<Self, JsValue> {
        let mut opts = AudioContextOptions::new();
        opts.sample_rate(SAMPLE_RATE);
        let ctx = AudioContext::new_with_context_options(&opts)?;

        let ws = WebSocket::new(pcm_ws_url)?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let inner = Rc::new(RefCell::new(Inner {
            ctx,
            queue: VecDeque::new(),
            next_time: 0.0,
        }));

        // WS onmessage: push binary frames into the queue.
        let inner_ws = Rc::clone(&inner);
        let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| {
            if let Ok(ab) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                inner_ws.borrow_mut().queue.push_back(ab);
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // setInterval: schedule audio every ~16 ms, independent of egui repaints.
        let inner_timer = Rc::clone(&inner);
        let poll_closure = Closure::wrap(Box::new(move || {
            inner_timer.borrow_mut().poll();
        }) as Box<dyn Fn()>);

        let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let interval_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            poll_closure.as_ref().unchecked_ref(),
            POLL_INTERVAL_MS,
        )?;

        Ok(Self {
            _ws: ws,
            _onmessage: onmessage,
            _poll_closure: poll_closure,
            interval_id,
            _inner: inner,
        })
    }

    /// No-op — scheduling is driven by the internal JS timer.
    /// Kept so `app.rs` can call it without a cfg guard.
    pub fn poll(&mut self) {}
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.clear_interval_with_handle(self.interval_id);
        }
    }
}
