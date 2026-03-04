// Haiku-specific terminal raw mode, key reading, and size query.
// crossterm depends on mio which uses Linux-only socket APIs; on Haiku we
// use POSIX termios/poll/read directly via libc instead.

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;
use pianeer_core::types::MenuAction;

// ── Raw mode ─────────────────────────────────────────────────────────────────

static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);
// Safety: written once from the main thread before input thread starts.
static mut ORIG_TERMIOS: MaybeUninit<libc::termios> = MaybeUninit::uninit();

pub fn enable_raw_mode() {
    unsafe {
        libc::tcgetattr(libc::STDIN_FILENO, ORIG_TERMIOS.as_mut_ptr());
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        let mut raw = *ORIG_TERMIOS.as_ptr();
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN]  = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
    }
}

pub fn disable_raw_mode() {
    if RAW_ACTIVE.load(Ordering::SeqCst) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, ORIG_TERMIOS.as_ptr());
        }
    }
}

// ── Terminal size ─────────────────────────────────────────────────────────────

pub fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = MaybeUninit::zeroed().assume_init();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

// ── Input thread ─────────────────────────────────────────────────────────────

fn poll_stdin(ms: i32) -> bool {
    unsafe {
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut fds as *mut _, 1, ms) > 0
    }
}

/// Read one keypress (or escape sequence) from stdin.
/// Returns the raw bytes, or None if nothing arrived within 50 ms.
fn read_key() -> Option<Vec<u8>> {
    if !poll_stdin(50) {
        return None;
    }
    let mut buf = [0u8; 8];
    let n = unsafe {
        libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1)
    };
    if n <= 0 {
        return None;
    }
    // ESC starts a multi-byte sequence (arrow keys, F-keys…); read the rest
    // with a short timeout so the whole sequence arrives in one call.
    if buf[0] == 0x1b && poll_stdin(20) {
        let m = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf[1..].as_mut_ptr() as *mut libc::c_void,
                7,
            )
        };
        if m > 0 {
            return Some(buf[..1 + m as usize].to_vec());
        }
    }
    Some(vec![buf[0]])
}

pub fn spawn_input_thread(
    action_tx: Sender<MenuAction>,
    quit:      Arc<AtomicBool>,
    reload:    Arc<AtomicBool>,
    show_qr:   Arc<AtomicBool>,
) {
    std::thread::spawn(move || loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        let key = match read_key() {
            Some(k) => k,
            None    => continue,
        };

        // Quit: q/Q/Ctrl-C
        if key.as_slice() == b"q"
            || key.as_slice() == b"Q"
            || key.as_slice() == b"\x03"
        {
            quit.store(true, Ordering::SeqCst);
            break;
        }

        // Any key dismisses the QR overlay
        if show_qr.load(Ordering::Relaxed) {
            let _ = action_tx.try_send(MenuAction::ShowQr);
            continue;
        }

        let action: Option<MenuAction> = match key.as_slice() {
            b"\x1b[A" => Some(MenuAction::CursorUp),
            b"\x1b[B" => Some(MenuAction::CursorDown),
            b"\x1b[D" => Some(MenuAction::CycleVariant(-1)),
            b"\x1b[C" => Some(MenuAction::CycleVariant(1)),
            b"\r" | b"\n" => Some(MenuAction::Select),
            b"r" | b"R" => {
                reload.store(true, Ordering::SeqCst);
                None
            }
            b"v" | b"V" => Some(MenuAction::CycleVeltrack),
            b"t" | b"T" => Some(MenuAction::CycleTune),
            b"e" | b"E" => Some(MenuAction::ToggleRelease),
            b"+" | b"=" => Some(MenuAction::VolumeChange(1)),
            b"-"        => Some(MenuAction::VolumeChange(-1)),
            b"["        => Some(MenuAction::TransposeChange(-1)),
            b"]"        => Some(MenuAction::TransposeChange(1)),
            b"h" | b"H" => Some(MenuAction::ToggleResonance),
            b"w" | b"W" => Some(MenuAction::ToggleRecord),
            b" "        => Some(MenuAction::PauseResume),
            b","        => Some(MenuAction::SeekRelative(-10)),
            b"."        => Some(MenuAction::SeekRelative(10)),
            b"p" | b"P" => Some(MenuAction::ShowQr),
            _           => None,
        };
        if let Some(a) = action {
            let _ = action_tx.try_send(a);
        }
    });
}
