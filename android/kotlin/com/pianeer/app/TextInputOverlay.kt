package com.pianeer.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.text.InputType
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText

/**
 * Shows a system AlertDialog with an EditText so the soft keyboard appears
 * reliably. NativeActivity's SurfaceView has no onCreateInputConnection, so
 * any showSoftInput call on it silently fails; a real View is required.
 *
 * Rust polls takePendingResult() each frame to check for a confirmed value.
 */
object TextInputOverlay {
    /** Set when user presses Connect; null means no pending result. */
    @Volatile var pendingResult: String? = null

    @JvmStatic
    fun showDialog(activity: Activity, current: String) {
        activity.runOnUiThread {
            val input = EditText(activity)
            input.inputType = InputType.TYPE_CLASS_TEXT or
                    InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                    InputType.TYPE_TEXT_VARIATION_URI
            input.imeOptions = EditorInfo.IME_ACTION_DONE
            input.setText(current)
            input.setSelection(current.length)

            val dialog = AlertDialog.Builder(activity)
                .setTitle("Connect to server  (host:4000)")
                .setView(input)
                .setPositiveButton("Connect") { _, _ ->
                    pendingResult = input.text.toString().trim()
                }
                .setNegativeButton("Cancel", null)
                .create()

            dialog.setOnShowListener {
                input.requestFocus()
                val imm = activity.getSystemService(Context.INPUT_METHOD_SERVICE)
                    as InputMethodManager
                imm.showSoftInput(input, InputMethodManager.SHOW_IMPLICIT)
            }
            dialog.show()
        }
    }

    /** Returns and clears the pending result, or null if none. */
    @JvmStatic
    fun takePendingResult(): String? {
        val r = pendingResult
        if (r != null) pendingResult = null
        return r
    }
}
