package com.pianeer.app;

import android.content.Context;
import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import android.media.midi.MidiOutputPort;
import android.util.Log;
import java.io.FileWriter;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * Synchronous helper for the Android MIDI API.
 * MidiManager.openDevice() is asynchronous; this class bridges it to a
 * blocking call Rust can use from a plain thread.
 */
public class MidiHelper implements MidiManager.OnDeviceOpenedListener {

    private static final Object FAILED = new Object();
    private final LinkedBlockingQueue<Object> slot = new LinkedBlockingQueue<>(1);

    @Override
    public void onDeviceOpened(MidiDevice device) {
        slot.offer(device != null ? device : FAILED);
    }

    /** Blocks up to 5 seconds for the device to open. Returns null on failure. */
    private static MidiDevice openSync(MidiManager manager, MidiDeviceInfo info)
            throws InterruptedException {
        MidiHelper helper = new MidiHelper();
        manager.openDevice(info, helper, null);
        Object result = helper.slot.poll(5, TimeUnit.SECONDS);
        return (result instanceof MidiDevice) ? (MidiDevice) result : null;
    }

    /**
     * Find the first MIDI device that has at least one output port (i.e. the
     * device sends MIDI data — a keyboard), open it, and return a connected
     * MidiQueue that Rust can poll.  Returns null if no suitable device is found.
     */
    public static MidiQueue connect(Context context) throws Exception {
        MidiManager manager = (MidiManager) context.getSystemService(Context.MIDI_SERVICE);
        if (manager == null) return null;

        MidiDeviceInfo[] devices = manager.getDevices();
        flog("java connect: getDevices count=" + devices.length);
        for (MidiDeviceInfo info : devices) {
            String name = info.getProperties().getString(MidiDeviceInfo.PROPERTY_NAME);
            flog("  device type=" + info.getType()
                    + " outPorts=" + info.getOutputPortCount()
                    + " inPorts=" + info.getInputPortCount()
                    + " name=" + name);
            if (info.getOutputPortCount() < 1) continue;

            MidiDevice device = openSync(manager, info);
            if (device == null) { flog("  openSync returned null"); continue; }

            MidiOutputPort port = device.openOutputPort(0);
            if (port == null) {
                flog("  openOutputPort(0) returned null");
                try { device.close(); } catch (Exception ignored) {}
                continue;
            }

            MidiQueue queue = new MidiQueue(device, port);
            port.connect(queue);
            flog("  connected OK: " + name);
            return queue;
        }
        flog("java connect: no suitable device found");
        return null;
    }

    static void flog(String msg) {
        Log.d("PianeerMIDI", msg);
        try (FileWriter fw = new FileWriter("/sdcard/Pianeer/midi_debug.log", true)) {
            fw.write("[java] " + msg + "\n");
        } catch (Exception ignored) {}
    }

    /** Returns true if at least one MIDI device is currently attached. */
    public static boolean isAnyDevicePresent(Context context) {
        MidiManager manager = (MidiManager) context.getSystemService(Context.MIDI_SERVICE);
        if (manager == null) return false;
        MidiDeviceInfo[] devices = manager.getDevices();
        return devices != null && devices.length > 0;
    }
}
