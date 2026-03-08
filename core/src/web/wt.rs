// WebTransport session handler.

use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use crate::audio_stream::AudioStreamHandle;
use crate::types::MenuAction;
use super::ws::dispatch_cmd;

pub(super) async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    snapshot: Arc<Mutex<String>>,
    action_tx: crossbeam_channel::Sender<MenuAction>,
    reload: Arc<AtomicBool>,
    audio: Arc<AudioStreamHandle>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request: wtransport::endpoint::SessionRequest = incoming.await?;
    let conn: wtransport::Connection = request.accept().await?;

    let (mut send, recv): (wtransport::SendStream, wtransport::RecvStream) =
        conn.open_bi().await?.await?;

    // Datagram pusher: subscribe to the broadcast and forward as WT datagrams.
    // Datagrams are unreliable/unordered — late frames are simply dropped,
    // which is the right behaviour for low-latency audio monitoring.
    let conn2 = conn.clone();
    let mut audio_rx = audio.frames.subscribe();
    let datagram_task = tokio::spawn(async move {
        loop {
            match audio_rx.recv().await {
                Ok(frame) => {
                    if conn2.send_datagram(frame.as_ref()).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {} // skip dropped frames
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // State pusher: send JSON snapshot every 100 ms.
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

    // Reader: handle newline-delimited JSON commands.
    let mut lines = BufReader::new(recv).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        dispatch_cmd(trimmed, &action_tx, &reload);
    }

    pusher.abort();
    datagram_task.abort();
    Ok(())
}
