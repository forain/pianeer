// FFI bindings for the Android NDK AMidi API (API level 29+).
// https://developer.android.com/ndk/reference/group/midi

pub enum AMidiDevice {}
pub enum AMidiOutputPort {}

pub const AMIDI_OPCODE_DATA: i32 = 1;
pub const AMEDIA_OK: i32 = 0;

#[link(name = "amidi")]
extern "C" {
    /// Obtain a native AMidiDevice handle from a Java MidiDevice object.
    /// The Java object must remain alive (held in a GlobalRef) while the
    /// AMidiDevice is in use.
    pub fn AMidiDevice_fromJava(
        env: *mut std::ffi::c_void,
        midiDeviceObj: *mut std::ffi::c_void,
        outDevice: *mut *mut AMidiDevice,
    ) -> i32;

    pub fn AMidiDevice_release(midiDevice: *mut AMidiDevice) -> i32;

    pub fn AMidiOutputPort_open(
        midiDevice: *mut AMidiDevice,
        portNumber: i32,
        outOutputPort: *mut *mut AMidiOutputPort,
    ) -> i32;

    /// Non-blocking receive.  Returns number of messages received (0 = none),
    /// or negative on error.
    pub fn AMidiOutputPort_receive(
        midiOutputPort: *mut AMidiOutputPort,
        opcodePtr: *mut i32,
        buffer: *mut u8,
        maxBytes: usize,
        numBytesReceivedPtr: *mut usize,
        outTimestamp: *mut i64,
    ) -> isize;

    pub fn AMidiOutputPort_close(midiOutputPort: *mut AMidiOutputPort);
}
