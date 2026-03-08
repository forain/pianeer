// WebSocket handlers and command dispatch.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use tokio::sync::broadcast;

use crate::audio_stream::AudioStreamHandle;
use crate::snapshot::ClientCmd;
use crate::types::MenuAction;

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

pub(super) async fn handle_audio_ws(mut socket: WebSocket, audio: Arc<AudioStreamHandle>) {
    let mut rx = audio.frames.subscribe();
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
