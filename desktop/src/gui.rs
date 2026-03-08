// Native GUI path (feature = "native-ui").
// Extracted from main.rs to keep the startup orchestrator slim.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::Sender;

use pianeer_core::dispatch::run_dispatch_loop;
use pianeer_core::loader::LoadingJob;
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::sys_stats::lan_server_url;
use pianeer_core::types::MenuAction;
use pianeer_core::types::MenuItem;

#[cfg(feature = "native-ui")]
pub fn run_native_ui(
    menu_init:            Vec<MenuItem>,
    state:                Arc<Mutex<SamplerState>>,
    midi_tx:              Sender<MidiEvent>,
    record_handle:        pianeer_core::midi_recorder::RecordHandle,
    midi_dir:             Option<std::path::PathBuf>,
    samples_dir:          std::path::PathBuf,
    web_snapshot:         Arc<Mutex<String>>,
    action_tx:            Sender<MenuAction>,
    action_rx:            crossbeam_channel::Receiver<MenuAction>,
    auto_tune_init:       f32,
    quit:                 Arc<AtomicBool>,
    reload:               Arc<AtomicBool>,
    initial_loading_job:  Option<(usize, LoadingJob)>,
) {
    use pianeer_egui::{LocalBackend, PianeerApp};

    let snap_bg = Arc::clone(&web_snapshot);
    let quit_bg = Arc::clone(&quit);

    // Background thread: action dispatch + snapshot updates (no terminal I/O).
    thread::spawn(move || {
        run_dispatch_loop(
            menu_init, state, midi_tx, record_handle,
            midi_dir, samples_dir, snap_bg, action_rx,
            auto_tune_init, quit_bg, reload, initial_loading_job,
        );
    });

    // Run eframe on the main thread (blocks until window closed).
    let backend = LocalBackend::new(Arc::clone(&web_snapshot), action_tx.clone());
    let server_url = lan_server_url(4000);
    let peers = crate::discovery::start(4000);
    let mut app = PianeerApp::new(Box::new(backend), server_url);
    app.set_peers(peers);
    app.set_local_source(web_snapshot, action_tx);
    eframe::run_native(
        "Pianeer",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    ).unwrap_or_else(|e| eprintln!("eframe error: {e}"));

    // Window closed — wake background thread so it exits cleanly.
    quit.store(true, std::sync::atomic::Ordering::SeqCst);
}
