package com.pianeer.app;

import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Bridge for the async MidiManager.openDevice() callback.
 * Also registers a DeviceCallback so Rust can detect device removal
 * before the native AMidi handles are invalidated.
 */
public class MidiOpener implements MidiManager.OnDeviceOpenedListener {
    private final CountDownLatch latch = new CountDownLatch(1);
    public volatile MidiDevice device;
    /** Set to true by DeviceWatcher when our device is removed. */
    public volatile boolean removed = false;

    private MidiManager mManager;
    private DeviceWatcher mWatcher;

    private static class DeviceWatcher extends MidiManager.DeviceCallback {
        private final MidiOpener opener;
        private final int targetId;
        DeviceWatcher(MidiOpener opener, int targetId) {
            this.opener = opener;
            this.targetId = targetId;
        }
        @Override
        public void onDeviceRemoved(MidiDeviceInfo info) {
            if (info.getId() == targetId) {
                opener.removed = true;
            }
        }
    }

    @Override
    public void onDeviceOpened(MidiDevice device) {
        this.device = device;
        latch.countDown();
    }

    /**
     * Opens the device synchronously, registers a removal watcher, and
     * returns this MidiOpener. Check .device for the opened MidiDevice.
     */
    public static MidiOpener openSync(MidiManager manager, MidiDeviceInfo info)
            throws InterruptedException {
        MidiOpener opener = new MidiOpener();
        opener.mManager = manager;
        manager.openDevice(info, opener, null);
        opener.latch.await(5, TimeUnit.SECONDS);
        if (opener.device != null) {
            DeviceWatcher watcher = new DeviceWatcher(opener, info.getId());
            opener.mWatcher = watcher;
            manager.registerDeviceCallback(watcher, null);
        }
        return opener;
    }

    /** Unregisters the removal watcher. Call when done with this session. */
    public void cleanup() {
        if (mManager != null && mWatcher != null) {
            mManager.unregisterDeviceCallback(mWatcher);
            mWatcher = null;
        }
    }
}
