use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::instruments::Instrument;

pub enum MenuItem {
    Instrument(Instrument),
    MidiFile { name: String, path: PathBuf },
}

impl MenuItem {
    pub fn display_name(&self) -> &str {
        match self {
            MenuItem::Instrument(i) => &i.name,
            MenuItem::MidiFile { name, .. } => name,
        }
    }
}

pub struct PlaybackState {
    pub stop: Arc<AtomicBool>,
    pub done: Arc<AtomicBool>,
    pub _handle: thread::JoinHandle<()>,
    pub menu_idx: usize,
}

pub enum MenuAction {
    CursorUp,
    CursorDown,
    Select,
}

pub fn stop_playback(pb: &mut Option<PlaybackState>) {
    if let Some(p) = pb.take() {
        p.stop.store(true, Ordering::Relaxed);
        // Thread is detached; it sends AllNotesOff before exiting.
    }
}

pub fn playing_idx(pb: &Option<PlaybackState>) -> Option<usize> {
    pb.as_ref()
        .filter(|p| !p.done.load(Ordering::Relaxed))
        .map(|p| p.menu_idx)
}

pub fn print_menu(menu: &[MenuItem], cursor: usize, loaded: usize, playing: Option<usize>) {
    print!("\x1b[2J\x1b[H");
    print!("Pianeer — \u{2191}/\u{2193}: navigate  Enter: load/play  R: rescan  Q/Ctrl+C: quit\r\n");

    let has_midi = menu.iter().any(|m| matches!(m, MenuItem::MidiFile { .. }));
    let mut in_midi = false;

    for (i, item) in menu.iter().enumerate() {
        if has_midi && !in_midi && matches!(item, MenuItem::Instrument(_)) && i == 0 {
            print!("\r\nInstruments:\r\n");
        }
        if !in_midi && matches!(item, MenuItem::MidiFile { .. }) {
            print!("\r\nMIDI files:\r\n");
            in_midi = true;
        }

        let is_loaded = matches!(item, MenuItem::Instrument(_)) && i == loaded;
        let is_playing = playing == Some(i);
        // Fixed-width tag column (9 chars) keeps titles aligned.
        let tag = if is_playing { "[playing]" } else if is_loaded { "[loaded] " } else { "         " };
        let title = item.display_name();
        if i == cursor {
            print!("  {}  \x1b[7m{}\x1b[27m\r\n", tag, title);
        } else {
            print!("  {}  {}\r\n", tag, title);
        }
    }
    std::io::stdout().flush().ok();
}
