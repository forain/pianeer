use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::{Router, extract::ws::{Message, WebSocket, WebSocketUpgrade}, response::Html, routing::get};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::ui::{MenuAction, MenuItem, Settings, ProcStats};

// ── Snapshot types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct WebMenuItem {
    pub name: String,
    pub type_label: String,
    pub loaded: bool,
    pub playing: bool,
    pub cursor: bool,
    pub variant: Option<String>,
}

#[derive(Serialize)]
pub struct WebSnapshot {
    pub menu: Vec<WebMenuItem>,
    pub settings: Settings,
    pub sustain: bool,
    pub stats: ProcStats,
}

// ── Command from browser ───────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum ClientCmd {
    CursorUp,
    CursorDown,
    Select,
    SelectAt { idx: usize },
    CycleVariant { dir: i8 },
    CycleVariantAt { idx: usize, dir: i8 },
    CycleVeltrack,
    CycleTune,
    ToggleRelease,
    VolumeChange { delta: i8 },
    TransposeChange { delta: i8 },
    ToggleResonance,
    Rescan,
}

// ── Public helpers ─────────────────────────────────────────────────────────────

pub fn build_snapshot(
    menu: &[MenuItem],
    cursor: usize,
    loaded: usize,
    playing: Option<usize>,
    settings: &Settings,
    sustain: bool,
    stats: &ProcStats,
) -> WebSnapshot {
    let items = menu.iter().enumerate().map(|(i, m)| {
        let variant = if let MenuItem::Instrument(inst) = m {
            inst.variant_name().map(|s| s.to_string())
        } else {
            None
        };
        WebMenuItem {
            name: m.display_name().to_string(),
            type_label: m.type_label().to_string(),
            loaded: matches!(m, MenuItem::Instrument(_)) && i == loaded,
            playing: playing == Some(i),
            cursor: i == cursor,
            variant,
        }
    }).collect();

    WebSnapshot {
        menu: items,
        settings: settings.clone(),
        sustain,
        stats: stats.clone(),
    }
}

pub fn spawn_web_server(
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for web server");
        rt.block_on(run(snapshot, action_tx, reload));
    })
}

// ── Async server core ──────────────────────────────────────────────────────────

/// Collect all local unicast IPs to include in the cert SAN list.
/// Chrome validates the SAN even when serverCertificateHashes is set,
/// so every IP a client might use to reach this host must be listed.
fn local_sans() -> Vec<String> {
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    // UDP-connect trick: no packet is sent; the OS tells us which source IP
    // it would use to reach an external address — i.e. the LAN IP.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("1.1.1.1:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if !sans.contains(&ip) {
                    sans.push(ip);
                }
            }
        }
    }
    // Same for IPv6 (may fail on v4-only systems — that's fine).
    if let Ok(sock) = std::net::UdpSocket::bind("[::]:0") {
        if sock.connect("[2606:4700:4700::1111]:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if !sans.contains(&ip) {
                    sans.push(ip);
                }
            }
        }
    }
    sans
}

async fn run(
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
) {
    let sans = local_sans();

    // Generate self-signed TLS identity (max 14 days per WebTransport spec).
    let identity = Identity::self_signed(sans.iter().map(|s| s.as_str()))
        .expect("failed to create self-signed identity");

    // Extract SHA-256 fingerprint of the leaf cert for the browser's
    // serverCertificateHashes option.
    let cert_hash: wtransport::tls::Sha256Digest =
        identity.certificate_chain().as_slice()[0].hash();
    let cert_bytes: &[u8; 32] = cert_hash.as_ref();
    let cert_hash_json = format!(
        "[{}]",
        cert_bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
    );

    let config = ServerConfig::builder()
        .with_bind_default(4433)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();

    let server = match Endpoint::server(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Web UI: failed to bind QUIC/WebTransport on :4433: {e}");
            return;
        }
    };

    eprintln!("Web UI:  http://localhost:8080  (Chrome/Edge 100+ required)");
    for san in &sans {
        if san != "localhost" && san != "127.0.0.1" && san != "::1" {
            eprintln!("Web UI:  http://{san}:8080");
        }
    }

    // HTTP server serving the single-page HTML app + WebSocket fallback.
    let snap_http = Arc::clone(&snapshot);
    tokio::spawn(serve_http(cert_hash_json, snap_http, action_tx.clone(), Arc::clone(&reload)));

    // Accept WebTransport sessions.
    loop {
        let incoming = server.accept().await;
        let snap = Arc::clone(&snapshot);
        let tx = action_tx.clone();
        let rl = Arc::clone(&reload);
        tokio::spawn(async move {
            if let Err(e) = handle_session(incoming, snap, tx, rl).await {
                eprintln!("Web session error: {e}");
            }
        });
    }
}

fn dispatch_cmd(
    line: &str,
    action_tx: &crossbeam_channel::Sender<MenuAction>,
    reload: &AtomicBool,
) {
    if line.is_empty() { return; }
    if let Ok(cmd) = serde_json::from_str::<ClientCmd>(line) {
        let action = match cmd {
            ClientCmd::CursorUp                       => Some(MenuAction::CursorUp),
            ClientCmd::CursorDown                     => Some(MenuAction::CursorDown),
            ClientCmd::Select                         => Some(MenuAction::Select),
            ClientCmd::SelectAt { idx }               => Some(MenuAction::SelectAt(idx)),
            ClientCmd::CycleVariant { dir }           => Some(MenuAction::CycleVariant(dir)),
            ClientCmd::CycleVariantAt { idx, dir }    => Some(MenuAction::CycleVariantAt { idx, dir }),
            ClientCmd::CycleVeltrack                  => Some(MenuAction::CycleVeltrack),
            ClientCmd::CycleTune                      => Some(MenuAction::CycleTune),
            ClientCmd::ToggleRelease                  => Some(MenuAction::ToggleRelease),
            ClientCmd::VolumeChange { delta }         => Some(MenuAction::VolumeChange(delta)),
            ClientCmd::TransposeChange { delta }      => Some(MenuAction::TransposeChange(delta)),
            ClientCmd::ToggleResonance                => Some(MenuAction::ToggleResonance),
            ClientCmd::Rescan => {
                reload.store(true, Ordering::SeqCst);
                None
            }
        };
        if let Some(a) = action {
            let _ = action_tx.try_send(a);
        }
    }
}

async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request: wtransport::endpoint::SessionRequest = incoming.await?;
    let conn: wtransport::Connection = request.accept().await?;

    // Server opens the bidi stream so the browser has no chicken-and-egg wait.
    // Send side: JSON state lines pushed every 100 ms.
    // Recv side: newline-delimited JSON commands from the browser.
    // open_bi() returns an OpeningBiStream future; await it twice.
    let (mut send, recv): (wtransport::SendStream, wtransport::RecvStream) =
        conn.open_bi().await?.await?;

    // Pusher: write state JSON (one line per update) to the send side.
    let snap_push = Arc::clone(&snapshot);
    let pusher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut json = snap_push.lock().map(|v| v.clone()).unwrap_or_default();
            json.push('\n');
            if send.write_all(json.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader: consume newline-delimited JSON commands from the recv side.
    let mut lines = BufReader::new(recv).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        dispatch_cmd(line.trim(), &action_tx, &reload);
    }

    pusher.abort();
    Ok(())
}

async fn handle_ws(
    mut socket: WebSocket,
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let json = snapshot.lock().map(|v| v.clone()).unwrap_or_default();
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        dispatch_cmd(text.trim(), &action_tx, &reload);
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            dispatch_cmd(text.trim(), &action_tx, &reload);
                        }
                    }
                    None | Some(Err(_)) => break,
                    _ => {} // ping/pong handled automatically
                }
            }
        }
    }
}

async fn serve_http(
    cert_hash_json: String,
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
) {
    let html = Arc::new(HTML_TEMPLATE.replace("__CERT_HASH__", &cert_hash_json));

    let app = Router::new()
        .route("/", get({
            let html = Arc::clone(&html);
            move || {
                let html = Arc::clone(&html);
                async move { Html(html.as_ref().clone()) }
            }
        }))
        .route("/ws", get({
            let snapshot = Arc::clone(&snapshot);
            let action_tx = action_tx.clone();
            let reload = Arc::clone(&reload);
            move |ws: WebSocketUpgrade| {
                let snapshot = Arc::clone(&snapshot);
                let action_tx = action_tx.clone();
                let reload = Arc::clone(&reload);
                async move {
                    ws.on_upgrade(move |socket| handle_ws(socket, snapshot, action_tx, reload))
                }
            }
        }));

    let listener = match tokio::net::TcpListener::bind("0.0.0.0:8080").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Web UI: failed to bind HTTP on :8080: {e}");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Web UI HTTP server error: {e}");
    }
}

// ── Embedded HTML/JS single-page app ──────────────────────────────────────────

const HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pianeer</title>
<style>
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
  background: #1a1a1a; color: #e0e0e0;
  font-family: 'Courier New', monospace; font-size: 14px;
  padding: 16px; max-width: 820px;
}
h1 { color: #80d0ff; margin-bottom: 6px; font-size: 18px; }
#status { font-size: 12px; color: #888; margin-bottom: 8px; }
.bar {
  background: #252525; padding: 8px 10px; border-radius: 4px;
  margin-bottom: 8px; line-height: 1.8;
}
.vu-row { display: flex; align-items: center; gap: 8px; margin: 3px 0; }
.vu-label { width: 14px; color: #888; }
.vu-track { flex: 1; height: 10px; background: #333; border-radius: 2px; overflow: hidden; }
.vu-fill  { height: 100%; width: 0%; transition: width 60ms linear; }
.vu-db    { width: 66px; text-align: right; font-size: 12px; color: #aaa; }
#clip     { color: #ff4444; font-weight: bold; font-size: 12px; margin-left: 6px; }
.section-label { color: #666; margin: 10px 0 4px; font-size: 12px; letter-spacing: 1px; }
.item {
  display: flex; align-items: center; gap: 8px;
  padding: 3px 6px; border-radius: 3px; cursor: pointer;
}
.item:hover    { background: #2a2a2a; }
.item.cursor   { background: #1e2a3a; outline: 1px solid #3355aa; }
.item.loaded   { color: #70c8ff; }
.item.playing  { color: #70ffaa; }
.tag           { width: 72px; font-size: 12px; color: #666; flex-shrink: 0; }
.type-lbl      { width: 34px; font-size: 12px; color: #555; flex-shrink: 0; }
.item-name     { flex: 1; }
.var-area      { display: flex; align-items: center; gap: 3px; margin-left: 4px; flex-shrink: 0; }
.var-name      { font-size: 11px; color: #888; min-width: 40px; text-align: center; }
.var-btn {
  background: transparent; border: 1px solid #3a3a3a; color: #666;
  cursor: pointer; font-size: 11px; padding: 1px 5px; border-radius: 2px;
  line-height: 1.3; font-family: inherit;
}
.var-btn:hover { color: #ccc; border-color: #666; }
.controls      { display: flex; flex-wrap: wrap; gap: 5px; margin-top: 14px; }
.btn {
  background: #2a2a2a; border: 1px solid #3a3a3a; color: #ccc;
  padding: 5px 11px; border-radius: 3px; cursor: pointer;
  font-family: inherit; font-size: 12px;
}
.btn:hover  { background: #333; }
.btn:active { background: #2a3a5a; }
</style>
</head>
<body>
<h1>Pianeer</h1>
<div id="status">Connecting&hellip;</div>

<div class="bar" id="settings-bar">Loading&hellip;</div>

<div class="bar" id="vu-area">
  <div class="vu-row">
    <span class="vu-label">L</span>
    <div class="vu-track"><div class="vu-fill" id="vu-l"></div></div>
    <span class="vu-db"  id="vu-l-db">-&infin;&nbsp;dB</span>
  </div>
  <div class="vu-row">
    <span class="vu-label">R</span>
    <div class="vu-track"><div class="vu-fill" id="vu-r"></div></div>
    <span class="vu-db"  id="vu-r-db">-&infin;&nbsp;dB</span>
  </div>
  <span id="clip"></span>
</div>

<div id="menu"></div>

<div class="controls">
  <button class="btn" data-cmd="CursorUp">&#8593; Up</button>
  <button class="btn" data-cmd="CursorDown">&#8595; Down</button>
  <button class="btn" data-cmd="Select">Enter: Load</button>
  <button class="btn" data-json='{"cmd":"CycleVariant","dir":-1}'>&#8592; Variant</button>
  <button class="btn" data-json='{"cmd":"CycleVariant","dir":1}'>&#8594; Variant</button>
  <button class="btn" data-cmd="CycleVeltrack">V: Vel</button>
  <button class="btn" data-cmd="CycleTune">T: Tune</button>
  <button class="btn" data-cmd="ToggleRelease">E: Release</button>
  <button class="btn" data-json='{"cmd":"VolumeChange","delta":1}'>+ Vol</button>
  <button class="btn" data-json='{"cmd":"VolumeChange","delta":-1}'>&minus; Vol</button>
  <button class="btn" data-json='{"cmd":"TransposeChange","delta":-1}'>[ Trns&minus;</button>
  <button class="btn" data-json='{"cmd":"TransposeChange","delta":1}'>] Trns+</button>
  <button class="btn" data-cmd="ToggleResonance">H: Res</button>
  <button class="btn" data-cmd="Rescan">R: Rescan</button>
</div>

<script>
'use strict';
const CERT_HASH = new Uint8Array(__CERT_HASH__);
const enc = new TextEncoder();
const dec = new TextDecoder();
let doSend = null; // set by whichever transport connects
let cursorIdx = 0;

function sendRaw(obj) { if (doSend) doSend(obj); }
function sendCmd(name) { sendRaw({ cmd: name }); }

// Buttons
document.querySelectorAll('.btn').forEach(btn => {
  btn.addEventListener('click', () => {
    const simple = btn.dataset.cmd;
    const json   = btn.dataset.json;
    if (simple) sendCmd(simple);
    else if (json) sendRaw(JSON.parse(json));
  });
});

// Keyboard shortcuts
document.addEventListener('keydown', e => {
  if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
  const k = e.key;
  if (k === 'ArrowUp')    { e.preventDefault(); sendCmd('CursorUp'); }
  else if (k === 'ArrowDown')  { e.preventDefault(); sendCmd('CursorDown'); }
  else if (k === 'ArrowLeft')  sendRaw({ cmd: 'CycleVariant', dir: -1 });
  else if (k === 'ArrowRight') sendRaw({ cmd: 'CycleVariant', dir: 1 });
  else if (k === 'Enter')      sendCmd('Select');
  else if (k === 'v' || k === 'V') sendCmd('CycleVeltrack');
  else if (k === 't' || k === 'T') sendCmd('CycleTune');
  else if (k === 'e' || k === 'E') sendCmd('ToggleRelease');
  else if (k === '+' || k === '=') sendRaw({ cmd: 'VolumeChange', delta: 1 });
  else if (k === '-')              sendRaw({ cmd: 'VolumeChange', delta: -1 });
  else if (k === '[')              sendRaw({ cmd: 'TransposeChange', delta: -1 });
  else if (k === ']')              sendRaw({ cmd: 'TransposeChange', delta: 1 });
  else if (k === 'h' || k === 'H') sendCmd('ToggleResonance');
  else if (k === 'r' || k === 'R') sendCmd('Rescan');
});

// Event delegation on #menu — survives innerHTML replacements.
// pointerdown for variant buttons (fires before the 100ms re-render can destroy the element).
// click for item rows (normal single-click selection).
const menuEl = document.getElementById('menu');
menuEl.addEventListener('pointerdown', e => {
  const btn = e.target.closest('[data-varidx]');
  if (!btn) return;
  e.preventDefault(); // suppress subsequent click
  sendRaw({ cmd: 'CycleVariantAt', idx: +btn.dataset.varidx, dir: +btn.dataset.vardir });
});
menuEl.addEventListener('click', e => {
  if (e.target.closest('[data-varidx]')) return; // already handled above
  const row = e.target.closest('[data-selidx]');
  if (row) sendRaw({ cmd: 'SelectAt', idx: +row.dataset.selidx });
});

// VU helpers
function vuPct(peak) {
  if (peak < 1e-10) return 0;
  const db = 20 * Math.log10(peak);
  return Math.max(0, Math.min(100, ((db + 48) / 48) * 100));
}
function vuColor(pct) {
  if (pct > 95) return '#cc3333';
  if (pct > 75) return '#ccaa22';
  return '#2a8040';
}
function vuDbStr(peak) {
  if (peak < 1e-10) return '-\u221e\u00a0dB';
  return (20 * Math.log10(peak)).toFixed(1) + '\u00a0dB';
}

function esc(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

function renderState(st) {
  const s = st.settings;
  const stats = st.stats;

  // Settings bar
  const sus = st.sustain ? '<strong>Sus:ON</strong>' : 'Sus:off';
  const vol = s.volume_db === 0 ? '0' : (s.volume_db > 0 ? '+' : '') + s.volume_db.toFixed(0);
  const trn = s.transpose  === 0 ? '0' : (s.transpose  > 0 ? '+' : '') + s.transpose;
  document.getElementById('settings-bar').innerHTML =
    `Vel:${s.veltrack}&nbsp; Tune:${s.tune}&nbsp; Rel:${s.release_enabled?'on':'off'}&nbsp; ` +
    `Vol:${vol}dB&nbsp; Trns:${trn}&nbsp; Res:${s.resonance_enabled?'on':'off'}&nbsp; ` +
    `${sus}&nbsp;&nbsp; CPU:${stats.cpu_pct}%&nbsp; Mem:${stats.mem_mb}MB&nbsp; Voices:${stats.voices}`;

  // VU meters
  const lp = vuPct(stats.peak_l), rp = vuPct(stats.peak_r);
  const lEl = document.getElementById('vu-l');
  lEl.style.width = lp + '%'; lEl.style.background = vuColor(lp);
  document.getElementById('vu-l-db').textContent = vuDbStr(stats.peak_l);
  const rEl = document.getElementById('vu-r');
  rEl.style.width = rp + '%'; rEl.style.background = vuColor(rp);
  document.getElementById('vu-r-db').textContent = vuDbStr(stats.peak_r);
  document.getElementById('clip').textContent = stats.clip ? '[CLIP]' : '';

  // Track cursor
  const cursorItem = st.menu.findIndex(m => m.cursor);
  if (cursorItem >= 0) cursorIdx = cursorItem;

  // Only rebuild the menu DOM when something actually changed.
  // This prevents the 100ms innerHTML replacement from destroying buttons
  // mid-click when the user is rapidly pressing variant arrows.
  const menuKey = JSON.stringify(st.menu);
  if (menuKey !== renderState._lastMenuKey) {
    renderState._lastMenuKey = menuKey;
    let html = '';
    let inMidi = false, shownInst = false;
    st.menu.forEach((item, idx) => {
      if (item.type_label !== 'MID' && !shownInst) {
        html += '<div class="section-label">INSTRUMENTS</div>';
        shownInst = true;
      }
      if (item.type_label === 'MID' && !inMidi) {
        html += '<div class="section-label">MIDI FILES</div>';
        inMidi = true;
      }
      const cls = ['item',
        item.cursor  ? 'cursor'  : '',
        item.loaded  ? 'loaded'  : '',
        item.playing ? 'playing' : '',
      ].filter(Boolean).join(' ');

      const tag = item.playing ? '[playing]'
                : item.loaded  ? '[loaded] '
                :                '         ';
      // data-varidx / data-vardir carry the action payload; no inline handlers.
      const varHtml = item.variant
        ? `<span class="var-area">` +
          `<button class="var-btn" data-varidx="${idx}" data-vardir="-1">\u25c4</button>` +
          `<span class="var-name">${esc(item.variant)}</span>` +
          `<button class="var-btn" data-varidx="${idx}" data-vardir="1">\u25ba</button>` +
          `</span>`
        : '';

      // data-selidx carries the row index for SelectAt; no inline onclick.
      html +=
        `<div class="${cls}" data-selidx="${idx}">` +
        `<span class="tag">${esc(tag)}</span>` +
        `<span class="type-lbl">${esc(item.type_label)}</span>` +
        `<span class="item-name">${esc(item.name)}</span>` +
        varHtml +
        `</div>`;
    });
    menuEl.innerHTML = html;
  }
}

async function connect() {
  doSend = null;
  const statusEl = document.getElementById('status');

  // ── Try WebTransport first (Chrome/Edge 100+) ───────────────────────────
  if (window.WebTransport) {
    statusEl.textContent = `Connecting (WebTransport) to ${window.location.hostname}:4433\u2026`;
    try {
      const transport = new WebTransport(`https://${window.location.hostname}:4433`, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: CERT_HASH }]
      });
      await transport.ready;
      statusEl.textContent = 'Connected (WebTransport)';

      // Server opens the bidi stream: recv side = state JSON lines,
      // writable side = commands we send.
      const bidiReader = transport.incomingBidirectionalStreams.getReader();
      const { value: bidi } = await bidiReader.read();
      const writer = bidi.writable.getWriter();
      doSend = obj => writer.write(enc.encode(JSON.stringify(obj) + '\n'));

      // Read newline-delimited JSON state from the stream's readable side.
      const stateReader = bidi.readable.getReader();
      let buf = '';
      while (true) {
        const { value, done } = await stateReader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        const lines = buf.split('\n');
        buf = lines.pop(); // keep any incomplete trailing line
        for (const line of lines) {
          if (line.trim()) {
            try { renderState(JSON.parse(line)); } catch (_) {}
          }
        }
      }

      doSend = null;
      statusEl.textContent = 'Disconnected. Reconnecting\u2026';
      setTimeout(connect, 3000);
      return;
    } catch (_) {
      doSend = null;
      // Fall through to WebSocket.
    }
  }

  // ── WebSocket fallback (all browsers) ────────────────────────────────────
  await new Promise(resolve => {
    const wsUrl = `ws://${window.location.hostname}:8080/ws`;
    statusEl.textContent = `Connecting (WebSocket) to ${wsUrl}\u2026`;
    const ws = new WebSocket(wsUrl);
    ws.onopen  = () => {
      statusEl.textContent = 'Connected (WebSocket)';
      doSend = obj => ws.send(JSON.stringify(obj));
    };
    ws.onmessage = e => {
      try { renderState(JSON.parse(e.data)); } catch (_) {}
    };
    ws.onerror = () => { doSend = null; };
    ws.onclose = () => { doSend = null; resolve(); };
  });

  statusEl.textContent = 'Disconnected. Reconnecting\u2026';
  setTimeout(connect, 3000);
}

connect();
</script>
</body>
</html>
"#;
