use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::{Router, extract::ws::{Message, WebSocket, WebSocketUpgrade}, response::Html, routing::get};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::ui::{MenuAction, MenuItem, PlaybackInfo, Settings, ProcStats};

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
    pub recording: bool,
    pub rec_elapsed_us: Option<u64>,
    pub playback: Option<PlaybackInfo>,
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
    ToggleRecord,
    PauseResume,
    SeekRelative { secs: i64 },
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
    recording: bool,
    rec_elapsed_us: Option<u64>,
    playback: Option<&PlaybackInfo>,
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
        recording,
        rec_elapsed_us,
        playback: playback.cloned(),
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

    eprintln!("Web UI:  http://localhost:4000  (Chrome/Edge 100+ required)");
    for san in &sans {
        if san != "localhost" && san != "127.0.0.1" && san != "::1" {
            eprintln!("Web UI:  http://{san}:4000");
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
            ClientCmd::ToggleRecord                   => Some(MenuAction::ToggleRecord),
            ClientCmd::PauseResume                    => Some(MenuAction::PauseResume),
            ClientCmd::SeekRelative { secs }          => Some(MenuAction::SeekRelative(secs)),
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

    let listener = match tokio::net::TcpListener::bind("0.0.0.0:4000").await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Web UI: failed to bind HTTP on :4000: {e}");
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
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>Pianeer</title>
<style>
/* ── Reset & base ─────────────────────────────────────────────────────── */
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html, body { height: 100%; }
body {
  background: #1a1a1a; color: #e0e0e0;
  font-family: 'Courier New', monospace; font-size: 14px;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
  overscroll-behavior: none;
  display: flex; flex-direction: column;
}
button { font-family: inherit; }

/* ── Header ───────────────────────────────────────────────────────────── */
#hdr {
  display: flex; align-items: center; gap: 10px;
  padding: 8px 14px; border-bottom: 1px solid #262626;
  flex-shrink: 0; background: #1f1f1f;
}
#hdr h1 { font-size: 18px; color: #80d0ff; }
#status  { font-size: 11px; color: #666; margin-left: auto; }

/* ── App: column on mobile, row on desktop ────────────────────────────── */
#app { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
@media (min-width: 700px) { #app { flex-direction: row; } }

/* ── Main column ──────────────────────────────────────────────────────── */
#main { display: flex; flex-direction: column; flex: 1; overflow: hidden; }
@media (min-width: 700px) { #main { border-right: 1px solid #262626; } }

/* ── VU meters ────────────────────────────────────────────────────────── */
#vu {
  background: #252525; padding: 7px 14px 5px;
  border-bottom: 1px solid #262626; flex-shrink: 0;
}
.vrow  { display: flex; align-items: center; gap: 8px; margin: 2px 0; }
.vlbl  { width: 12px; color: #777; font-size: 11px; }
.vtrk  { flex: 1; height: 8px; background: #333; border-radius: 2px; overflow: hidden; }
.vfill { height: 100%; width: 0%; transition: width 60ms linear; }
.vdb   { width: 58px; text-align: right; font-size: 11px; color: #aaa; }
#clip  { color: #e05050; font-weight: bold; font-size: 11px; }
#stats { display: flex; flex-wrap: wrap; gap: 8px; padding: 3px 0 1px; font-size: 11px; color: #666; }

/* ── Settings chips (mobile only, hidden on desktop) ──────────────────── */
#chips {
  display: flex; flex-wrap: wrap; gap: 5px;
  padding: 7px 14px; border-bottom: 1px solid #262626; flex-shrink: 0;
}
@media (min-width: 700px) { #chips { display: none; } }

.chip {
  display: inline-flex; align-items: stretch;
  height: 40px; background: #252525; border: 1px solid #383838;
  border-radius: 6px; overflow: hidden; font-size: 12px;
}
.clbl {
  padding: 0 7px; color: #666; font-size: 11px;
  border-right: 1px solid #383838;
  display: flex; align-items: center; flex-shrink: 0;
}
.ctap {
  padding: 0 10px; min-width: 46px;
  display: flex; align-items: center; justify-content: center;
  cursor: pointer; user-select: none;
}
.ctap:active { background: #2a3a5a; }
.chip.on  .ctap { color: #70ffaa; }
.chip.off .ctap { color: #555; }
.chip.sus .ctap { color: #80d0ff; font-weight: bold; }
/* stepper chip (Vol, Trn) */
.cstep {
  width: 36px; display: flex; align-items: center; justify-content: center;
  cursor: pointer; color: #666; font-size: 17px; user-select: none;
  border-left: 1px solid #383838;
}
.cstep:active { background: #2a3a5a; color: #e0e0e0; }
.cval {
  padding: 0 8px; color: #e0e0e0; font-size: 12px; min-width: 50px;
  border-left: 1px solid #383838; border-right: 1px solid #383838;
  display: flex; align-items: center; justify-content: center;
}

/* ── Playback / recording bar ─────────────────────────────────────────── */
#pb {
  background: #1e2a1e; border-bottom: 1px solid #2a402a;
  padding: 8px 14px; flex-shrink: 0;
}
#pb.hidden { display: none; }
#pb-row1 { display: flex; align-items: center; gap: 10px; margin-bottom: 6px; }
#pb-icon { font-size: 18px; width: 22px; text-align: center; }
#pb-time { font-size: 12px; color: #aaa; white-space: nowrap; }
#pb-controls { display: flex; gap: 5px; margin-left: auto; }
.pb-btn {
  background: #2a3a2a; border: 1px solid #3a5a3a; color: #90e090;
  border-radius: 4px; padding: 5px 10px; cursor: pointer;
  font-size: 12px; font-family: inherit;
}
.pb-btn:active { background: #3a5a3a; }
#pb-bar-wrap {
  height: 6px; background: #2a2a2a; border-radius: 3px;
  cursor: pointer; position: relative;
}
#pb-fill {
  height: 100%; background: #50c050; border-radius: 3px;
  width: 0%; pointer-events: none; transition: width 100ms linear;
}
#rec-bar {
  background: #2a1e1e; border-bottom: 1px solid #5a2a2a;
  padding: 6px 14px; flex-shrink: 0; font-size: 12px;
  color: #e05050; display: none; align-items: center; gap: 8px;
}
#rec-bar.visible { display: flex; }

/* ── Instrument list ──────────────────────────────────────────────────── */
#mscroll { flex: 1; overflow-y: auto; -webkit-overflow-scrolling: touch; }
.slbl {
  padding: 8px 14px 3px; font-size: 11px; color: #555;
  letter-spacing: 1.5px; user-select: none;
}
.item {
  display: flex; align-items: center; gap: 8px;
  padding: 11px 14px; cursor: pointer; user-select: none;
  min-height: 48px; border-bottom: 1px solid #212121;
}
.item:hover  { background: #212121; }
.item:active { background: #1b2b40; }
.item.cursor { background: #1b2b40; box-shadow: inset 3px 0 0 #3a5599; }
.item.loaded  .iname { color: #70c8ff; }
.item.playing .iname { color: #70ffaa; }
.itype { width: 28px; font-size: 11px; color: #555; flex-shrink: 0; margin-right: 4px; }
.iname { flex: 1; font-size: 13px; }
.vara  { display: flex; align-items: center; gap: 3px; flex-shrink: 0; }
.vname { font-size: 11px; color: #777; min-width: 38px; text-align: center; }
.vbtn  {
  background: transparent; border: 1px solid #3a3a3a; color: #666;
  cursor: pointer; font-size: 14px; width: 36px; height: 36px;
  border-radius: 4px; display: flex; align-items: center; justify-content: center;
}
.vbtn:hover  { color: #ccc; border-color: #666; }
.vbtn:active { background: #2a3a5a; }

/* ── Bottom nav bar ───────────────────────────────────────────────────── */
#nav {
  display: flex; gap: 6px; padding: 8px 14px;
  border-top: 1px solid #262626; flex-shrink: 0; background: #252525;
}
.nbtn {
  background: #2e2e2e; border: 1px solid #383838; color: #e0e0e0;
  cursor: pointer; border-radius: 5px; height: 46px;
  display: flex; align-items: center; justify-content: center; user-select: none;
}
.nbtn:active { background: #2a3a5a; }
.nbtn-ico { width: 46px; font-size: 20px; }
#load-btn {
  flex: 1; background: #1b2b40; border-color: #3a5599;
  color: #80d0ff; font-size: 14px; font-weight: bold; letter-spacing: 0.5px;
}
#load-btn:active { background: #243558; }
#rec-btn.recording { background: #3a1a1a; border-color: #cc3333; color: #ff6060; }
.nbtn-txt { padding: 0 14px; font-size: 12px; color: #666; }

/* ── Settings side panel (desktop only) ───────────────────────────────── */
#side {
  width: 240px; flex-shrink: 0; padding: 12px 14px;
  display: flex; flex-direction: column; gap: 6px;
  overflow-y: auto; background: #1c1c1c;
}
@media (max-width: 699px) { #side { display: none; } }

.shdr { font-size: 11px; color: #555; letter-spacing: 1.5px; margin: 4px 0 2px; }
.srow { display: flex; align-items: center; gap: 6px; }
.slb2 { width: 72px; font-size: 12px; color: #777; flex-shrink: 0; }
.sval {
  flex: 1; background: #252525; border: 1px solid #383838;
  border-radius: 4px; padding: 9px 6px; font-size: 12px;
  cursor: pointer; text-align: center; color: #e0e0e0; user-select: none;
}
.sval:hover  { background: #2e2e2e; }
.sval:active { background: #2a3a5a; }
.sval.on  { color: #70ffaa; }
.sval.off { color: #555; }
.sval.sus { color: #80d0ff; font-weight: bold; }
.sval.ro  { cursor: default; pointer-events: none; }
.sbtn {
  background: #252525; border: 1px solid #383838; color: #777;
  border-radius: 4px; width: 38px; height: 38px; cursor: pointer;
  font-size: 18px; display: flex; align-items: center; justify-content: center;
  flex-shrink: 0; user-select: none;
}
.sbtn:hover  { background: #2e2e2e; color: #e0e0e0; }
.sbtn:active { background: #2a3a5a; }
.snum { flex: 1; text-align: center; font-size: 13px; padding: 4px; color: #e0e0e0; }
#sd-rescan {
  margin-top: 4px; padding: 10px; background: #252525;
  border: 1px solid #383838; border-radius: 4px;
  cursor: pointer; text-align: center; font-size: 12px; color: #666; user-select: none;
}
#sd-rescan:hover  { background: #2e2e2e; }
#sd-rescan:active { background: #2a3a5a; }
</style>
</head>
<body>

<div id="hdr">
  <h1>Pianeer</h1>
  <span id="status">Connecting&hellip;</span>
</div>

<div id="app">

  <!-- Main column: VU + chips + list + nav -->
  <div id="main">

    <div id="vu">
      <div class="vrow">
        <span class="vlbl">L</span>
        <div class="vtrk"><div class="vfill" id="vu-l"></div></div>
        <span class="vdb" id="vu-l-db">-&infin;&nbsp;dB</span>
        <span id="clip"></span>
      </div>
      <div class="vrow">
        <span class="vlbl">R</span>
        <div class="vtrk"><div class="vfill" id="vu-r"></div></div>
        <span class="vdb" id="vu-r-db">-&infin;&nbsp;dB</span>
      </div>
      <div id="stats"></div>
    </div>

    <!-- Settings chips (mobile only) — rebuilt by JS -->
    <div id="chips"></div>

    <!-- Recording bar (only visible while recording) -->
    <div id="rec-bar">
      <span>&#9679; REC</span>
      <span id="rec-time">00:00</span>
      <button class="pb-btn" data-cmd="ToggleRecord" style="margin-left:auto">&#9646;&#9646; Stop</button>
    </div>

    <!-- Playback bar -->
    <div id="pb" class="hidden">
      <div id="pb-row1">
        <span id="pb-icon">&#9654;</span>
        <span id="pb-time">00:00 / 00:00</span>
        <div id="pb-controls">
          <button class="pb-btn" data-json='{"cmd":"SeekRelative","secs":-10}'>&#8722;10s</button>
          <button class="pb-btn" id="pb-pause" data-cmd="PauseResume">&#9646;&#9646;</button>
          <button class="pb-btn" data-json='{"cmd":"SeekRelative","secs":10}'>+10s</button>
        </div>
      </div>
      <div id="pb-bar-wrap">
        <div id="pb-fill"></div>
      </div>
    </div>

    <div id="mscroll"><div id="menu"></div></div>

    <div id="nav">
      <button class="nbtn nbtn-ico" data-cmd="CursorUp">&#8593;</button>
      <button class="nbtn nbtn-ico" data-cmd="CursorDown">&#8595;</button>
      <button class="nbtn" id="load-btn" data-cmd="Select">Load</button>
      <button class="nbtn nbtn-txt" id="rec-btn" data-cmd="ToggleRecord">&#9679; Rec</button>
      <button class="nbtn nbtn-txt" data-cmd="Rescan">Rescan</button>
    </div>

  </div><!-- #main -->

  <!-- Settings side panel (desktop only) -->
  <div id="side">
    <div class="shdr">SETTINGS</div>
    <div class="srow">
      <span class="slb2">Velocity</span>
      <div class="sval" id="sd-vel" data-cmd="CycleVeltrack"></div>
    </div>
    <div class="srow">
      <span class="slb2">Tune</span>
      <div class="sval" id="sd-tune" data-cmd="CycleTune"></div>
    </div>
    <div class="srow">
      <span class="slb2">Release</span>
      <div class="sval" id="sd-rel" data-cmd="ToggleRelease"></div>
    </div>
    <div class="srow">
      <span class="slb2">Resonance</span>
      <div class="sval" id="sd-res" data-cmd="ToggleResonance"></div>
    </div>
    <div class="srow">
      <span class="slb2">Sustain</span>
      <div class="sval ro" id="sd-sus"></div>
    </div>
    <div class="shdr" style="margin-top:8px">VOLUME</div>
    <div class="srow">
      <button class="sbtn" data-json='{"cmd":"VolumeChange","delta":-1}'>&#8722;</button>
      <div class="snum" id="sd-vol">0&nbsp;dB</div>
      <button class="sbtn" data-json='{"cmd":"VolumeChange","delta":1}'>+</button>
    </div>
    <div class="shdr" style="margin-top:6px">TRANSPOSE</div>
    <div class="srow">
      <button class="sbtn" data-json='{"cmd":"TransposeChange","delta":-1}'>&#8722;</button>
      <div class="snum" id="sd-trn">0</div>
      <button class="sbtn" data-json='{"cmd":"TransposeChange","delta":1}'>+</button>
    </div>
    <button id="sd-rescan" data-cmd="Rescan">Rescan instruments</button>
    <button id="sd-rec" data-cmd="ToggleRecord" style="margin-top:4px;padding:10px;background:#252525;border:1px solid #383838;border-radius:4px;cursor:pointer;text-align:center;font-size:12px;color:#e05050;font-family:inherit;">&#9679;&nbsp;Record</button>
  </div><!-- #side -->

</div><!-- #app -->

<script>
'use strict';
const CERT_HASH = new Uint8Array(__CERT_HASH__);
const enc = new TextEncoder(), dec = new TextDecoder();
let doSend = null;

function sendRaw(obj) { if (doSend) doSend(obj); }
function sendCmd(c)   { sendRaw({ cmd: c }); }

// ── Event delegation (works across innerHTML rebuilds) ─────────────────────
document.addEventListener('click', e => {
  const el = e.target.closest('[data-cmd],[data-json]');
  if (!el) return;
  const c = el.dataset.cmd, j = el.dataset.json;
  if (c) sendCmd(c); else if (j) sendRaw(JSON.parse(j));
});

// Menu: pointerdown for variant arrows (beats the 100ms re-render),
//       click for row selection.
const menuEl = document.getElementById('menu');
menuEl.addEventListener('pointerdown', e => {
  const btn = e.target.closest('[data-varidx]');
  if (!btn) return;
  e.preventDefault();
  sendRaw({ cmd: 'CycleVariantAt', idx: +btn.dataset.varidx, dir: +btn.dataset.vardir });
});
menuEl.addEventListener('click', e => {
  if (e.target.closest('[data-varidx]')) return;
  const row = e.target.closest('[data-selidx]');
  if (row) sendRaw({ cmd: 'SelectAt', idx: +row.dataset.selidx });
});

// ── Keyboard shortcuts ─────────────────────────────────────────────────────
document.addEventListener('keydown', e => {
  if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
  const k = e.key;
  if      (k === 'ArrowUp')        { e.preventDefault(); sendCmd('CursorUp'); }
  else if (k === 'ArrowDown')      { e.preventDefault(); sendCmd('CursorDown'); }
  else if (k === 'ArrowLeft')      sendRaw({ cmd: 'CycleVariant', dir: -1 });
  else if (k === 'ArrowRight')     sendRaw({ cmd: 'CycleVariant', dir: 1 });
  else if (k === 'Enter')          sendCmd('Select');
  else if (k === 'v' || k === 'V') sendCmd('CycleVeltrack');
  else if (k === 't' || k === 'T') sendCmd('CycleTune');
  else if (k === 'e' || k === 'E') sendCmd('ToggleRelease');
  else if (k === '+' || k === '=') sendRaw({ cmd: 'VolumeChange',    delta: 1 });
  else if (k === '-')              sendRaw({ cmd: 'VolumeChange',    delta: -1 });
  else if (k === '[')              sendRaw({ cmd: 'TransposeChange', delta: -1 });
  else if (k === ']')              sendRaw({ cmd: 'TransposeChange', delta: 1 });
  else if (k === 'h' || k === 'H') sendCmd('ToggleResonance');
  else if (k === 'w' || k === 'W') sendCmd('ToggleRecord');
  else if (k === ' ')              { e.preventDefault(); sendCmd('PauseResume'); }
  else if (k === ',')              sendRaw({ cmd: 'SeekRelative', secs: -10 });
  else if (k === '.')              sendRaw({ cmd: 'SeekRelative', secs:  10 });
  else if (k === 'r' || k === 'R') sendCmd('Rescan');
});

// ── Seekbar click → seek ───────────────────────────────────────────────────
document.getElementById('pb-bar-wrap').addEventListener('click', e => {
  const rect = e.currentTarget.getBoundingClientRect();
  const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  const totalUs = renderState._totalUs || 0;
  const secs = Math.round((frac * totalUs / 1_000_000) - (totalUs / 2_000_000));
  // Compute absolute position and send as SeekRelative from current
  const curUs = renderState._curUs || 0;
  const targetUs = frac * totalUs;
  const deltaSecs = Math.round((targetUs - curUs) / 1_000_000);
  sendRaw({ cmd: 'SeekRelative', secs: deltaSecs });
});

// ── VU helpers ─────────────────────────────────────────────────────────────
function vuPct(peak) {
  if (peak < 1e-10) return 0;
  return Math.max(0, Math.min(100, ((20 * Math.log10(peak) + 48) / 48) * 100));
}
function vuColor(pct) {
  return pct > 95 ? '#cc3333' : pct > 75 ? '#ccaa22' : '#2a8040';
}
function vuDb(peak) {
  return peak < 1e-10 ? '-\u221e\u00a0dB' : (20 * Math.log10(peak)).toFixed(1) + '\u00a0dB';
}
function esc(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

// ── Chip builders ──────────────────────────────────────────────────────────
function mkChip(lbl, val, cmd, cls) {
  return `<span class="chip ${cls}">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="ctap" data-cmd="${cmd}">${esc(String(val))}</span>` +
    `</span>`;
}
function mkChipRo(lbl, val, cls) {
  return `<span class="chip ${cls}">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="ctap" style="cursor:default">${esc(String(val))}</span>` +
    `</span>`;
}
function mkStepper(lbl, val, jMinus, jPlus) {
  return `<span class="chip">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="cstep" data-json='${jMinus}'>&#8722;</span>` +
    `<span class="cval">${esc(String(val))}</span>` +
    `<span class="cstep" data-json='${jPlus}'>+</span>` +
    `</span>`;
}

function fmtTime(us) {
  const s = Math.floor(us / 1_000_000);
  return String(Math.floor(s/60)).padStart(2,'0') + ':' + String(s%60).padStart(2,'0');
}

// ── Main render ────────────────────────────────────────────────────────────
function renderState(st) {
  const s = st.settings, stats = st.stats;
  const vol = s.volume_db === 0 ? '0\u00a0dB'
    : (s.volume_db > 0 ? '+' : '') + s.volume_db.toFixed(0) + '\u00a0dB';
  const trn = s.transpose === 0 ? '0'
    : (s.transpose > 0 ? '+' : '') + s.transpose;

  // VU
  function setVU(fId, dId, peak) {
    const f = document.getElementById(fId), pct = vuPct(peak);
    if (f) { f.style.width = pct + '%'; f.style.background = vuColor(pct); }
    const d = document.getElementById(dId);
    if (d) d.textContent = vuDb(peak);
  }
  setVU('vu-l', 'vu-l-db', stats.peak_l);
  setVU('vu-r', 'vu-r-db', stats.peak_r);
  document.getElementById('clip').textContent = stats.clip ? '[CLIP]' : '';
  document.getElementById('stats').innerHTML =
    `<span>CPU\u00a0${stats.cpu_pct}%</span>` +
    `<span>Mem\u00a0${stats.mem_mb}\u00a0MB</span>` +
    `<span>Voices\u00a0${stats.voices}</span>`;

  // Recording bar + nav/side buttons
  const recBar = document.getElementById('rec-bar');
  if (recBar) recBar.classList.toggle('visible', !!st.recording);
  const recTimeEl = document.getElementById('rec-time');
  if (recTimeEl && st.recording) {
    recTimeEl.textContent = st.rec_elapsed_us != null ? fmtTime(st.rec_elapsed_us) : '00:00';
  }
  const recBtn = document.getElementById('rec-btn');
  if (recBtn) {
    recBtn.classList.toggle('recording', !!st.recording);
    recBtn.innerHTML = st.recording ? '&#9646;&#9646;&nbsp;Stop' : '&#9679;&nbsp;Rec';
  }
  const sdRec = document.getElementById('sd-rec');
  if (sdRec) {
    sdRec.textContent = st.recording ? '\u23F8 Stop Rec' : '\u25CF Record';
    sdRec.style.color  = st.recording ? '#ff6060' : '#e05050';
    sdRec.style.background = st.recording ? '#3a1a1a' : '#252525';
  }

  // Playback bar
  const pbEl = document.getElementById('pb');
  if (pbEl) {
    const pb = st.playback;
    if (pb && pb.total_us > 0) {
      pbEl.classList.remove('hidden');
      renderState._curUs   = pb.current_us;
      renderState._totalUs = pb.total_us;
      const pct = Math.min(100, pb.current_us / pb.total_us * 100);
      const fill = document.getElementById('pb-fill');
      if (fill) fill.style.width = pct + '%';
      const timeEl = document.getElementById('pb-time');
      if (timeEl) timeEl.textContent = fmtTime(pb.current_us) + ' / ' + fmtTime(pb.total_us);
      const icon = document.getElementById('pb-icon');
      if (icon) icon.textContent = pb.paused ? '\u23F8' : '\u25B6';
      const pauseBtn = document.getElementById('pb-pause');
      if (pauseBtn) pauseBtn.innerHTML = pb.paused ? '&#9654;' : '&#9646;&#9646;';
    } else {
      pbEl.classList.add('hidden');
      renderState._curUs   = 0;
      renderState._totalUs = 0;
    }
  }

  // Desktop side panel
  function sd(id, text, cls) {
    const el = document.getElementById(id);
    if (!el) return;
    el.textContent = text;
    el.className = 'sval ' + cls + (el.classList.contains('ro') ? ' ro' : '');
  }
  sd('sd-vel',  s.veltrack,                        '');
  sd('sd-tune', s.tune,                            '');
  sd('sd-rel',  s.release_enabled   ? 'on':'off',  s.release_enabled   ? 'on':'off');
  sd('sd-res',  s.resonance_enabled ? 'on':'off',  s.resonance_enabled ? 'on':'off');
  sd('sd-sus',  st.sustain          ? 'ON':'off',  st.sustain          ? 'sus':'off');
  const sdVol = document.getElementById('sd-vol');
  if (sdVol) sdVol.textContent = vol;
  const sdTrn = document.getElementById('sd-trn');
  if (sdTrn) sdTrn.textContent = trn;

  // Mobile chips — only rebuild when settings/sustain change
  const ck = s.veltrack + '|' + s.tune + '|' + s.release_enabled + '|' +
             s.resonance_enabled + '|' + s.volume_db + '|' + s.transpose + '|' + st.sustain;
  if (ck !== renderState._ck) {
    renderState._ck = ck;
    const relC = s.release_enabled   ? 'on' : 'off';
    const resC = s.resonance_enabled ? 'on' : 'off';
    const susC = st.sustain          ? 'sus': 'off';
    document.getElementById('chips').innerHTML =
      mkChip('Vel',  s.veltrack,                        'CycleVeltrack',   '') +
      mkChip('Tune', s.tune,                            'CycleTune',       '') +
      mkChip('Rel',  s.release_enabled   ? 'on':'off',  'ToggleRelease',   relC) +
      mkChip('Res',  s.resonance_enabled ? 'on':'off',  'ToggleResonance', resC) +
      mkStepper('Vol', vol,
        '{"cmd":"VolumeChange","delta":-1}',
        '{"cmd":"VolumeChange","delta":1}') +
      mkStepper('Trn', trn,
        '{"cmd":"TransposeChange","delta":-1}',
        '{"cmd":"TransposeChange","delta":1}') +
      mkChipRo('Sus', st.sustain ? 'ON' : 'off', susC);
  }

  // Menu — only rebuild when list content changes
  const mk = JSON.stringify(st.menu);
  if (mk !== renderState._mk) {
    renderState._mk = mk;
    let html = '', inMidi = false, shownInst = false;
    st.menu.forEach((item, idx) => {
      if (item.type_label !== 'MID' && !shownInst) {
        html += '<div class="slbl">INSTRUMENTS</div>';
        shownInst = true;
      }
      if (item.type_label === 'MID' && !inMidi) {
        html += '<div class="slbl">MIDI FILES</div>';
        inMidi = true;
      }
      const cls = ['item',
        item.cursor  ? 'cursor'  : '',
        item.loaded  ? 'loaded'  : '',
        item.playing ? 'playing' : '',
      ].filter(Boolean).join(' ');
      const varHtml = item.variant
        ? `<span class="vara">` +
          `<button class="vbtn" data-varidx="${idx}" data-vardir="-1">\u25c4</button>` +
          `<span class="vname">${esc(item.variant)}</span>` +
          `<button class="vbtn" data-varidx="${idx}" data-vardir="1">\u25ba</button>` +
          `</span>`
        : '';
      html +=
        `<div class="${cls}" data-selidx="${idx}">` +
        `<span class="itype">${esc(item.type_label)}</span>` +
        `<span class="iname">${esc(item.name)}</span>` +
        varHtml + `</div>`;
    });
    menuEl.innerHTML = html;
  }
}

// ── Transport ──────────────────────────────────────────────────────────────
async function connect() {
  doSend = null;
  const statusEl = document.getElementById('status');

  if (window.WebTransport) {
    statusEl.textContent = `WT ${window.location.hostname}:4433\u2026`;
    try {
      const transport = new WebTransport(`https://${window.location.hostname}:4433`, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: CERT_HASH }]
      });
      await transport.ready;
      statusEl.textContent = 'WebTransport';
      const bidiReader = transport.incomingBidirectionalStreams.getReader();
      const { value: bidi } = await bidiReader.read();
      const writer = bidi.writable.getWriter();
      doSend = obj => writer.write(enc.encode(JSON.stringify(obj) + '\n'));
      const stateReader = bidi.readable.getReader();
      let buf = '';
      while (true) {
        const { value, done } = await stateReader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        const lines = buf.split('\n');
        buf = lines.pop();
        for (const line of lines) {
          if (line.trim()) { try { renderState(JSON.parse(line)); } catch (_) {} }
        }
      }
      doSend = null;
      statusEl.textContent = 'Reconnecting\u2026';
      setTimeout(connect, 3000);
      return;
    } catch (_) { doSend = null; }
  }

  await new Promise(resolve => {
    const wsUrl = `ws://${window.location.hostname}:4000/ws`;
    statusEl.textContent = 'WS\u2026';
    const ws = new WebSocket(wsUrl);
    ws.onopen    = () => { statusEl.textContent = 'WebSocket'; doSend = obj => ws.send(JSON.stringify(obj)); };
    ws.onmessage = e => { try { renderState(JSON.parse(e.data)); } catch (_) {} };
    ws.onerror   = () => { doSend = null; };
    ws.onclose   = () => { doSend = null; resolve(); };
  });

  statusEl.textContent = 'Reconnecting\u2026';
  setTimeout(connect, 3000);
}

connect();
</script>
</body>
</html>
"#;
