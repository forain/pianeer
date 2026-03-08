// HTTP routes: API endpoints, WebSocket upgrades, and static file serving.

use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;

use axum::{
    Router,
    extract::{Host, ws::WebSocketUpgrade},
    http::header,
    response::IntoResponse,
    routing::get,
};
use crate::audio_stream::AudioStreamHandle;
use crate::types::MenuAction;
use super::ws::{handle_audio_ws, handle_ws};

static WASM_DIST: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../web-wasm/dist");

fn resolve_qr_url(host: &str) -> String {
    // If accessed via localhost, substitute the LAN IP so the QR code
    // is actually scannable from a phone on the same network.
    let is_local = host.starts_with("localhost")
        || host.starts_with("127.0.0.1")
        || host.starts_with("[::1]")
        || host == "::1";
    if is_local {
        if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
            if sock.connect("1.1.1.1:80").is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    return format!("http://{}:4000", addr.ip());
                }
            }
        }
    }
    format!("http://{}", host)
}

async fn serve_qr(Host(host): Host) -> impl IntoResponse {
    let url = resolve_qr_url(&host);
    let svg = match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(280, 280)
            .build(),
        Err(_) => r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#.to_string(),
    };
    ([(header::CONTENT_TYPE, "image/svg+xml")], svg)
}

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".wasm")      { "application/wasm" }
    else if path.ends_with(".js")   { "application/javascript" }
    else if path.ends_with(".html") { "text/html; charset=utf-8" }
    else if path.ends_with(".css")  { "text/css" }
    else if path.ends_with(".svg")  { "image/svg+xml" }
    else                            { "application/octet-stream" }
}

async fn serve_embedded(uri_path: &str) -> impl IntoResponse {
    use axum::http::StatusCode;
    let path = uri_path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WASM_DIST.get_file(path) {
        Some(f) => (
            [(header::CONTENT_TYPE, mime_for(path))],
            f.contents().to_vec(),
        ).into_response(),
        None => match WASM_DIST.get_file("index.html") {
            Some(f) => (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                f.contents().to_vec(),
            ).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

pub(super) async fn serve_http(
    cert_hash_json: String,
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
    audio: Arc<AudioStreamHandle>,
) {
    // Build API routes shared by both UI modes.
    let mut app = Router::new()
        .route("/qr", get(serve_qr))
        .route("/cert-hash", get({
            let hash = cert_hash_json.clone();
            move || async move {
                ([(header::CONTENT_TYPE, "application/json")], hash)
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
        }))
        .route("/audio-ws", get({
            let audio = Arc::clone(&audio);
            move |ws: WebSocketUpgrade| {
                let audio = Arc::clone(&audio);
                async move {
                    ws.on_upgrade(move |socket| handle_audio_ws(socket, audio))
                }
            }
        }))
        ;

    app = app.fallback(|req: axum::extract::Request| async move {
        serve_embedded(req.uri().path()).await
    });

    // Bind both IPv4 and IPv6 so `http://localhost:4000` works on macOS,
    // where browsers resolve `localhost` to ::1 (IPv6) first.
    // On Linux with dual-stack [::] covers both; the 0.0.0.0 bind may
    // fail with EADDRINUSE in that case — that's fine, we ignore it.
    let l4 = tokio::net::TcpListener::bind("0.0.0.0:4000").await.ok();
    let l6 = tokio::net::TcpListener::bind("[::]:4000").await.ok();

    if l4.is_none() && l6.is_none() {
        eprintln!("Web UI: failed to bind HTTP on :4000");
        return;
    }

    if let Some(l) = l6 {
        let a = app.clone();
        tokio::spawn(async move { axum::serve(l, a).await.ok(); });
    }
    if let Some(l) = l4 {
        tokio::spawn(async move { axum::serve(l, app).await.ok(); });
    }
}
