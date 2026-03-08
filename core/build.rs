fn main() {
    // Ensure web-wasm/dist/ exists so include_dir! doesn't panic before trunk has been run.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let dist = std::path::Path::new(&manifest).join("../web-wasm/dist");
    if !dist.exists() {
        std::fs::create_dir_all(&dist).ok();
    }
    println!("cargo:rerun-if-changed=../web-wasm/dist");
}
