package com.pianeer.app;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.DialogInterface;
import android.text.InputType;
import android.view.inputmethod.EditorInfo;
import android.widget.EditText;

/**
 * Shows a system AlertDialog with an EditText so the soft keyboard appears
 * reliably. NativeActivity's SurfaceView has no onCreateInputConnection, so
 * any showSoftInput call on it silently fails; a real View is required.
 *
 * Rust polls takePendingResult() each frame to check for a confirmed value.
 */
public class TextInputOverlay {
    /** Set when user presses Connect; null means no pending result. */
    public static volatile String pendingResult = null;

    public static void showDialog(final Activity activity, final String current) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                final EditText input = new EditText(activity);
                input.setInputType(InputType.TYPE_CLASS_TEXT
                        | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
                        | InputType.TYPE_TEXT_VARIATION_URI);
                input.setImeOptions(EditorInfo.IME_ACTION_DONE);
                input.setText(current);
                input.setSelection(current.length());

                final android.app.AlertDialog dialog = new AlertDialog.Builder(activity)
                        .setTitle("Connect to server  (host:4000)")
                        .setView(input)
                        .setPositiveButton("Connect", new DialogInterface.OnClickListener() {
                            @Override
                            public void onClick(DialogInterface d, int w) {
                                pendingResult = input.getText().toString().trim();
                            }
                        })
                        .setNegativeButton("Cancel", null)
                        .create();

                // Explicitly show the soft keyboard when the dialog appears.
                dialog.setOnShowListener(new DialogInterface.OnShowListener() {
                    @Override
                    public void onShow(DialogInterface d) {
                        input.requestFocus();
                        android.view.inputmethod.InputMethodManager imm =
                            (android.view.inputmethod.InputMethodManager)
                                activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                        imm.showSoftInput(input, android.view.inputmethod.InputMethodManager.SHOW_IMPLICIT);
                    }
                });
                dialog.show();
            }
        });
    }

    /** Returns and clears the pending result, or null if none. */
    public static String takePendingResult() {
        String r = pendingResult;
        if (r != null) pendingResult = null;
        return r;
    }
}
