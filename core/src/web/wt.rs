// WebTransport session handler.

use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use crate::audio_stream::AudioStreamHandle;
use crate::types::MenuAction;
use super::ws::{b64, dispatch_cmd};

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

    // Send audio_init so the browser can configure its AudioDecoder.
    // The browser will start feeding datagrams once it receives this.
    let audio_init = format!(
        "{{\"type\":\"audio_init\",\"streaminfo\":\"{}\"}}\n",
        b64(&audio.stream_header[8..]) // bytes [8..42] = raw STREAMINFO payload
    );
    if send.write_all(audio_init.as_bytes()).await.is_err() {
        return Ok(());
    }

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
    let audio_for_reinit = Arc::clone(&audio);
    let pusher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut json = snap_push.lock().map(|v| v.clone()).unwrap_or_default();
            json.push('\n');
            if send.write_all(json.as_bytes()).await.is_err() {
                break;
            }
        }
        drop(audio_for_reinit); // keep Arc alive until pusher exits
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
