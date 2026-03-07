package com.pianeer.app

import android.media.midi.MidiDevice
import android.media.midi.MidiDeviceInfo
import android.media.midi.MidiManager
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Bridge for the async MidiManager.openDevice() callback.
 * Also registers a DeviceCallback so Rust can detect device removal
 * before the native AMidi handles are invalidated.
 */
class MidiOpener : MidiManager.OnDeviceOpenedListener {
    private val latch = CountDownLatch(1)

    /** Populated by onDeviceOpened(); null if open timed out. */
    @Volatile @JvmField var device: MidiDevice? = null

    /** Set to true by DeviceWatcher when our device is removed. */
    @Volatile @JvmField var removed: Boolean = false

    private var mManager: MidiManager? = null
    private var mWatcher: DeviceWatcher? = null

    private class DeviceWatcher(
        private val opener: MidiOpener,
        private val targetId: Int,
    ) : MidiManager.DeviceCallback() {
        override fun onDeviceRemoved(info: MidiDeviceInfo) {
            if (info.id == targetId) {
                opener.removed = true
            }
        }
    }

    override fun onDeviceOpened(device: MidiDevice) {
        this.device = device
        latch.countDown()
    }

    companion object {
        /**
         * Opens the device synchronously, registers a removal watcher, and
         * returns this MidiOpener. Check .device for the opened MidiDevice.
         */
        @JvmStatic
        @Throws(InterruptedException::class)
        fun openSync(manager: MidiManager, info: MidiDeviceInfo): MidiOpener {
            val opener = MidiOpener()
            opener.mManager = manager
            manager.openDevice(info, opener, null)
            opener.latch.await(5, TimeUnit.SECONDS)
            if (opener.device != null) {
                val watcher = DeviceWatcher(opener, info.id)
                opener.mWatcher = watcher
                manager.registerDeviceCallback(watcher, null)
            }
            return opener
        }
    }

    /** Unregisters the removal watcher. Call when done with this session. */
    fun cleanup() {
        val mgr = mManager
        val w = mWatcher
        if (mgr != null && w != null) {
            mgr.unregisterDeviceCallback(w)
            mWatcher = null
        }
    }
}
