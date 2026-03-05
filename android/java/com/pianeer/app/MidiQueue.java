package com.pianeer.app;

import android.media.midi.MidiDevice;
import android.media.midi.MidiOutputPort;
import android.media.midi.MidiReceiver;
import android.util.Log;
import java.io.IOException;
import java.util.Arrays;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * MidiReceiver subclass that queues raw MIDI bytes so Rust can poll them.
 * Each call to onSend() enqueues a single byte[] containing the raw MIDI
 * message (not USB MIDI 4-byte packets).
 */
public class MidiQueue extends MidiReceiver {

    private final LinkedBlockingQueue<byte[]> queue = new LinkedBlockingQueue<>(1024);
    private final MidiDevice device;
    private final MidiOutputPort port;

    MidiQueue(MidiDevice device, MidiOutputPort port) {
        this.device = device;
        this.port   = port;
    }

    @Override
    public void onSend(byte[] msg, int offset, int count, long timestamp) throws IOException {
        MidiHelper.flog("onSend count=" + count);
        if (count > 0) {
            queue.offer(Arrays.copyOfRange(msg, offset, offset + count));
        }
    }

    /**
     * Return the next raw MIDI message, or null if none arrives within
     * timeoutMs milliseconds.  Called from Rust via JNI.
     */
    public byte[] poll(long timeoutMs) throws InterruptedException {
        return queue.poll(timeoutMs, TimeUnit.MILLISECONDS);
    }

    /** Close the port and device connection. */
    public void close() {
        try { port.close();   } catch (Exception ignored) {}
        try { device.close(); } catch (Exception ignored) {}
    }
}
