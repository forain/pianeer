// WebSocket handlers and command dispatch.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use tokio::sync::broadcast;

use crate::audio_stream::AudioStreamHandle;
use crate::snapshot::ClientCmd;
use crate::types::MenuAction;

pub(super) fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

pub(super) fn dispatch_cmd(
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
                Some(MenuAction::Rescan)
            }
            ClientCmd::RequestAudioInit => None, // handled at transport level
        };
        if let Some(a) = action {
            let _ = action_tx.try_send(a);
        }
    }
}

pub(super) async fn handle_ws(
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

pub(super) async fn handle_audio_pcm_ws(mut socket: WebSocket, audio: Arc<AudioStreamHandle>) {
    let mut rx = audio.pcm_frames.subscribe();
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if socket.send(Message::Binary((*frame).clone())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

pub(super) async fn handle_audio_ws(mut socket: WebSocket, audio: Arc<AudioStreamHandle>) {
    // Send stream header as the first binary message so the browser can configure
    // its AudioDecoder with the STREAMINFO payload (bytes [8..42]).
    if socket.send(Message::Binary((*audio.stream_header).clone())).await.is_err() {
        return;
    }
    let mut rx = audio.frames.subscribe();
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if socket.send(Message::Binary((*frame).clone())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {} // skip dropped frames
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
