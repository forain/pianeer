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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use pianeer_core::loader::load_instrument_data;
use pianeer_core::sampler::{MidiEvent, SamplerState};
use pianeer_core::snapshot::build_snapshot;
use pianeer_core::sys_stats::{read_cpu_ticks, read_mem_mb};
use pianeer_core::types::{
    MenuAction, MenuItem, PlaybackInfo, PlaybackState, ProcStats, Settings,
    stop_playback, playing_idx,
};
use pianeer_core::{instruments, midi_player, midi_recorder};
use pianeer_egui::{LocalBackend, PianeerApp};
#[cfg(target_os = "android")]
use oboe::Stereo;

const SAMPLES_DIR: &str = "/sdcard/Pianeer/samples";
const MIDI_DIR: &str = "/sdcard/Pianeer/midi";


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
            let mut prev_cpu_ticks  = read_cpu_ticks().unwrap_or(0);
            let mut prev_cpu_instant = std::time::Instant::now();
            let mut clip_until: Option<std::time::Instant> = None;

            loop {
                // ── 1. Storage permission gate ────────────────────────────────
                if !storage_ready {
                    if is_storage_manager(&app_bg) {
                        menu = discover_menu();
                        // Load first instrument asynchronously.
                        if let Some(MenuItem::Instrument(inst)) = menu.first() {
                            let new_path    = inst.path().clone();
                            let inst_clone  = inst.clone();
                            let state2      = Arc::clone(&state);
                            let at_bits2    = Arc::clone(&auto_tune_bits);
                            let tune_semi   = settings.tune.semitones();
                            thread::spawn(move || {
                                match load_instrument_data(&inst_clone) {
                                    Ok(loaded) => {
                                        if let Ok(ref mut s) = state2.lock() {
                                            s.swap_instrument(
                                                loaded.regions, loaded.samples,
                                                loaded.cc_defaults,
                                                loaded.sw_lokey, loaded.sw_hikey,
                                                loaded.sw_default,
                                            );
                                            let new_at = s.detect_a4_tune_correction();
                                            s.master_tune_semitones = new_at + tune_semi;
                                            at_bits2.store(new_at.to_bits(), Ordering::Relaxed);
                                        }
                                    }
                                    Err(e) => log::error!("initial load: {e}"),
                                }
                            });
                            current      = 0;
                            current_path = new_path;
                        }
                        storage_ready = true;
                    } else if !perm_requested {
                        request_storage_manager_permission(&app_bg);
                        perm_requested = true;
                    }
                }

                // ── 2. Drain actions ──────────────────────────────────────────
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
                        MenuAction::CycleVeltrack => {
                            settings.veltrack = settings.veltrack.cycle();
                            if let Ok(ref mut s) = state.lock() {
                                s.veltrack_override = settings.veltrack.override_pct();
                            }
                        }
                        MenuAction::CycleTune => {
                            settings.tune = settings.tune.cycle();
                            let auto_tune = f32::from_bits(auto_tune_bits.load(Ordering::Relaxed));
                            if let Ok(ref mut s) = state.lock() {
                                s.master_tune_semitones = auto_tune + settings.tune.semitones();
                            }
                        }
                        MenuAction::ToggleRelease => {
                            settings.release_enabled = !settings.release_enabled;
                            if let Ok(ref mut s) = state.lock() {
                                s.release_enabled = settings.release_enabled;
                            }
                        }
                        MenuAction::VolumeChange(d) => {
                            settings.volume_db =
                                (settings.volume_db + d as f32 * 3.0).clamp(-12.0, 12.0);
                            if let Ok(ref mut s) = state.lock() {
                                s.master_volume = 10.0_f32.powf(settings.volume_db / 20.0);
                            }
                        }
                        MenuAction::TransposeChange(d) => {
                            settings.transpose =
                                (settings.transpose + d as i32).clamp(-12, 12);
                            if let Ok(ref mut s) = state.lock() {
                                s.transpose = settings.transpose;
                            }
                        }
                        MenuAction::ToggleResonance => {
                            settings.resonance_enabled = !settings.resonance_enabled;
                            if let Ok(ref mut s) = state.lock() {
                                s.resonance_enabled = settings.resonance_enabled;
                            }
                        }
                        MenuAction::PauseResume => {
                            if let Some(ref pb) = playback {
                                if !pb.done.load(Ordering::Relaxed) {
                                    let was = pb.paused.load(Ordering::Relaxed);
                                    pb.paused.store(!was, Ordering::Relaxed);
                                }
                            }
                        }
                        MenuAction::SeekRelative(secs) => {
                            if let Some(ref pb) = playback {
                                if !pb.done.load(Ordering::Relaxed) {
                                    let cur = pb.current_us.load(Ordering::Relaxed);
                                    let new_us = (cur as i64 + secs * 1_000_000i64)
                                        .clamp(0, pb.total_us as i64) as u64;
                                    pb.seek_to.store(new_us, Ordering::Relaxed);
                                }
                            }
                        }
                        MenuAction::ToggleRecord => {
                            if midi_recorder::is_recording(&record_handle) {
                                if let Some(buf) = midi_recorder::stop(&record_handle) {
                                    let dir = std::path::PathBuf::from(MIDI_DIR);
                                    let _ = std::fs::create_dir_all(&dir);
                                    match midi_recorder::save(buf, &dir) {
                                        Ok(p)  => log::info!("recording saved: {}", p.display()),
                                        Err(e) => log::error!("save failed: {e}"),
                                    }
                                    menu = discover_menu();
                                    cursor  = cursor.min(menu.len().saturating_sub(1));
                                    current = current.min(menu.len().saturating_sub(1));
                                }
                            } else {
                                midi_recorder::start(&record_handle);
                            }
                        }
                        MenuAction::Select => {
                            android_select(
                                cursor, &mut menu, &mut current, &mut current_path,
                                &auto_tune_bits, &settings, &state, &midi_tx_bg,
                                &mut playback,
                            );
                        }
                        MenuAction::SelectAt(target) => {
                            cursor = target.min(menu.len().saturating_sub(1));
                            android_select(
                                cursor, &mut menu, &mut current, &mut current_path,
                                &auto_tune_bits, &settings, &state, &midi_tx_bg,
                                &mut playback,
                            );
                        }
                        MenuAction::Rescan => {
                            let saved_path = match menu.get(current) {
                                Some(MenuItem::Instrument(i)) => Some(i.path().clone()),
                                _ => None,
                            };
                            menu = discover_menu();
                            current = saved_path
                                .and_then(|p| menu.iter().position(|m| {
                                    matches!(m, MenuItem::Instrument(i) if i.path() == &p)
                                }))
                                .unwrap_or(0);
                            cursor = cursor.min(menu.len().saturating_sub(1));
                            current_path = menu.get(current)
                                .and_then(|m| {
                                    if let MenuItem::Instrument(i) = m {
                                        Some(i.path().clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                        }
                        MenuAction::ShowQr => {} // no-op on Android
                    }
                }

                // ── 3. Poll sampler state ─────────────────────────────────────
                let (sustain_now, voices_now, peak_l, peak_r, clip_now) = state
                    .try_lock()
                    .map(|mut s| {
                        let clip = s.clip_l || s.clip_r;
                        s.clip_l = false;
                        s.clip_r = false;
                        (s.sustain_pedal, s.active_voice_count(), s.peak_l, s.peak_r, clip)
                    })
                    .unwrap_or((false, 0, 0.0, 0.0, false));

                // ── 4. CPU / memory stats ─────────────────────────────────────
                let now = std::time::Instant::now();
                if clip_now {
                    clip_until = Some(now + Duration::from_secs(2));
                }
                let clip_holding = clip_until.map(|t| now < t).unwrap_or(false);

                let elapsed = now.duration_since(prev_cpu_instant).as_secs_f32().max(0.001);
                let ticks_now = read_cpu_ticks().unwrap_or(prev_cpu_ticks);
                let cpu_pct = (((ticks_now.saturating_sub(prev_cpu_ticks)) as f32 / 100.0)
                    / elapsed
                    * 100.0)
                    .round()
                    .clamp(0.0, 100.0) as u8;
                prev_cpu_ticks = ticks_now;
                prev_cpu_instant = now;

                let stats = ProcStats {
                    cpu_pct,
                    mem_mb: read_mem_mb(),
                    voices: voices_now,
                    peak_l,
                    peak_r,
                    clip: clip_holding,
                };

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
                let snap_obj = build_snapshot(
                    &menu, cursor, current,
                    playing_idx(&playback),
                    &settings, sustain_now, &stats,
                    rec_active,
                    rec_elapsed.map(|d| d.as_micros() as u64),
                    pb_info.as_ref(),
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
    let _audio = start_oboe_stream(Arc::clone(&state));

    // ── eframe native window (android-native-activity) ────────────────────────
    let backend    = LocalBackend::new(snap_for_ui, action_tx);
    // Determine the LAN URL so the QR button shows a scannable code pointing at
    // this device's web server.  Falls back to localhost if no LAN route found.
    let server_url = std::net::UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| { s.connect("1.1.1.1:80").ok()?; s.local_addr().ok() })
        .map(|a| format!("http://{}:4000", a.ip()))
        .unwrap_or_else(|| "http://localhost:4000".to_string());
    let inner      = PianeerApp::new(Box::new(backend), server_url);
    let app        = InsetApp { inner, android_app: android_app.clone(), top_px: None };

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

/// Write a timestamped line to /sdcard/Pianeer/midi_debug.log (readable even
/// when logcat is unreachable over wireless ADB).
fn midi_log(msg: &str) {
    use std::io::Write;
    log::info!("midi: {}", msg);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open("/sdcard/Pianeer/midi_debug.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

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

/// Spawn a thread that connects to a MIDI device via the Android MIDI API
/// (MidiManager) and forwards events to the sampler.  Auto-reconnects every
/// 2 seconds after unplug.  Uses custom Java helpers (MidiHelper / MidiQueue)
/// injected as classes2.dex.
#[cfg(target_os = "android")]
fn spawn_midi_api_thread(
    app:    android_activity::AndroidApp,
    tx:     crossbeam_channel::Sender<MidiEvent>,
    record: pianeer_core::midi_recorder::RecordHandle,
) {
    use jni::objects::{JByteArray, JClass, JObject, JValueGen};
    use jni::JavaVM;

    thread::spawn(move || {
        let vm = match unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) } {
            Ok(v) => v,
            Err(e) => { midi_log(&format!("JavaVM error: {e:?}")); return; }
        };
        let activity_ptr = app.activity_as_ptr();
        midi_log("midi_api thread started");

        let mut poll_n: u32 = 0;
        loop {
            thread::sleep(Duration::from_secs(2));
            poll_n += 1;
            midi_log(&format!("poll #{poll_n}"));

            // ── Try to open a MIDI device via MidiHelper.connect() ────────────
            let queue_gref: Option<jni::objects::GlobalRef> = (|| -> Option<jni::objects::GlobalRef> {
                let mut env = vm.attach_current_thread()
                    .map_err(|e| midi_log(&format!("attach: {e:?}"))).ok()?;
                let activity = unsafe { JObject::from_raw(activity_ptr as *mut _) };

                // Build a DexClassLoader that reads classes.dex directly from
                // our APK.  We cannot use activity.getClassLoader() because
                // NativeActivity apps have an empty DexPathList — ART never
                // runs dex2oat for them since the original APK has no Java code.
                let apk_path = env.call_method(
                    &activity, "getPackageCodePath",
                    "()Ljava/lang/String;", &[],
                ).map_err(|e| midi_log(&format!("getPackageCodePath: {e:?}"))).ok()?.l().ok()?;

                // getCodeCacheDir() → optimized-dex output dir (API 21+)
                let code_cache_file = env.call_method(
                    &activity, "getCodeCacheDir",
                    "()Ljava/io/File;", &[],
                ).ok()?.l().ok()?;
                let cache_path = env.call_method(
                    &code_cache_file, "getAbsolutePath",
                    "()Ljava/lang/String;", &[],
                ).ok()?.l().ok()?;

                let parent_loader = env.call_method(
                    &activity, "getClassLoader",
                    "()Ljava/lang/ClassLoader;", &[],
                ).ok()?.l().ok()?;

                let dcl_class = env.find_class("dalvik/system/DexClassLoader")
                    .map_err(|e| midi_log(&format!("find DexClassLoader: {e:?}"))).ok()?;
                let null_str = JObject::null();
                let loader = env.new_object(
                    &dcl_class,
                    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;\
                      Ljava/lang/ClassLoader;)V",
                    &[
                        JValueGen::Object(&apk_path),
                        JValueGen::Object(&cache_path),
                        JValueGen::Object(&null_str),
                        JValueGen::Object(&parent_loader),
                    ],
                ).map_err(|e| midi_log(&format!("new DexClassLoader: {e:?}"))).ok()?;

                let cls_name = env.new_string("com.pianeer.app.MidiHelper").ok()?;
                let cls_obj = env.call_method(
                    &loader, "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValueGen::Object(&cls_name)],
                ).map_err(|e| midi_log(&format!("loadClass MidiHelper: {e:?}"))).ok()?.l().ok()?;
                if cls_obj.is_null() {
                    midi_log("MidiHelper class not found — was classes.dex injected?");
                    return None;
                }
                let cls = unsafe { JClass::from_raw(cls_obj.into_raw()) };

                // MidiHelper.connect(Context) -> MidiQueue
                let queue_obj = env.call_static_method(
                    &cls, "connect",
                    "(Landroid/content/Context;)Lcom/pianeer/app/MidiQueue;",
                    &[JValueGen::Object(&activity)],
                ).map_err(|e| midi_log(&format!("MidiHelper.connect: {e:?}"))).ok()?.l().ok()?;

                if queue_obj.is_null() {
                    midi_log("no MIDI device with output port found");
                    return None;
                }
                let gref = env.new_global_ref(&queue_obj)
                    .map_err(|e| midi_log(&format!("new_global_ref: {e:?}"))).ok()?;
                midi_log("MIDI device connected via Android MIDI API");
                Some(gref)
            })();

            let queue_gref = match queue_gref {
                Some(q) => q,
                None    => continue,
            };

            // ── Inner loop: poll MidiQueue every 100 ms ───────────────────────
            let mut iter: u32 = 0;
            loop {
                iter += 1;

                let result = (|| -> Option<Option<Vec<u8>>> {
                    let mut env = vm.attach_current_thread().ok()?;

                    // Every 20 iterations (~2 s) check if the device is still present.
                    if iter % 20 == 0 {
                        let activity = unsafe { JObject::from_raw(activity_ptr as *mut _) };
                        let apk_path = env.call_method(&activity, "getPackageCodePath", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
                        let code_cache_file = env.call_method(&activity, "getCodeCacheDir", "()Ljava/io/File;", &[]).ok()?.l().ok()?;
                        let cache_path = env.call_method(&code_cache_file, "getAbsolutePath", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
                        let parent_loader = env.call_method(&activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]).ok()?.l().ok()?;
                        let dcl_class = env.find_class("dalvik/system/DexClassLoader").ok()?;
                        let null_str = JObject::null();
                        let loader = env.new_object(&dcl_class,
                            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V",
                            &[JValueGen::Object(&apk_path), JValueGen::Object(&cache_path), JValueGen::Object(&null_str), JValueGen::Object(&parent_loader)],
                        ).ok()?;
                        let cls_name = env.new_string("com.pianeer.app.MidiHelper").ok()?;
                        let cls_obj = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;",
                            &[JValueGen::Object(&cls_name)]).ok()?.l().ok()?;
                        let cls = unsafe { JClass::from_raw(cls_obj.into_raw()) };
                        let present = env.call_static_method(
                            &cls, "isAnyDevicePresent",
                            "(Landroid/content/Context;)Z",
                            &[JValueGen::Object(&activity)],
                        ).ok()?.z().ok()?;
                        if !present {
                            midi_log("MIDI device no longer present — reconnecting");
                            return None; // signals outer loop to close + retry
                        }
                    }

                    // queue.poll(100L) -> byte[] or null
                    let bytes_obj = env.call_method(
                        queue_gref.as_obj(), "poll", "(J)[B",
                        &[JValueGen::Long(100)],
                    ).map_err(|e| {
                        if env.exception_check().unwrap_or(false) {
                            let _ = env.exception_clear();
                        }
                        midi_log(&format!("queue.poll exception: {e:?}"));
                    }).ok()?.l().ok()?;

                    if bytes_obj.is_null() {
                        return Some(None); // timeout, no data
                    }

                    let j_arr = unsafe { JByteArray::from_raw(bytes_obj.into_raw()) };
                    let len   = env.get_array_length(&j_arr).ok()? as usize;
                    let mut buf = vec![0i8; len];
                    env.get_byte_array_region(&j_arr, 0, &mut buf).ok()?;
                    let data: Vec<u8> = buf.iter().map(|&b| b as u8).collect();
                    Some(Some(data))
                })();

                match result {
                    None => break, // JNI error or device gone — reconnect
                    Some(None) => {} // poll timeout — keep looping
                    Some(Some(data)) => {
                        midi_log(&format!("rx {} bytes: {:02x?}", data.len(), &data));
                        let events = parse_midi_bytes(&data);
                        midi_log(&format!("parsed {} events: {:?}", events.len(), &events));
                        for ev in events {
                            if let Ok(mut g) = record.lock() {
                                if let Some(ref mut buf) = *g {
                                    buf.events.push((buf.start.elapsed(), ev.clone()));
                                }
                            }
                            let _ = tx.send(ev);
                        }
                    }
                }
            }

            // ── Close queue before reconnect attempt ──────────────────────────
            if let Ok(mut env) = vm.attach_current_thread() {
                let _ = env.call_method(queue_gref.as_obj(), "close", "()V", &[]);
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
            }
            midi_log("MIDI session ended, will reconnect");
        }
    });
}

// ── Instrument / MIDI select helper ──────────────────────────────────────────

fn android_select(
    idx:            usize,
    menu:           &mut Vec<MenuItem>,
    current:        &mut usize,
    current_path:   &mut std::path::PathBuf,
    auto_tune_bits: &Arc<AtomicU32>,
    settings:       &Settings,
    state:          &Arc<Mutex<SamplerState>>,
    midi_tx:        &crossbeam_channel::Sender<MidiEvent>,
    playback:       &mut Option<PlaybackState>,
) {
    if idx >= menu.len() {
        return;
    }
    match &menu[idx] {
        MenuItem::Instrument(inst)
            if idx != *current || inst.path() != current_path.as_path() =>
        {
            let new_path   = inst.path().clone();
            let inst_clone = inst.clone();
            let state2     = Arc::clone(state);
            let at_bits2   = Arc::clone(auto_tune_bits);
            let tune_semi  = settings.tune.semitones();
            thread::spawn(move || {
                match load_instrument_data(&inst_clone) {
                    Ok(loaded) => {
                        if let Ok(ref mut s) = state2.lock() {
                            s.swap_instrument(
                                loaded.regions, loaded.samples,
                                loaded.cc_defaults,
                                loaded.sw_lokey, loaded.sw_hikey, loaded.sw_default,
                            );
                            let new_at = s.detect_a4_tune_correction();
                            s.master_tune_semitones = new_at + tune_semi;
                            at_bits2.store(new_at.to_bits(), Ordering::Relaxed);
                        }
                    }
                    Err(e) => log::error!("load instrument: {e}"),
                }
            });
            *current      = idx;
            *current_path = new_path;
        }
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
    inner:       PianeerApp,
    android_app: android_activity::AndroidApp,
    /// Cached pixel height of the status bar (None until first successful JNI call).
    top_px:      Option<f32>,
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
    }
}

// ── Oboe audio callback ───────────────────────────────────────────────────────

#[cfg(target_os = "android")]
fn start_oboe_stream(
    state: Arc<Mutex<SamplerState>>,
) -> oboe::AudioStreamAsync<oboe::Output, SamplerCallback> {
    use oboe::{AudioStream, AudioStreamBuilder, PerformanceMode, SharingMode};

    let callback = SamplerCallback { state };
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
    state: Arc<Mutex<SamplerState>>,
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

        for (i, (l, r)) in frames.iter_mut().enumerate() {
            *l = buf_l[i];
            *r = buf_r[i];
        }

        oboe::DataCallbackResult::Continue
    }
}

// Suppress unused warnings when not on Android
#[cfg(not(target_os = "android"))]
fn _not_android() {}
