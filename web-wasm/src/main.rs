// pianeer-wasm: eframe WebRunner entry point.
//
// Build with:   cd web-wasm && trunk build --release
// Dev server:   cd web-wasm && trunk serve
//
// The WASM app connects to the running pianeer desktop server's WebSocket at
// ws://localhost:6000/ws  (or the host/port embedded in the page URL).
// WebTransport upgrade is handled transparently by ewebsock when available.

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        use eframe::wasm_bindgen::JsValue;

        let ws_url = ws_url_from_page();

        let backend = match pianeer_egui::RemoteBackend::connect(&ws_url) {
            Ok(b) => b,
            Err(e) => {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "Pianeer: could not connect to {ws_url}: {e}"
                )));
                return;
            }
        };

        let server_url = http_url_from_page();
        let app = pianeer_egui::PianeerApp::new(Box::new(backend), server_url);

        let web_options = eframe::WebOptions::default();
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("pianeer_canvas"))
            .and_then(|e| {
                use eframe::wasm_bindgen::JsCast;
                e.dyn_into::<web_sys::HtmlCanvasElement>().ok()
            })
            .expect("could not find #pianeer_canvas");

        wasm_bindgen_futures::spawn_local(async {
            eframe::WebRunner::new()
                .start(canvas, web_options, Box::new(|_cc| Ok(Box::new(app))))
                .await
                .expect("failed to start eframe");
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("pianeer-wasm is a WASM-only crate. Build with `trunk build`.");
}

/// Derive the WebSocket URL from the current page origin.
#[cfg(target_arch = "wasm32")]
fn ws_url_from_page() -> String {
    let window = web_sys::window().expect("no window");
    let host = window.location().host().unwrap_or_else(|_| "localhost:4000".to_string());
    format!("ws://{}/ws", host)
}

/// Derive the HTTP origin URL (e.g. "http://192.168.1.5:4000") from the page.
#[cfg(target_arch = "wasm32")]
fn http_url_from_page() -> String {
    let window = web_sys::window().expect("no window");
    window.location().origin().unwrap_or_else(|_| "http://localhost:4000".to_string())
}
