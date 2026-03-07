// pianeer-android: Android NDK entry point.
//
// Audio backend: Oboe (AAudio/OpenSL ES).
// UI: eframe with android-native-activity.
//
// Cross-compile:
//   cargo ndk -t arm64-v8a -o android/jniLibs build --release -p pianeer-android
//
// Package APK:
//   cargo apk build --release -p pianeer-android

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Global InMemoryDexClassLoader holding all compiled app Kotlin classes.
/// Initialized once at startup; used by any thread that needs to call app Kotlin code.
static APP_CLASS_LOADER: OnceLock<jni::objects::GlobalRef> = OnceLock::new();

use crossbeam_channel::bounded;
use pianeer_core::dispatch::{apply_settings_action, rebuild_menu};
use pianeer_core::loader::{LoadingJob, poll_loading_job, save_last_instrument_path, last_instrument_path};
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::StatsCollector;
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackState, Settings,
    stop_playback, playing_idx, pause_resume, seek_relative,
};
use pianeer_core::{instruments, midi_player, midi_recorder};
use pianeer_egui::{LocalBackend, PianeerApp, RemoteAudioPlayer};
#[cfg(target_os = "android")]
use oboe::Stereo;
use ringbuf::{HeapConsumer, HeapProducer};

#[cfg(target_os = "android")]
mod amidi;

const SAMPLES_DIR: &str = "/sdcard/Pianeer/samples";
const MIDI_DIR:    &str = "/sdcard/Pianeer/midi";
const STATE_FILE:  &str = "/sdcard/Pianeer/.last_instrument";


// ── Menu discovery ────────────────────────────────────────────────────────────

fn discover_menu() -> Vec<MenuItem> {
    let mut menu: Vec<MenuItem> =
        instruments::discover(&std::path::PathBuf::from(SAMPLES_DIR))
            .into_iter()
            .map(MenuItem::Instrument)
            .collect();
    let midi_dir = std::path::PathBuf::from(MIDI_DIR);
    if midi_dir.is_dir() {
        for (name, path) in midi_player::discover(&midi_dir) {
            menu.push(MenuItem::MidiFile { name, path });
        }
    }
    menu
}

// ── JNI: storage permission helpers ──────────────────────────────────────────

/// Returns true if this process has all-files access (or API < 30 where legacy
/// READ/WRITE_EXTERNAL_STORAGE is sufficient).
#[cfg(target_os = "android")]
fn is_storage_manager(app: &android_activity::AndroidApp) -> bool {
    fn try_check(app: &android_activity::AndroidApp) -> Option<bool> {
        use jni::JavaVM;
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _)
                .map_err(|e| log::error!("is_storage_manager vm: {e:?}"))
                .ok()?;
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| log::error!("is_storage_manager attach: {e:?}"))
                .ok()?;

            // Check SDK_INT — on API < 30 the legacy permissions are sufficient.
            let build_ver = env
                .find_class("android/os/Build$VERSION")
                .map_err(|e| log::error!("find Build$VERSION: {e:?}"))
                .ok()?;
            let sdk_int = env
                .get_static_field(&build_ver, "SDK_INT", "I")
                .map_err(|e| log::error!("SDK_INT: {e:?}"))
                .ok()?
                .i()
                .ok()?;
            if sdk_int < 30 {
                return Some(true);
            }

            // API 30+: check Environment.isExternalStorageManager()
            let env_class = env
                .find_class("android/os/Environment")
                .map_err(|e| log::error!("find Environment: {e:?}"))
                .ok()?;
            let result = env
                .call_static_method(&env_class, "isExternalStorageManager", "()Z", &[])
                .map_err(|e| log::error!("isExternalStorageManager: {e:?}"))
                .ok()?
                .z()
                .ok()?;
            Some(result)
        }
    }
    try_check(app).unwrap_or(false)
}

/// Load the app DEX (all compiled Kotlin classes) into an InMemoryDexClassLoader
/// and store it globally so any thread can call loadClass() on it.
#[cfg(target_os = "android")]
fn init_class_loader(app: &android_activity::AndroidApp) {
    fn try_init(app: &android_activity::AndroidApp) -> Option<()> {
        use jni::objects::{JObject, JValueGen};
        use jni::JavaVM;
        const DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/midi_opener.dex"));
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _)
                .map_err(|e| log::error!("init_class_loader vm: {e:?}")).ok()?;
            // Use get_env() because android-activity already permanently attached this thread;
            // calling attach_current_thread() would schedule an unwanted detach on drop.
            let mut env = vm.get_env()
                .or_else(|_| vm.attach_current_thread_permanently())
                .map_err(|e| log::error!("init_class_loader attach: {e:?}")).ok()?;
            let activity = JObject::from_raw(app.activity_as_ptr() as *mut _);

            let mut dex_copy = DEX.to_vec();
            let byte_buf = env.new_direct_byte_buffer(dex_copy.as_mut_ptr(), dex_copy.len())
                .map_err(|e| log::error!("init_class_loader ByteBuffer: {e:?}")).ok()?;
            let parent = env.call_method(
                &activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[],
            ).ok()?.l().ok()?;
            let imdcl = env.find_class("dalvik/system/InMemoryDexClassLoader").ok()?;
            let loader = env.new_object(
                &imdcl,
                "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
                &[JValueGen::Object(&byte_buf), JValueGen::Object(&parent)],
            ).map_err(|e| log::error!("init_class_loader new IMDCL: {e:?}")).ok()?;
            drop(dex_copy);
            let gref = env.new_global_ref(&loader)
                .map_err(|e| log::error!("init_class_loader global_ref: {e:?}")).ok()?;
            APP_CLASS_LOADER.set(gref).ok();
            log::info!("init_class_loader: DEX loaded ({} bytes)", DEX.len());
            Some(())
        }
    }
    if try_init(app).is_none() {
        log::error!("init_class_loader: failed");
    }
}

/// Load a named class from the app DEX using the global class loader.
#[cfg(target_os = "android")]
fn load_app_class<'a>(
    env: &mut jni::JNIEnv<'a>,
    name: &str,
) -> Option<jni::objects::JClass<'a>> {
    use jni::objects::{JClass, JValueGen};
    let loader = APP_CLASS_LOADER.get()?;
    let cls_name = env.new_string(name).ok()?;
    let cls_obj = env.call_method(
        loader, "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValueGen::Object(&cls_name)],
    ).ok()?.l().ok()?;
    Some(unsafe { JClass::from_raw(cls_obj.into_raw()) })
}

/// Acquire a SCREEN_BRIGHT_WAKE_LOCK so the display stays on while the app runs.
/// Uses PowerManager JNI — safe to call from any thread; no UI thread needed.
#[cfg(target_os = "android")]
fn keep_screen_on(app: &android_activity::AndroidApp) {
    fn try_keep(app: &android_activity::AndroidApp) -> Option<()> {
        use jni::objects::{JObject, JValueGen};
        use jni::JavaVM;
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _)
                .map_err(|e| log::error!("keep_screen_on vm: {e:?}"))
                .ok()?;
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| log::error!("keep_screen_on attach: {e:?}"))
                .ok()?;
            let activity = JObject::from_raw(app.activity_as_ptr() as *mut _);

            let svc_str = env.new_string("power")
                .map_err(|e| log::error!("new_string power: {e:?}"))
                .ok()?;
            let pm = env
                .call_method(
                    &activity,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValueGen::Object(&svc_str)],
                )
                .map_err(|e| log::error!("getSystemService: {e:?}"))
                .ok()?
                .l()
                .map_err(|e| log::error!("pm l(): {e:?}"))
                .ok()?;

            // SCREEN_BRIGHT_WAKE_LOCK (0xa) | ON_AFTER_RELEASE (0x20000000)
            let flags: i32 = 0x2000_000a;
            let tag = env.new_string("Pianeer:WakeLock")
                .map_err(|e| log::error!("new_string tag: {e:?}"))
                .ok()?;
            let wl = env
                .call_method(
                    &pm,
                    "newWakeLock",
                    "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
                    &[JValueGen::Int(flags), JValueGen::Object(&tag)],
                )
                .map_err(|e| log::error!("newWakeLock: {e:?}"))
                .ok()?
                .l()
                .map_err(|e| log::error!("wl l(): {e:?}"))
                .ok()?;
            env.call_method(&wl, "acquire", "()V", &[])
                .map_err(|e| log::error!("wl.acquire: {e:?}"))
                .ok()?;

            Some(())
        }
    }
    if try_keep(app).is_none() {
        log::warn!("keep_screen_on: failed to acquire wake lock");
    }
}


/// Open the system screen that lets the user grant MANAGE_EXTERNAL_STORAGE.
#[cfg(target_os = "android")]
fn request_storage_manager_permission(app: &android_activity::AndroidApp) {
    fn try_request(app: &android_activity::AndroidApp) -> Option<()> {
        use jni::objects::{JObject, JValueGen};
        use jni::JavaVM;
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _)
                .map_err(|e| log::error!("request_perm vm: {e:?}"))
                .ok()?;
            let mut env = vm
                .attach_current_thread()
                .map_err(|e| log::error!("request_perm attach: {e:?}"))
                .ok()?;
            let activity = JObject::from_raw(app.activity_as_ptr() as *mut _);

            // Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION
            let settings_class = env
                .find_class("android/provider/Settings")
                .map_err(|e| log::error!("find Settings: {e:?}"))
                .ok()?;
            let action = env
                .get_static_field(
                    &settings_class,
                    "ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION",
                    "Ljava/lang/String;",
                )
                .map_err(|e| log::error!("ACTION_MANAGE_APP: {e:?}"))
                .ok()?
                .l()
                .map_err(|e| log::error!("action l(): {e:?}"))
                .ok()?;

            // new Intent(action)
            let intent_class = env
                .find_class("android/content/Intent")
                .map_err(|e| log::error!("find Intent: {e:?}"))
                .ok()?;
            let intent = env
                .new_object(
                    &intent_class,
                    "(Ljava/lang/String;)V",
                    &[JValueGen::Object(&action)],
                )
                .map_err(|e| log::error!("new Intent: {e:?}"))
                .ok()?;

            // Uri.parse("package:com.pianeer.app")
            let uri_class = env
                .find_class("android/net/Uri")
                .map_err(|e| log::error!("find Uri: {e:?}"))
                .ok()?;
            let pkg_str = env
                .new_string("package:com.pianeer.app")
                .map_err(|e| log::error!("new_string pkg: {e:?}"))
                .ok()?;
            let uri = env
                .call_static_method(
                    &uri_class,
                    "parse",
                    "(Ljava/lang/String;)Landroid/net/Uri;",
                    &[JValueGen::Object(&pkg_str)],
                )
                .map_err(|e| log::error!("Uri.parse: {e:?}"))
                .ok()?
                .l()
                .map_err(|e| log::error!("uri l(): {e:?}"))
                .ok()?;

            // intent.setData(uri)
            env.call_method(
                &intent,
                "setData",
                "(Landroid/net/Uri;)Landroid/content/Intent;",
                &[JValueGen::Object(&uri)],
            )
            .map_err(|e| log::error!("setData: {e:?}"))
            .ok()?;

            // activity.startActivity(intent)
            env.call_method(
                &activity,
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValueGen::Object(&intent)],
            )
            .map_err(|e| log::error!("startActivity: {e:?}"))
            .ok()?;

            Some(())
        }
    }
    let _ = try_request(app);
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(android_app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    // Load app Java classes into a global class loader for use from any thread.
    init_class_loader(&android_app);
    // Keep screen on for the lifetime of the process.
    keep_screen_on(&android_app);

    // ── MIDI + action channels ────────────────────────────────────────────────
    let (midi_tx, midi_rx) = bounded::<MidiEvent>(4096);
    let (action_tx, action_rx) = bounded::<MenuAction>(16);

    // ── Shared snapshot (JSON string updated ~10 Hz) ──────────────────────────
    let web_snapshot: Arc<Mutex<String>> = Arc::new(Mutex::new("{}".to_string()));
    let snap_for_ui   = Arc::clone(&web_snapshot);
    let snap_for_loop = Arc::clone(&web_snapshot);

    // ── Sampler state (empty until first instrument loads) ───────────────────
    let state = Arc::new(Mutex::new(SamplerState::new(
        vec![], vec![], 48000.0, midi_rx,
        [0u8; 128], 0, 0, None,
    )));

    // ── Ring buffer for audio streaming tap (JACK callback → FLAC encoder) ───
    // 2 seconds of headroom at 48 kHz stereo.
    let (rb_prod, rb_cons) = ringbuf::HeapRb::<f32>::new(48000 * 2 * 2).split();
    state.lock().unwrap().audio_sink = Some(rb_prod);
    let audio_handle = std::sync::Arc::new(
        pianeer_core::audio_stream::start(rb_cons, 48000)
    );

    // ── Web server (Axum HTTP :4000 + WebTransport :4433) ────────────────────
    // Allows browsers on the same WiFi network to control the Android sampler.
    // MenuAction::Rescan from web clients flows through action_tx, same as egui.
    let reload_web = Arc::new(AtomicBool::new(false));
    let _web_thread = pianeer_core::web::spawn_web_server(
        Arc::clone(&web_snapshot),
        action_tx.clone(),
        Arc::clone(&reload_web),
        Arc::clone(&audio_handle),
    );

    // ── Shared MIDI recorder handle ───────────────────────────────────────────
    let record_handle = midi_recorder::new_handle();

    // ── Android MIDI API input (MidiManager; auto-reconnects) ─────────────────
    spawn_midi_api_thread(android_app.clone(), midi_tx.clone(), record_handle.clone());

    // ── Background: action dispatch + snapshot + instrument loading ───────────
    {
        let state      = Arc::clone(&state);
        let snap       = snap_for_loop;
        let app_bg     = android_app.clone();
        let midi_tx_bg = midi_tx.clone();
        let record_handle = record_handle;

        thread::spawn(move || {
            // auto_tune correction detected after each instrument load.
            // Stored as f32 bits for lock-free sharing with loading threads.
            let auto_tune_bits = Arc::new(AtomicU32::new(0f32.to_bits()));

            let mut settings        = Settings::default();
            let mut menu: Vec<MenuItem> = Vec::new();
            let mut cursor          = 0usize;
            let mut current         = 0usize;
            let mut current_path    = std::path::PathBuf::new();
            let mut playback: Option<PlaybackState> = None;
            let mut storage_ready   = false;
            let mut perm_requested  = false;
            let mut stats_collector = StatsCollector::new();
            let mut loading_job: Option<(usize, LoadingJob)> = None;

            loop {
                // ── 1. Storage permission gate ────────────────────────────────
                if !storage_ready {
                    if is_storage_manager(&app_bg) {
                        menu = discover_menu();
                        // Restore last session's instrument, or fall back to first.
                        let saved = last_instrument_path(std::path::Path::new(STATE_FILE));
                        let initial = saved.as_ref()
                            .and_then(|p| menu.iter().enumerate().find_map(|(i, m)| {
                                if let MenuItem::Instrument(inst) = m {
                                    if inst.path() == p.as_path() { Some((i, inst.clone())) } else { None }
                                } else { None }
                            }))
                            .or_else(|| menu.iter().enumerate().find_map(|(i, m)| {
                                if let MenuItem::Instrument(inst) = m { Some((i, inst.clone())) } else { None }
                            }));
                        if let Some((idx, inst)) = initial {
                            cursor = idx;
                            loading_job = Some((idx, LoadingJob::start(inst)));
                        }
                        storage_ready = true;
                    } else if !perm_requested {
                        request_storage_manager_permission(&app_bg);
                        perm_requested = true;
                    }
                }

                // ── 2. Drain actions ──────────────────────────────────────────
                let auto_tune = f32::from_bits(auto_tune_bits.load(Ordering::Relaxed));
                while let Ok(action) = action_rx.try_recv() {
                    match action {
                        MenuAction::CursorUp => {
                            if cursor > 0 { cursor -= 1; }
                        }
                        MenuAction::CursorDown => {
                            if cursor + 1 < menu.len() { cursor += 1; }
                        }
                        MenuAction::CycleVariant(dir) => {
                            if let Some(MenuItem::Instrument(inst)) = menu.get_mut(cursor) {
                                inst.cycle_variant(dir);
                            }
                        }
                        MenuAction::CycleVariantAt { idx, dir } => {
                            if let Some(MenuItem::Instrument(inst)) = menu.get_mut(idx) {
                                inst.cycle_variant(dir);
                            }
                        }
                        MenuAction::PauseResume => { pause_resume(&playback); }
                        MenuAction::SeekRelative(secs) => { seek_relative(&playback, secs); }
                        MenuAction::ToggleRecord => {
                            if midi_recorder::is_recording(&record_handle) {
                                if let Some(buf) = midi_recorder::stop(&record_handle) {
                                    let dir = std::path::Path::new(MIDI_DIR);
                                    let _ = std::fs::create_dir_all(dir);
                                    match midi_recorder::save(buf, dir) {
                                        Ok(p)  => log::info!("recording saved: {}", p.display()),
                                        Err(e) => log::error!("save failed: {e}"),
                                    }
                                    (menu, current, cursor, current_path) = rebuild_menu(
                                        std::path::Path::new(SAMPLES_DIR),
                                        Some(std::path::Path::new(MIDI_DIR)),
                                        &menu, current, cursor,
                                    );
                                }
                            } else {
                                midi_recorder::start(&record_handle);
                            }
                        }
                        MenuAction::Select => {
                            let idx = cursor;
                            if idx < menu.len() {
                                match &menu[idx] {
                                    MenuItem::Instrument(inst)
                                        if idx != current || inst.path() != current_path.as_path() =>
                                    {
                                        loading_job = Some((idx, LoadingJob::start(inst.clone())));
                                    }
                                    MenuItem::MidiFile { .. } => {
                                        android_select(idx, &mut menu, &midi_tx_bg, &mut playback);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        MenuAction::SelectAt(target) => {
                            cursor = target.min(menu.len().saturating_sub(1));
                            let idx = cursor;
                            if idx < menu.len() {
                                match &menu[idx] {
                                    MenuItem::Instrument(inst)
                                        if idx != current || inst.path() != current_path.as_path() =>
                                    {
                                        loading_job = Some((idx, LoadingJob::start(inst.clone())));
                                    }
                                    MenuItem::MidiFile { .. } => {
                                        android_select(idx, &mut menu, &midi_tx_bg, &mut playback);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        MenuAction::Rescan => {
                            (menu, current, cursor, current_path) = rebuild_menu(
                                std::path::Path::new(SAMPLES_DIR),
                                Some(std::path::Path::new(MIDI_DIR)),
                                &menu, current, cursor,
                            );
                        }
                        MenuAction::ShowQr => {} // no-op on Android
                        _ => { apply_settings_action(&action, &mut settings, auto_tune, &state); }
                    }
                }

                // ── 2b. Poll in-progress instrument load ─────────────────────
                if let Some((new_idx, new_path, new_at)) =
                    poll_loading_job(&mut loading_job, &state, &settings, &menu)
                {
                    current = new_idx;
                    save_last_instrument_path(std::path::Path::new(STATE_FILE), &new_path);
                    current_path = new_path;
                    auto_tune_bits.store(new_at.to_bits(), Ordering::Relaxed);
                }

                // ── 3+4. Stats ────────────────────────────────────────────────
                let (stats, sustain_now) = stats_collector.tick(&state);

                // ── 5. Playback bookkeeping ───────────────────────────────────
                if playback.as_ref().map_or(false, |p| p.done.load(Ordering::Relaxed)) {
                    playback = None;
                }
                let pb_info = playback
                    .as_ref()
                    .filter(|p| !p.done.load(Ordering::Relaxed))
                    .map(|p| p.info());
                let rec_elapsed  = midi_recorder::elapsed(&record_handle);
                let rec_active   = midi_recorder::is_recording(&record_handle);

                // ── 6. Build + publish snapshot ───────────────────────────────
                let loading_info = loading_job.as_ref().map(|(idx, job)| (*idx, job.pct()));
                let snap_obj = build_snapshot(
                    &menu, cursor, current,
                    playing_idx(&playback),
                    &settings, sustain_now, &stats,
                    rec_active,
                    rec_elapsed.map(|d| d.as_micros() as u64),
                    pb_info.as_ref(),
                    loading_info,
                );
                if let Ok(json) = serde_json::to_string(&snap_obj) {
                    if let Ok(mut s) = snap.lock() {
                        *s = json;
                    }
                }

                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    // ── Oboe audio stream ─────────────────────────────────────────────────────
    let (remote_prod, remote_cons) = ringbuf::HeapRb::<f32>::new(48000 * 2).split();
    let _audio = start_oboe_stream(Arc::clone(&state), remote_cons);

    // ── eframe native window (android-native-activity) ────────────────────────
    let backend    = LocalBackend::new(snap_for_ui, action_tx);
    // Determine the LAN URL so the QR button shows a scannable code pointing at
    // this device's web server.  Falls back to localhost if no LAN route found.
    let server_url = std::net::UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| { s.connect("1.1.1.1:80").ok()?; s.local_addr().ok() })
        .map(|a| format!("http://{}:4000", a.ip()))
        .unwrap_or_else(|| "http://localhost:4000".to_string());
    let mut inner  = PianeerApp::new(Box::new(backend), server_url);
    inner.set_remote_audio(Box::new(AndroidRemoteAudio::new(remote_prod)));
    let app        = InsetApp { inner, android_app: android_app.clone(), top_px: None, connect_dialog_open: false };

    let mut native_options = eframe::NativeOptions::default();
    native_options.event_loop_builder = Some(Box::new(move |builder| {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        builder.with_android_app(android_app);
    }));

    eframe::run_native(
        "Pianeer",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .unwrap_or_else(|e| log::error!("eframe error: {e}"));
}

// ── Android MIDI API input ────────────────────────────────────────────────────

/// Parse raw MIDI bytes (as delivered by the Android MIDI API) into MidiEvents.
/// Each call receives one or more complete MIDI messages concatenated.
fn parse_midi_bytes(data: &[u8]) -> Vec<MidiEvent> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let status = data[i];
        if status & 0x80 == 0 { i += 1; continue; } // ignore running-status bytes
        let msg_type = status & 0xF0;
        let channel  = status & 0x0F;
        match msg_type {
            0x80 if i + 2 < data.len() => {
                events.push(MidiEvent::NoteOff { channel, note: data[i + 1] & 0x7F });
                i += 3;
            }
            0x90 if i + 2 < data.len() => {
                let note = data[i + 1] & 0x7F;
                let vel  = data[i + 2] & 0x7F;
                if vel == 0 {
                    events.push(MidiEvent::NoteOff { channel, note });
                } else {
                    events.push(MidiEvent::NoteOn { channel, note, velocity: vel });
                }
                i += 3;
            }
            0xB0 if i + 2 < data.len() => {
                let ctl = data[i + 1] & 0x7F;
                let val = data[i + 2] & 0x7F;
                if ctl == 123 || ctl == 120 {
                    events.push(MidiEvent::AllNotesOff);
                } else if ctl != 1 {
                    events.push(MidiEvent::ControlChange { channel, controller: ctl, value: val });
                }
                i += 3;
            }
            0xF0 => { // SysEx — skip to 0xF7
                while i < data.len() && data[i] != 0xF7 { i += 1; }
                i += 1;
            }
            0xFE => { i += 1; } // Active Sensing — ignore
            _    => { i += 1; }
        }
    }
    events
}

/// Spawn a thread that connects to a MIDI device via the NDK AMidi API and
/// forwards events to the sampler.  Auto-reconnects every 2 seconds after
/// unplug.  MidiOpener (a tiny Java class embedded as DEX bytes) handles the
/// async openDevice() callback; AMidi handles all subsequent MIDI I/O natively.
#[cfg(target_os = "android")]
fn spawn_midi_api_thread(
    app:    android_activity::AndroidApp,
    tx:     crossbeam_channel::Sender<MidiEvent>,
    record: pianeer_core::midi_recorder::RecordHandle,
) {
    use amidi::*;
    use jni::objects::{JClass, JObject, JObjectArray, JValueGen};
    use jni::JavaVM;

    // DEX for MidiOpener.kt — compiled by build.rs, embedded at build time.
    // InMemoryDexClassLoader (API 26+) loads it without touching the filesystem.
    const OPENER_DEX: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/midi_opener.dex"));

    thread::spawn(move || {
        let vm = match unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) } {
            Ok(v) => v,
            Err(e) => { log::error!("midi: JavaVM error: {e:?}"); return; }
        };
        let activity_ptr = app.activity_as_ptr();
        log::info!("midi: AMidi thread started");

        loop {
            thread::sleep(Duration::from_secs(2));

            // Try to open a MIDI device via AMidi.
            // Returns (amidi_dev, amidi_port, dev_gref, opener_gref).
            // opener_gref holds the MidiOpener Java object whose `.removed` field
            // is set true by DeviceWatcher when the USB device is unplugged.
            let session = (|| -> Option<(*mut AMidiDevice, *mut AMidiOutputPort, jni::objects::GlobalRef, jni::objects::GlobalRef)> {
                let mut env = vm.attach_current_thread()
                    .map_err(|e| log::error!("midi: attach: {e:?}")).ok()?;
                // Clear any exception left over from a previous failed iteration
                // to prevent jni-rs from panicking on the next JNI call.
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
                let activity = unsafe { JObject::from_raw(activity_ptr as *mut _) };

                // Load MidiOpener via InMemoryDexClassLoader.
                // Use a Vec so new_direct_byte_buffer gets a mutable pointer;
                // IMDCL copies the bytes during construction.
                let mut dex_copy = OPENER_DEX.to_vec();
                let byte_buf = unsafe {
                    env.new_direct_byte_buffer(dex_copy.as_mut_ptr(), dex_copy.len())
                        .map_err(|e| log::error!("midi: ByteBuffer: {e:?}")).ok()?
                };
                let parent_loader = env.call_method(
                    &activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[],
                ).map_err(|e| log::error!("midi: getClassLoader: {e:?}")).ok()?.l().ok()?;
                let imdcl = env.find_class("dalvik/system/InMemoryDexClassLoader")
                    .map_err(|e| log::error!("midi: find InMemoryDexClassLoader: {e:?}")).ok()?;
                let loader = env.new_object(
                    &imdcl,
                    "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
                    &[JValueGen::Object(&byte_buf), JValueGen::Object(&parent_loader)],
                ).map_err(|e| log::error!("midi: new IMDCL: {e:?}")).ok()?;
                drop(dex_copy); // safe: IMDCL has copied the bytes

                let cls_name = env.new_string("com.pianeer.app.MidiOpener").ok()?;
                let cls_obj = env.call_method(
                    &loader, "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValueGen::Object(&cls_name)],
                ).map_err(|e| log::error!("midi: loadClass: {e:?}")).ok()?.l().ok()?;
                let opener_cls = unsafe { JClass::from_raw(cls_obj.into_raw()) };

                // Get MidiManager.
                let svc = env.new_string("midi").ok()?;
                let midi_mgr = env.call_method(
                    &activity, "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValueGen::Object(&svc)],
                ).map_err(|e| log::error!("midi: getSystemService: {e:?}")).ok()?.l().ok()?;
                if midi_mgr.is_null() {
                    log::error!("midi: MidiManager is null (MIDI unsupported?)");
                    return None;
                }

                // Find first device with an output port.
                let devs_obj = env.call_method(
                    &midi_mgr, "getDevices",
                    "()[Landroid/media/midi/MidiDeviceInfo;", &[],
                ).map_err(|e| log::error!("midi: getDevices: {e:?}")).ok()?.l().ok()?;
                let devs = unsafe { JObjectArray::from_raw(devs_obj.into_raw()) };
                let n = env.get_array_length(&devs).unwrap_or(0);
                log::info!("midi: {} devices", n);

                let mut info_gref: Option<jni::objects::GlobalRef> = None;
                for i in 0..n {
                    let info = env.get_object_array_element(&devs, i).ok()?;
                    let out_ports = env.call_method(
                        &info, "getOutputPortCount", "()I", &[],
                    ).ok()?.i().unwrap_or(0);
                    if out_ports > 0 {
                        info_gref = Some(env.new_global_ref(&info)
                            .map_err(|e| log::error!("midi: global_ref info: {e:?}")).ok()?);
                        break;
                    }
                }
                let info_gref = info_gref?;

                // Open device synchronously via MidiOpener.openSync().
                // Returns a MidiOpener object (not MidiDevice directly).
                let opener_obj = env.call_static_method(
                    &opener_cls, "openSync",
                    "(Landroid/media/midi/MidiManager;\
                      Landroid/media/midi/MidiDeviceInfo;)\
                      Lcom/pianeer/app/MidiOpener;",
                    &[JValueGen::Object(&midi_mgr), JValueGen::Object(info_gref.as_obj())],
                ).map_err(|e| log::error!("midi: openSync: {e:?}")).ok()?.l().ok()?;
                if opener_obj.is_null() {
                    log::error!("midi: openSync returned null");
                    return None;
                }
                let opener_gref = env.new_global_ref(&opener_obj)
                    .map_err(|e| log::error!("midi: global_ref opener: {e:?}")).ok()?;

                // Get the MidiDevice from the opener's .device field.
                let dev_obj = env.get_field(&opener_obj, "device", "Landroid/media/midi/MidiDevice;")
                    .map_err(|e| log::error!("midi: opener.device: {e:?}")).ok()?
                    .l().map_err(|e| log::error!("midi: opener.device l(): {e:?}")).ok()?;
                if dev_obj.is_null() {
                    log::error!("midi: openSync: device is null");
                    return None;
                }
                let dev_gref = env.new_global_ref(&dev_obj)
                    .map_err(|e| log::error!("midi: global_ref dev: {e:?}")).ok()?;

                // AMidiDevice_fromJava — bridge Java MidiDevice → native handle.
                let raw_env = env.get_raw() as *mut std::ffi::c_void;
                let raw_dev = dev_gref.as_obj().as_raw() as *mut std::ffi::c_void;
                let mut amidi_dev: *mut AMidiDevice = std::ptr::null_mut();
                let rc = unsafe { AMidiDevice_fromJava(raw_env, raw_dev, &mut amidi_dev) };
                if rc != AMEDIA_OK || amidi_dev.is_null() {
                    log::error!("midi: AMidiDevice_fromJava failed: {rc}");
                    return None;
                }

                // Open output port 0.
                let mut amidi_port: *mut AMidiOutputPort = std::ptr::null_mut();
                let rc = unsafe { AMidiOutputPort_open(amidi_dev, 0, &mut amidi_port) };
                if rc != AMEDIA_OK || amidi_port.is_null() {
                    log::error!("midi: AMidiOutputPort_open failed: {rc}");
                    unsafe { AMidiDevice_release(amidi_dev); }
                    return None;
                }

                log::info!("midi: connected via AMidi");
                Some((amidi_dev, amidi_port, dev_gref, opener_gref))
            })();

            let (amidi_dev, port, dev_gref, opener_gref) = match session {
                Some(s) => s,
                None    => continue,
            };

            // Native polling loop — no JNI overhead in hot path.
            // `handles_valid`: false when Android already freed the native AMidi handles
            // (i.e. AMidiOutputPort_receive returned an error).  In that case we must
            // NOT call AMidiOutputPort_close / AMidiDevice_release or we will crash.
            let mut handles_valid = true;
            let mut opcode: i32 = 0;
            let mut midi_buf = [0u8; 128];
            let mut n_bytes: usize = 0;
            let mut timestamp: i64 = 0;
            loop {
                // Check the Java DeviceWatcher flag BEFORE every receive call so we
                // stop polling as soon as Android signals removal — before native
                // handles are invalidated.  The JNI boolean read is ~1 µs; negligible
                // vs the 5 ms idle sleep below.
                let removed = vm.attach_current_thread().ok()
                    .and_then(|mut env| {
                        env.get_field(opener_gref.as_obj(), "removed", "Z")
                            .ok()?.z().ok()
                    })
                    .unwrap_or(false);
                if removed {
                    log::info!("midi: device removed (DeviceCallback), closing");
                    let _ = tx.send(MidiEvent::AllNotesOff);
                    break;
                }

                let result = unsafe {
                    AMidiOutputPort_receive(
                        port, &mut opcode,
                        midi_buf.as_mut_ptr(), midi_buf.len(),
                        &mut n_bytes, &mut timestamp,
                    )
                };
                if result < 0 {
                    // Android already freed the native handles — do NOT call
                    // AMidiOutputPort_close / AMidiDevice_release after this.
                    log::info!("midi: receive error {result}, reconnecting");
                    handles_valid = false;
                    let _ = tx.send(MidiEvent::AllNotesOff);
                    break;
                }
                if result == 0 || n_bytes == 0 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                if opcode == AMIDI_OPCODE_DATA {
                    for ev in parse_midi_bytes(&midi_buf[..n_bytes]) {
                        if let Ok(mut g) = record.lock() {
                            if let Some(ref mut buf) = *g {
                                buf.events.push((buf.start.elapsed(), ev.clone()));
                            }
                        }
                        let _ = tx.send(ev);
                    }
                }
            }

            // Only release native handles when we broke cleanly (DeviceCallback path).
            // If the receive returned an error, Android has already freed the handles
            // and calling close/release would crash.
            if handles_valid {
                unsafe {
                    AMidiOutputPort_close(port);
                    AMidiDevice_release(amidi_dev);
                }
            }
            // Always do the Java-side cleanup regardless.
            if let Ok(mut env) = vm.attach_current_thread() {
                if env.exception_check().unwrap_or(false) { let _ = env.exception_clear(); }
                let _ = env.call_method(opener_gref.as_obj(), "cleanup", "()V", &[]);
                if env.exception_check().unwrap_or(false) { let _ = env.exception_clear(); }
                let _ = env.call_method(dev_gref.as_obj(), "close", "()V", &[]);
                if env.exception_check().unwrap_or(false) { let _ = env.exception_clear(); }
            }
            log::info!("midi: session ended, will reconnect");
        }
    });
}

// ── Instrument / MIDI select helper ──────────────────────────────────────────

fn android_select(
    idx:     usize,
    menu:    &mut Vec<MenuItem>,
    midi_tx: &crossbeam_channel::Sender<MidiEvent>,
    playback: &mut Option<PlaybackState>,
) {
    if idx >= menu.len() {
        return;
    }
    match &menu[idx] {
        MenuItem::MidiFile { path, .. } => {
            let path           = path.clone();
            let already_playing = playback
                .as_ref()
                .map_or(false, |p| p.menu_idx == idx && !p.done.load(Ordering::Relaxed));
            stop_playback(playback);
            if !already_playing {
                let stop_flag   = Arc::new(AtomicBool::new(false));
                let done_flag   = Arc::new(AtomicBool::new(false));
                let paused_flag = Arc::new(AtomicBool::new(false));
                let cur_us      = Arc::new(AtomicU64::new(0));
                let seek_flag   = Arc::new(AtomicU64::new(u64::MAX));
                let (handle, total_us) = midi_player::spawn(
                    path,
                    midi_tx.clone(),
                    Arc::clone(&stop_flag),
                    Arc::clone(&done_flag),
                    Arc::clone(&paused_flag),
                    Arc::clone(&cur_us),
                    Arc::clone(&seek_flag),
                );
                *playback = Some(PlaybackState {
                    stop:       stop_flag,
                    done:       done_flag,
                    paused:     paused_flag,
                    current_us: cur_us,
                    total_us,
                    seek_to:    seek_flag,
                    _handle:    handle,
                    menu_idx:   idx,
                });
            }
        }
        _ => {} // same instrument re-selected, nothing to do
    }
}

// ── Status-bar inset wrapper ──────────────────────────────────────────────────
// Reserves a transparent top strip equal to the Android status bar height so
// the egui content starts below it rather than behind it.
//
// ANativeActivity::onContentRectChanged reports top=0 when the window draws
// behind the status bar, so we query the real pixel height via JNI instead:
//   Resources.getDimensionPixelSize(R.dimen.status_bar_height)

#[cfg(target_os = "android")]
fn status_bar_height_px(app: &android_activity::AndroidApp) -> Option<f32> {
    use jni::objects::{JObject, JValueGen};
    use jni::JavaVM;
    let result = unsafe {
        let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _)
            .map_err(|e| { log::error!("vm_from_raw: {e:?}"); e })
            .ok()?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| { log::error!("attach_thread: {e:?}"); e })
            .ok()?;
        let activity  = JObject::from_raw(app.activity_as_ptr() as *mut _);
        let resources = env
            .call_method(&activity, "getResources", "()Landroid/content/res/Resources;", &[])
            .map_err(|e| { log::error!("getResources: {e:?}"); e })
            .ok()?
            .l()
            .map_err(|e| { log::error!("getResources l(): {e:?}"); e })
            .ok()?;
        let name = env
            .new_string("status_bar_height")
            .map_err(|e| { log::error!("new_string name: {e:?}"); e })
            .ok()?;
        let kind = env
            .new_string("dimen")
            .map_err(|e| { log::error!("new_string kind: {e:?}"); e })
            .ok()?;
        let pkg = env
            .new_string("android")
            .map_err(|e| { log::error!("new_string pkg: {e:?}"); e })
            .ok()?;
        let id: i32 = env
            .call_method(
                &resources,
                "getIdentifier",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                &[JValueGen::Object(&name), JValueGen::Object(&kind), JValueGen::Object(&pkg)],
            )
            .map_err(|e| { log::error!("getIdentifier: {e:?}"); e })
            .ok()?
            .i()
            .map_err(|e| { log::error!("getIdentifier i(): {e:?}"); e })
            .ok()?;
        if id == 0 {
            log::error!("status_bar_height dimen id == 0");
            return None;
        }
        let px: i32 = env
            .call_method(&resources, "getDimensionPixelSize", "(I)I", &[JValueGen::Int(id)])
            .map_err(|e| { log::error!("getDimensionPixelSize: {e:?}"); e })
            .ok()?
            .i()
            .map_err(|e| { log::error!("getDimensionPixelSize i(): {e:?}"); e })
            .ok()?;
        log::info!("status_bar_height_px = {px}");
        Some(px as f32)
    };
    result
}

#[cfg(target_os = "android")]
struct InsetApp {
    inner:               PianeerApp,
    android_app:         android_activity::AndroidApp,
    /// Cached pixel height of the status bar (None until first successful JNI call).
    top_px:              Option<f32>,
    /// True while the connect dialog is open (prevents showing it again).
    connect_dialog_open: bool,
}

/// Show a system AlertDialog with an EditText for text input.
/// NativeActivity's SurfaceView has no IME connection, so showSoftInput always
/// fails. A real EditText in a dialog is the only reliable solution.
#[cfg(target_os = "android")]
fn show_connect_dialog(app: &android_activity::AndroidApp, current: &str) {
    fn try_show(app: &android_activity::AndroidApp, current: &str) -> Option<()> {
        use jni::objects::{JObject, JValueGen};
        use jni::JavaVM;
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _).ok()?;
            let mut env = vm.attach_current_thread().ok()?;
            let activity = JObject::from_raw(app.activity_as_ptr() as *mut _);
            let cls = load_app_class(&mut env, "com.pianeer.app.TextInputOverlay")?;
            let cur = env.new_string(current).ok()?;
            env.call_static_method(
                &cls,
                "showDialog",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValueGen::Object(&activity), JValueGen::Object(&cur)],
            )
            .ok()?;
            Some(())
        }
    }
    let _ = try_show(app, current);
}

/// Poll TextInputOverlay for a confirmed result. Returns Some(text) once.
#[cfg(target_os = "android")]
fn poll_connect_dialog(app: &android_activity::AndroidApp) -> Option<String> {
    fn try_poll(app: &android_activity::AndroidApp) -> Option<Option<String>> {
        use jni::JavaVM;
        unsafe {
            let vm = JavaVM::from_raw(app.vm_as_ptr() as *mut _).ok()?;
            let mut env = vm.attach_current_thread().ok()?;
            let cls = load_app_class(&mut env, "com.pianeer.app.TextInputOverlay")?;
            let result = env
                .call_static_method(&cls, "takePendingResult", "()Ljava/lang/String;", &[])
                .ok()?
                .l()
                .ok()?;
            if result.is_null() {
                return Some(None);
            }
            let s: String = env.get_string(&result.into()).ok()?.into();
            Some(Some(s))
        }
    }
    try_poll(app).flatten()
}

#[cfg(target_os = "android")]
impl eframe::App for InsetApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.top_px.is_none() {
            self.top_px = status_bar_height_px(&self.android_app);
        }
        if let Some(top_px) = self.top_px {
            if top_px > 0.0 {
                egui::TopBottomPanel::top("_status_bar_inset")
                    .exact_height(top_px / ctx.pixels_per_point())
                    .frame(egui::Frame::none())
                    .show(ctx, |_| {});
            }
        }
        self.inner.update(ctx, frame);

        // Poll for a confirmed result from the connect dialog.
        if let Some(text) = poll_connect_dialog(&self.android_app) {
            self.inner.handle_connect_result(text);
            self.connect_dialog_open = false;
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() { m.surrender_focus(id); }
            });
        }

        // Also trigger dialog when egui TextEdit gets focus (belt-and-suspenders).
        let wants_kbd = ctx.wants_keyboard_input();
        if wants_kbd && !self.connect_dialog_open {
            let current = self.inner.get_connect_input().to_string();
            show_connect_dialog(&self.android_app, &current);
            self.connect_dialog_open = true;
        }
        if !wants_kbd { self.connect_dialog_open = false; }

        // Floating "Connect" button — always visible, bypasses egui focus entirely.
        // The soft keyboard never works in NativeActivity without a real View.
        let is_connected = self.inner.is_remote();
        egui::Window::new("android_connect")
            .title_bar(false)
            .anchor(egui::Align2::LEFT_BOTTOM, [8.0, -72.0])
            .resizable(false)
            .show(ctx, |ui| {
                let label = if is_connected { "🌐 ✓ Connect" } else { "🌐 Connect" };
                let color = if is_connected {
                    egui::Color32::from_rgb(80, 192, 80)
                } else {
                    egui::Color32::from_gray(200)
                };
                if ui.button(egui::RichText::new(label).color(color)).clicked()
                    && !self.connect_dialog_open
                {
                    let current = self.inner.get_connect_input().to_string();
                    show_connect_dialog(&self.android_app, &current);
                    self.connect_dialog_open = true;
                }
            });
    }
}

// ── Oboe audio callback ───────────────────────────────────────────────────────

#[cfg(target_os = "android")]
fn start_oboe_stream(
    state: Arc<Mutex<SamplerState>>,
    remote_cons: HeapConsumer<f32>,
) -> oboe::AudioStreamAsync<oboe::Output, SamplerCallback> {
    use oboe::{AudioStream, AudioStreamBuilder, PerformanceMode, SharingMode};

    let callback = SamplerCallback { state, remote_cons };
    let mut stream = AudioStreamBuilder::default()
        .set_performance_mode(PerformanceMode::LowLatency)
        .set_sharing_mode(SharingMode::Exclusive)
        .set_stereo()
        .set_format::<f32>()
        .set_callback(callback)
        .open_stream()
        .expect("failed to open Oboe stream");
    stream.start().expect("failed to start Oboe stream");
    stream
}

#[cfg(target_os = "android")]
struct SamplerCallback {
    state:       Arc<Mutex<SamplerState>>,
    remote_cons: HeapConsumer<f32>,
}

#[cfg(target_os = "android")]
impl oboe::AudioOutputCallback for SamplerCallback {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        frames: &mut [(f32, f32)],
    ) -> oboe::DataCallbackResult {
        let n = frames.len();
        let mut buf_l = vec![0f32; n];
        let mut buf_r = vec![0f32; n];

        if let Ok(ref mut s) = self.state.try_lock() {
            s.process(&mut buf_l, &mut buf_r);
        }

        // Mix in remote audio: interleaved f32 stereo (L, R, L, R, …)
        let mut rbuf = vec![0f32; n * 2];
        let got = self.remote_cons.pop_slice(&mut rbuf);
        for i in 0..(got / 2) {
            buf_l[i] += rbuf[i * 2];
            buf_r[i] += rbuf[i * 2 + 1];
        }

        for (i, (l, r)) in frames.iter_mut().enumerate() {
            *l = buf_l[i];
            *r = buf_r[i];
        }

        oboe::DataCallbackResult::Continue
    }
}

// ── Remote audio streaming (PCM WebSocket → Oboe ring buffer) ─────────────────

struct AndroidRemoteAudio {
    connected: Arc<AtomicBool>,
    active:    Arc<AtomicBool>,
    ws_url:    Arc<Mutex<String>>,
}

impl AndroidRemoteAudio {
    fn new(prod: HeapProducer<f32>) -> Self {
        let connected = Arc::new(AtomicBool::new(false));
        let active    = Arc::new(AtomicBool::new(false));
        let ws_url    = Arc::new(Mutex::new(String::new()));

        let connected2 = Arc::clone(&connected);
        let active2    = Arc::clone(&active);
        let ws_url2    = Arc::clone(&ws_url);

        thread::Builder::new().name("remote-audio".into()).spawn(move || {
            remote_audio_loop(prod, connected2, active2, ws_url2);
        }).expect("spawn remote-audio");

        Self { connected, active, ws_url }
    }
}

impl RemoteAudioPlayer for AndroidRemoteAudio {
    fn connect(&mut self, pcm_ws_url: &str) {
        *self.ws_url.lock().unwrap() = pcm_ws_url.to_string();
        self.connected.store(true, Ordering::SeqCst);
    }

    fn disconnect(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

fn remote_audio_loop(
    mut prod:  HeapProducer<f32>,
    connected: Arc<AtomicBool>,
    active:    Arc<AtomicBool>,
    ws_url:    Arc<Mutex<String>>,
) {
    loop {
        // Wait until connect() is called.
        while !connected.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }

        let url = ws_url.lock().unwrap().clone();
        let (_sender, receiver) = match ewebsock::connect(&url, ewebsock::Options::default()) {
            Ok(pair) => pair,
            Err(_) => {
                connected.store(false, Ordering::SeqCst);
                continue;
            }
        };
        active.store(true, Ordering::SeqCst);

        while connected.load(Ordering::Relaxed) {
            while let Some(ev) = receiver.try_recv() {
                match ev {
                    ewebsock::WsEvent::Message(ewebsock::WsMessage::Binary(data)) => {
                        // data: i16 LE stereo — convert to f32 and push to ring buffer.
                        for chunk in data.chunks_exact(2) {
                            let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0;
                            let _ = prod.push(s);
                        }
                    }
                    ewebsock::WsEvent::Closed | ewebsock::WsEvent::Error(_) => {
                        connected.store(false, Ordering::SeqCst);
                        break;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(1));
        }

        active.store(false, Ordering::SeqCst);
    }
}

// Suppress unused warnings when not on Android
#[cfg(not(target_os = "android"))]
fn _not_android() {}
