package com.pianeer.app;

import android.app.Activity;
import android.content.Context;
import android.os.Build;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.InputMethodManager;

/**
 * Called from Rust/JNI to show/hide the soft keyboard.
 * Must run on the UI thread; done here via runOnUiThread.
 */
public class WindowHelper {
    public static void showKeyboard(final Activity activity) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (Build.VERSION.SDK_INT >= 30) {
                    // API 30+: WindowInsetsController works at the window level
                    // without requiring a view with onCreateInputConnection.
                    WindowInsetsController ctrl =
                        activity.getWindow().getInsetsController();
                    if (ctrl != null) {
                        ctrl.show(WindowInsets.Type.ime());
                        return;
                    }
                }
                // API 29 fallback
                InputMethodManager imm = (InputMethodManager)
                    activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                View decor = activity.getWindow().getDecorView();
                decor.requestFocus();
                imm.showSoftInput(decor, InputMethodManager.SHOW_FORCED);
            }
        });
    }

    public static void hideKeyboard(final Activity activity) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (Build.VERSION.SDK_INT >= 30) {
                    WindowInsetsController ctrl =
                        activity.getWindow().getInsetsController();
                    if (ctrl != null) {
                        ctrl.hide(WindowInsets.Type.ime());
                        return;
                    }
                }
                // API 29 fallback
                InputMethodManager imm = (InputMethodManager)
                    activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                View decor = activity.getWindow().getDecorView();
                imm.hideSoftInputFromWindow(decor.getWindowToken(), 0);
            }
        });
    }
}
