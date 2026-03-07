/// Platform-provided remote PCM audio player (non-WASM only).
/// Android uses Oboe; desktop could use cpal. Set via `PianeerApp::set_remote_audio`.
pub trait RemoteAudioPlayer: Send {
    fn connect(&mut self, pcm_ws_url: &str);
    fn disconnect(&mut self);
    fn is_active(&self) -> bool;
}
