fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "haiku" {
        cc::Build::new()
            .cpp(true)
            .file("src/haiku_audio.cpp")
            .file("src/haiku_midi.cpp")
            .compile("haiku_shims");
        println!("cargo:rustc-link-lib=media");
        println!("cargo:rustc-link-lib=midi2");
        println!("cargo:rustc-link-lib=be");
        println!("cargo:rustc-link-lib=stdc++");
    }
}
