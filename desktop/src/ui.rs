use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::terminal;

pub use pianeer_core::types::{
    MenuItem, MenuAction, PlaybackInfo, ProcStats,
    Settings, VeltrackLevel, TunePreset,
};

// ── Playback state (desktop — owns the JoinHandle) ────────────────────────────

pub struct PlaybackState {
    pub stop:       Arc<AtomicBool>,
    pub done:       Arc<AtomicBool>,
    pub paused:     Arc<AtomicBool>,
    pub current_us: Arc<AtomicU64>,
    pub total_us:   u64,
    pub seek_to:    Arc<AtomicU64>,
    pub _handle:    thread::JoinHandle<()>,
    pub menu_idx:   usize,
}

impl PlaybackState {
    pub fn info(&self) -> PlaybackInfo {
        PlaybackInfo {
            current_us: self.current_us.load(Ordering::Relaxed),
            total_us:   self.total_us,
            paused:     self.paused.load(Ordering::Relaxed),
            menu_idx:   self.menu_idx,
        }
    }
}

// ── Playback helpers ──────────────────────────────────────────────────────────

pub fn stop_playback(pb: &mut Option<PlaybackState>) {
    if let Some(p) = pb.take() {
        p.stop.store(true, Ordering::Relaxed);
    }
}

pub fn playing_idx(pb: &Option<PlaybackState>) -> Option<usize> {
    pb.as_ref()
        .filter(|p| !p.done.load(Ordering::Relaxed))
        .map(|p| p.menu_idx)
}

// ── VU bar ────────────────────────────────────────────────────────────────────

fn vu_bar(peak: f32) -> String {
    const WIDTH: usize = 20;
    const DB_FLOOR: f32 = -48.0;
    const GREEN_END: usize  = 15;
    const YELLOW_END: usize = 19;

    let db = if peak > 1e-10 { 20.0 * peak.log10() } else { -96.0_f32 };
    let frac = ((db - DB_FLOOR) / (-DB_FLOOR)).clamp(0.0, 1.0);
    let filled = (frac * WIDTH as f32).round() as usize;
    let empty  = WIDTH - filled;

    let green_n  = filled.min(GREEN_END);
    let yellow_n = filled.saturating_sub(GREEN_END).min(YELLOW_END - GREEN_END);
    let red_n    = filled.saturating_sub(YELLOW_END);

    let green:  String = std::iter::repeat('█').take(green_n).collect();
    let yellow: String = std::iter::repeat('█').take(yellow_n).collect();
    let red:    String = std::iter::repeat('█').take(red_n).collect();
    let empty:  String = std::iter::repeat('░').take(empty).collect();

    let db_display = db.clamp(DB_FLOOR, 6.0);
    format!(
        "\x1b[32m{}\x1b[33m{}\x1b[31m{}\x1b[0m{} {:>+6.1}dB",
        green, yellow, red, empty, db_display,
    )
}

// ── Seekbar ───────────────────────────────────────────────────────────────────

pub fn fmt_time(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn seekbar_line(info: &PlaybackInfo) -> String {
    const W: usize = 32;
    let frac = if info.total_us > 0 {
        (info.current_us as f64 / info.total_us as f64).clamp(0.0, 1.0)
    } else { 0.0 };
    let filled = (frac * W as f64).round() as usize;
    let bar: String = (0..W).map(|i| if i < filled { '═' } else { '░' }).collect();
    let icon   = if info.paused { "⏸" } else { "▶" };
    let action = if info.paused { "resume" } else { "pause " };
    format!(
        "  {} {} / {}  \x1b[36m[{}]\x1b[0m  Space:{} ,/.:±10s",
        icon,
        fmt_time(info.current_us),
        fmt_time(info.total_us),
        bar,
        action,
    )
}

// ── QR modal ──────────────────────────────────────────────────────────────────

fn qr_web_url() -> String {
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("1.1.1.1:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return format!("http://{}:4000", addr.ip());
            }
        }
    }
    "http://localhost:4000".to_string()
}

pub fn print_qr_modal() {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let url = qr_web_url();
    let qr_lines: Vec<String> = match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let img = code.render::<qrcode::render::unicode::Dense1x2>().build();
            img.lines().map(|l| l.to_string()).collect()
        }
        Err(_) => vec!["[QR unavailable]".to_string()],
    };

    let total_content = 1 + 1 + qr_lines.len() + 1 + 1 + 1;
    let top_pad = ((rows as usize).saturating_sub(total_content)) / 2;

    print!("\x1b[2J\x1b[H");
    for _ in 0..top_pad { print!("\r\n"); }

    let title = "Scan to open web UI";
    let title_pad = " ".repeat((cols as usize).saturating_sub(title.len()) / 2);
    print!("{}\x1b[36m{}\x1b[0m\r\n\r\n", title_pad, title);

    let qr_width = qr_lines.first().map(|l| l.len()).unwrap_or(0);
    let qr_pad = " ".repeat((cols as usize).saturating_sub(qr_width) / 2);
    for line in &qr_lines {
        print!("{}\x1b[30;47m{}\x1b[0m\r\n", qr_pad, line);
    }

    let url_pad = " ".repeat((cols as usize).saturating_sub(url.len()) / 2);
    print!("{}\x1b[36m{}\x1b[0m\r\n\r\n", url_pad, url);

    let hint = "[ press any key to close ]";
    let hint_pad = " ".repeat((cols as usize).saturating_sub(hint.len()) / 2);
    print!("{}\x1b[90m{}\x1b[0m\r\n", hint_pad, hint);

    std::io::stdout().flush().ok();
}

// ── print_menu ────────────────────────────────────────────────────────────────

pub fn print_menu(
    menu:        &[MenuItem],
    cursor:      usize,
    loaded:      usize,
    playing:     Option<usize>,
    settings:    &Settings,
    sustain:     bool,
    stats:       &ProcStats,
    recording:   bool,
    rec_elapsed: Option<Duration>,
    pb_info:     Option<&PlaybackInfo>,
) {
    print!("\x1b[2J\x1b[H");
    print!("Pianeer  \u{2191}/\u{2193} nav  Enter load  \u{2190}/\u{2192} variant  R rescan  Q quit\r\n");
    print!("  V vel  T tune  E release  +/- vol  [/] trns  H res  W rec  P qr\r\n");

    let sus_label = if sustain { "\x1b[1mSus:ON\x1b[0m" } else { "Sus:off" };
    print!(
        "  Vel:{}  Tune:{}  Rel:{}  Vol:{}dB  Trns:{}  Res:{}  {}\r\n",
        settings.veltrack.label(),
        settings.tune.label(),
        if settings.release_enabled { "on" } else { "off" },
        if settings.volume_db == 0.0 {
            "0".to_string()
        } else {
            format!("{:+.0}", settings.volume_db)
        },
        if settings.transpose == 0 {
            "0".to_string()
        } else {
            format!("{:+}", settings.transpose)
        },
        if settings.resonance_enabled { "on" } else { "off" },
        sus_label,
    );
    let clip_str = if stats.clip { "  \x1b[1;31m[CLIP]\x1b[0m" } else { "" };
    print!(
        "  CPU:{:3}%  Mem:{:4}MB  Voices:{}\r\n  L {}  R {}{}\r\n",
        stats.cpu_pct, stats.mem_mb, stats.voices,
        vu_bar(stats.peak_l), vu_bar(stats.peak_r), clip_str,
    );

    if recording {
        let rec_time = rec_elapsed
            .map(|d| fmt_time(d.as_micros() as u64))
            .unwrap_or_else(|| "00:00".to_string());
        print!("  \x1b[1;31m● REC {}\x1b[0m  (W to stop)\r\n", rec_time);
    }

    if let Some(info) = pb_info {
        print!("{}\r\n", seekbar_line(info));
    }

    let has_inst = menu.iter().any(|m| matches!(m, MenuItem::Instrument(_)));
    let mut in_midi = false;

    if has_inst {
        print!("\r\nInstruments:\r\n");
    }

    for (i, item) in menu.iter().enumerate() {
        if !in_midi && matches!(item, MenuItem::MidiFile { .. }) {
            print!("\r\nMIDI files:\r\n");
            in_midi = true;
        }

        let is_loaded  = matches!(item, MenuItem::Instrument(_)) && i == loaded;
        let is_playing = playing == Some(i);
        let is_paused  = pb_info.map(|p| p.menu_idx == i && p.paused).unwrap_or(false);

        let tag = if is_paused       { "[paused] " }
                  else if is_playing { "[playing]" }
                  else if is_loaded  { "[loaded] " }
                  else               { "         " };

        let type_label = item.type_label();
        let title      = item.display_name();

        let variant_suffix = if let MenuItem::Instrument(inst) = item {
            if let Some(vname) = inst.variant_name() {
                format!(" [{} \u{25c4}\u{25ba}]", vname)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if i == cursor {
            print!("  {}  {}  \x1b[7m{}{}\x1b[27m\r\n", tag, type_label, title, variant_suffix);
        } else {
            print!("  {}  {}  {}{}\r\n", tag, type_label, title, variant_suffix);
        }
    }

    std::io::stdout().flush().ok();
}
