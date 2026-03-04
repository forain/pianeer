use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use pianeer_core::snapshot::{ClientCmd, WebSnapshot};
use pianeer_core::types::MenuAction;

// ── Trait ─────────────────────────────────────────────────────────────────────

pub trait PianeerBackend {
    /// Drain incoming messages / update cached state.
    fn poll(&mut self);
    /// Latest snapshot. Guaranteed to be valid after `poll()`.
    fn snapshot(&self) -> &WebSnapshot;
    /// Send a command to the sampler.
    fn send(&mut self, cmd: ClientCmd);
}

// ── LocalBackend — in-process (desktop native window + Android) ──────────────

pub struct LocalBackend {
    snapshot_arc: Arc<Mutex<String>>,
    action_tx:    Sender<MenuAction>,
    cached:       WebSnapshot,
}

impl LocalBackend {
    pub fn new(snapshot_arc: Arc<Mutex<String>>, action_tx: Sender<MenuAction>) -> Self {
        Self {
            snapshot_arc,
            action_tx,
            cached: WebSnapshot::default(),
        }
    }
}

impl PianeerBackend for LocalBackend {
    fn poll(&mut self) {
        if let Ok(json) = self.snapshot_arc.lock().map(|g| g.clone()) {
            if let Ok(snap) = serde_json::from_str::<WebSnapshot>(&json) {
                self.cached = snap;
            }
        }
    }

    fn snapshot(&self) -> &WebSnapshot {
        &self.cached
    }

    fn send(&mut self, cmd: ClientCmd) {
        let action = client_cmd_to_action(cmd);
        if let Some(a) = action {
            let _ = self.action_tx.try_send(a);
        }
    }
}

fn client_cmd_to_action(cmd: ClientCmd) -> Option<MenuAction> {
    match cmd {
        ClientCmd::CursorUp                    => Some(MenuAction::CursorUp),
        ClientCmd::CursorDown                  => Some(MenuAction::CursorDown),
        ClientCmd::Select                      => Some(MenuAction::Select),
        ClientCmd::SelectAt { idx }            => Some(MenuAction::SelectAt(idx)),
        ClientCmd::CycleVariant { dir }        => Some(MenuAction::CycleVariant(dir)),
        ClientCmd::CycleVariantAt { idx, dir } => Some(MenuAction::CycleVariantAt { idx, dir }),
        ClientCmd::CycleVeltrack               => Some(MenuAction::CycleVeltrack),
        ClientCmd::CycleTune                   => Some(MenuAction::CycleTune),
        ClientCmd::ToggleRelease               => Some(MenuAction::ToggleRelease),
        ClientCmd::VolumeChange { delta }      => Some(MenuAction::VolumeChange(delta)),
        ClientCmd::TransposeChange { delta }   => Some(MenuAction::TransposeChange(delta)),
        ClientCmd::ToggleResonance             => Some(MenuAction::ToggleResonance),
        ClientCmd::ToggleRecord                => Some(MenuAction::ToggleRecord),
        ClientCmd::PauseResume                 => Some(MenuAction::PauseResume),
        ClientCmd::SeekRelative { secs }       => Some(MenuAction::SeekRelative(secs)),
        ClientCmd::Rescan                      => Some(MenuAction::Rescan),
        ClientCmd::RequestAudioInit            => None,
    }
}

// ── RemoteBackend (non-WASM) — ewebsock WebSocket only ───────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub struct RemoteBackend {
    sender:   ewebsock::WsSender,
    receiver: ewebsock::WsReceiver,
    cached:   WebSnapshot,
}

#[cfg(not(target_arch = "wasm32"))]
impl RemoteBackend {
    /// Connect to the Pianeer server WebSocket endpoint.
    /// `url` should be e.g. `"ws://127.0.0.1:6000/ws"`.
    pub fn connect(url: &str) -> Result<Self, String> {
        let (sender, receiver) = ewebsock::connect(url, ewebsock::Options::default())
            .map_err(|e| e.to_string())?;
        Ok(Self { sender, receiver, cached: WebSnapshot::default() })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl PianeerBackend for RemoteBackend {
    fn poll(&mut self) {
        while let Some(event) = self.receiver.try_recv() {
            if let ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) = event {
                if let Ok(snap) = serde_json::from_str::<WebSnapshot>(&text) {
                    self.cached = snap;
                }
            }
        }
    }

    fn snapshot(&self) -> &WebSnapshot { &self.cached }

    fn send(&mut self, cmd: ClientCmd) {
        if let Ok(json) = serde_json::to_string(&cmd) {
            self.sender.send(ewebsock::WsMessage::Text(json));
        }
    }
}

// ── RemoteBackend (WASM) — WebTransport with WebSocket fallback ───────────────
//
// Connection sequence:
//   1. Check if `WebTransport` is available in the browser.
//   2. Fetch /cert-hash from the HTTP server to get the self-signed cert fingerprint.
//   3. Try WebTransport (port 4433) with serverCertificateHashes.
//   4. On any failure, fall back to ewebsock WebSocket (/ws on port 4000).
//
// All async work runs in a `spawn_local` task.  `poll()` / `send()` interact
// with it via `Rc<RefCell<VecDeque>>` queues (WASM is single-threaded).

#[cfg(target_arch = "wasm32")]
pub struct RemoteBackend {
    incoming: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<WebSnapshot>>>,
    outgoing: std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<String>>>,
    cached:   WebSnapshot,
}

#[cfg(target_arch = "wasm32")]
impl RemoteBackend {
    pub fn connect(ws_url: &str) -> Result<Self, String> {
        use std::collections::VecDeque;
        use std::rc::Rc;
        use std::cell::RefCell;

        let incoming = Rc::new(RefCell::new(VecDeque::<WebSnapshot>::new()));
        let outgoing = Rc::new(RefCell::new(VecDeque::<String>::new()));

        let inc = Rc::clone(&incoming);
        let out = Rc::clone(&outgoing);
        let url = ws_url.to_string();

        wasm_bindgen_futures::spawn_local(async move {
            wasm_connect(url, inc, out).await;
        });

        Ok(Self { incoming, outgoing, cached: WebSnapshot::default() })
    }
}

#[cfg(target_arch = "wasm32")]
impl PianeerBackend for RemoteBackend {
    fn poll(&mut self) {
        while let Some(snap) = self.incoming.borrow_mut().pop_front() {
            self.cached = snap;
        }
    }

    fn snapshot(&self) -> &WebSnapshot { &self.cached }

    fn send(&mut self, cmd: ClientCmd) {
        if let Ok(json) = serde_json::to_string(&cmd) {
            self.outgoing.borrow_mut().push_back(json);
        }
    }
}

// ── WASM async helpers ────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
type SnapshotQueue = std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<WebSnapshot>>>;
#[cfg(target_arch = "wasm32")]
type CmdQueue = std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<String>>>;

/// Top-level connection task: try WebTransport, then fall back to WebSocket.
#[cfg(target_arch = "wasm32")]
async fn wasm_connect(ws_url: String, incoming: SnapshotQueue, outgoing: CmdQueue) {
    // Derive host-only (no port) and HTTP base from the WS URL.
    let stripped = ws_url.trim_start_matches("ws://").trim_start_matches("wss://");
    let host_port = stripped.split('/').next().unwrap_or("localhost:4000");
    let host_only = host_port.split(':').next().unwrap_or("localhost");
    let http_base = format!("http://{}", host_port);

    // Check if WebTransport is available in this browser.
    let wt_supported = js_sys::Reflect::has(&js_sys::global(), &"WebTransport".into())
        .unwrap_or(false);

    if wt_supported {
        let cert_url = format!("{}/cert-hash", http_base);
        let wt_url   = format!("https://{}:4433", host_only);

        if let Ok(cert_bytes) = fetch_cert_hash(&cert_url).await {
            match wt_run(&wt_url, &cert_bytes, incoming.clone(), outgoing.clone()).await {
                Ok(()) => return, // session ended normally
                Err(()) => {
                    web_sys::console::warn_1(
                        &"Pianeer: WebTransport failed, falling back to WebSocket".into(),
                    );
                }
            }
        }
    }

    ws_poll_loop(ws_url, incoming, outgoing).await;
}

/// Fetch the server's TLS cert fingerprint as a Vec<u8> from /cert-hash.
#[cfg(target_arch = "wasm32")]
async fn fetch_cert_hash(url: &str) -> Result<Vec<u8>, ()> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or(())?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await.map_err(|_| ())?
        .dyn_into().map_err(|_| ())?;
    let text: String = JsFuture::from(resp.text().map_err(|_| ())?)
        .await.map_err(|_| ())?
        .as_string().ok_or(())?;
    serde_json::from_str::<Vec<u8>>(&text).map_err(|_| ())
}

/// Run the WebTransport session.  Returns Ok(()) when the session ends
/// cleanly; Err(()) on any setup or I/O error (caller falls back to WS).
#[cfg(target_arch = "wasm32")]
async fn wt_run(
    url: &str,
    cert_bytes: &[u8],
    incoming: SnapshotQueue,
    outgoing: CmdQueue,
) -> Result<(), ()> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    // Build options object with serverCertificateHashes via JS reflection so we
    // don't need the WebTransportHashAlgorithm / WebTransportReceiveStream etc.
    // unstable feature flags.
    let cert_arr = js_sys::Uint8Array::from(cert_bytes);
    let hash_obj = js_sys::Object::new();
    js_sys::Reflect::set(&hash_obj, &"algorithm".into(), &"sha-256".into()).map_err(|_| ())?;
    js_sys::Reflect::set(&hash_obj, &"value".into(), &cert_arr).map_err(|_| ())?;
    let hashes = js_sys::Array::of1(&hash_obj);

    let opts = web_sys::WebTransportOptions::new();
    js_sys::Reflect::set(&opts, &"serverCertificateHashes".into(), &hashes).map_err(|_| ())?;

    let transport = web_sys::WebTransport::new_with_options(url, &opts).map_err(|_| ())?;
    JsFuture::from(transport.ready()).await.map_err(|_| ())?;
    web_sys::console::log_1(&"Pianeer: WebTransport connected".into());

    // Accept the first incoming bidirectional stream opened by the server.
    let bidi_reader: web_sys::ReadableStreamDefaultReader =
        transport.incoming_bidirectional_streams().get_reader().unchecked_into();
    let bidi_result = JsFuture::from(bidi_reader.read()).await.map_err(|_| ())?;
    if js_sys::Reflect::get(&bidi_result, &"done".into())
        .map_err(|_| ())?.as_bool().unwrap_or(true) {
        return Err(());
    }
    let bidi_stream: web_sys::WebTransportBidirectionalStream =
        js_sys::Reflect::get(&bidi_result, &"value".into())
            .map_err(|_| ())?.dyn_into().map_err(|_| ())?;

    // Get readable / writable via JS reflection to avoid needing
    // WebTransportReceiveStream / WebTransportSendStream feature flags.
    let readable: web_sys::ReadableStream =
        js_sys::Reflect::get(&bidi_stream, &"readable".into())
            .map_err(|_| ())?.unchecked_into();
    let writable: web_sys::WritableStream =
        js_sys::Reflect::get(&bidi_stream, &"writable".into())
            .map_err(|_| ())?.unchecked_into();

    let writer = writable.get_writer().map_err(|_| ())?;
    let data_reader: web_sys::ReadableStreamDefaultReader =
        readable.get_reader().unchecked_into();

    let mut line_buf = String::new();
    loop {
        // Flush any pending outgoing commands before blocking on the next read.
        let pending: Vec<String> = outgoing.borrow_mut().drain(..).collect();
        for cmd in pending {
            let data = format!("{}\n", cmd);
            let arr = js_sys::Uint8Array::from(data.as_bytes());
            let _ = JsFuture::from(writer.write_with_chunk(&arr)).await;
        }

        // Wait for next chunk from the server.
        let result = JsFuture::from(data_reader.read()).await.map_err(|_| ())?;
        if js_sys::Reflect::get(&result, &"done".into())
            .map_err(|_| ())?.as_bool().unwrap_or(true) {
            break;
        }
        let chunk: js_sys::Uint8Array =
            js_sys::Reflect::get(&result, &"value".into())
                .map_err(|_| ())?.dyn_into().map_err(|_| ())?;

        line_buf.push_str(&String::from_utf8_lossy(&chunk.to_vec()));
        while let Some(nl) = line_buf.find('\n') {
            let line = line_buf[..nl].to_string();
            line_buf = line_buf[nl + 1..].to_string();
            // Skip the audio_init line (handled at transport level).
            if let Ok(snap) = serde_json::from_str::<WebSnapshot>(&line) {
                incoming.borrow_mut().push_back(snap);
            }
        }
    }

    Ok(())
}

/// Poll ewebsock WebSocket in a loop, yielding to the browser event loop via
/// setTimeout(50) between iterations so WS callbacks can fire.
#[cfg(target_arch = "wasm32")]
async fn ws_poll_loop(ws_url: String, incoming: SnapshotQueue, outgoing: CmdQueue) {
    let (mut sender, receiver) =
        match ewebsock::connect(&ws_url, ewebsock::Options::default()) {
            Ok(pair) => pair,
            Err(_) => return,
        };

    loop {
        while let Some(ev) = receiver.try_recv() {
            if let ewebsock::WsEvent::Message(ewebsock::WsMessage::Text(text)) = ev {
                if let Ok(snap) = serde_json::from_str::<WebSnapshot>(&text) {
                    incoming.borrow_mut().push_back(snap);
                }
            }
        }
        let pending: Vec<String> = outgoing.borrow_mut().drain(..).collect();
        for cmd in pending {
            sender.send(ewebsock::WsMessage::Text(cmd));
        }
        sleep_ms(50).await;
    }
}

/// Yield to the browser event loop for `ms` milliseconds using setTimeout.
#[cfg(target_arch = "wasm32")]
async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}
