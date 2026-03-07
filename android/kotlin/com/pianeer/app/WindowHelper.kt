package com.pianeer.app

import android.app.Activity
import android.content.Context
import android.os.Build
import android.view.WindowInsets
import android.view.inputmethod.InputMethodManager

/**
 * Called from Rust/JNI to show/hide the soft keyboard.
 * Must run on the UI thread; done here via runOnUiThread.
 */
object WindowHelper {
    @JvmStatic
    fun showKeyboard(activity: Activity) {
        activity.runOnUiThread {
            if (Build.VERSION.SDK_INT >= 30) {
                val ctrl = activity.window.insetsController
                if (ctrl != null) {
                    ctrl.show(WindowInsets.Type.ime())
                    return@runOnUiThread
                }
            }
            // API 29 fallback
            val imm = activity.getSystemService(Context.INPUT_METHOD_SERVICE)
                as InputMethodManager
            val decor = activity.window.decorView
            decor.requestFocus()
            imm.showSoftInput(decor, InputMethodManager.SHOW_FORCED)
        }
    }

    @JvmStatic
    fun hideKeyboard(activity: Activity) {
        activity.runOnUiThread {
            if (Build.VERSION.SDK_INT >= 30) {
                val ctrl = activity.window.insetsController
                if (ctrl != null) {
                    ctrl.hide(WindowInsets.Type.ime())
                    return@runOnUiThread
                }
            }
            // API 29 fallback
            val imm = activity.getSystemService(Context.INPUT_METHOD_SERVICE)
                as InputMethodManager
            val decor = activity.window.decorView
            imm.hideSoftInputFromWindow(decor.windowToken, 0)
        }
    }
}
