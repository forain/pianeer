use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use wtransport::{Endpoint, Identity, ServerConfig};

use crate::audio_stream::AudioStreamHandle;
use crate::types::MenuAction;

mod http;
mod wt;
mod ws;

pub fn spawn_web_server(
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
    audio: Arc<AudioStreamHandle>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for web server");
        rt.block_on(run(snapshot, action_tx, reload, audio));
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
    audio: Arc<AudioStreamHandle>,
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

    // Always start the HTTP server first so :4000 is reachable even if
    // WebTransport / QUIC fails to bind (e.g. firewall on macOS).
    eprintln!("Web UI:  http://localhost:4000  (Chrome/Edge 100+ required)");
    for san in &sans {
        if san != "localhost" && san != "127.0.0.1" && san != "::1" {
            eprintln!("Web UI:  http://{san}:4000");
        }
    }
    let snap_http = Arc::clone(&snapshot);
    tokio::spawn(http::serve_http(cert_hash_json, snap_http, action_tx.clone(), Arc::clone(&reload), Arc::clone(&audio)));

    let config = ServerConfig::builder()
        .with_bind_default(4433)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();

    let server = match Endpoint::server(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Web UI: WebTransport unavailable ({e}); using WebSocket fallback on :4000");
            // Park this future forever so the tokio runtime (and the HTTP
            // server task above) stays alive.
            std::future::pending::<()>().await;
            return;
        }
    };

    // Accept WebTransport sessions.
    loop {
        let incoming = server.accept().await;
        let snap = Arc::clone(&snapshot);
        let tx = action_tx.clone();
        let rl = Arc::clone(&reload);
        let aud = Arc::clone(&audio);
        tokio::spawn(async move {
            if let Err(e) = wt::handle_session(incoming, snap, tx, rl, aud).await {
                eprintln!("Web session error: {e}");
            }
        });
    }
}
