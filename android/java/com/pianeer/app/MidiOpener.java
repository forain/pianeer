package com.pianeer.app;

import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Minimal bridge for the async MidiManager.openDevice() callback.
 * Rust calls openSync() via JNI; AMidi takes over from there.
 */
public class MidiOpener implements MidiManager.OnDeviceOpenedListener {
    private final CountDownLatch latch = new CountDownLatch(1);
    public volatile MidiDevice device;

    @Override
    public void onDeviceOpened(MidiDevice device) {
        this.device = device;
        latch.countDown();
    }

    public static MidiDevice openSync(MidiManager manager, MidiDeviceInfo info)
            throws InterruptedException {
        MidiOpener opener = new MidiOpener();
        manager.openDevice(info, opener, null);
        opener.latch.await(5, TimeUnit.SECONDS);
        return opener.device;
    }
}
