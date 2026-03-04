fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "haiku" {
        // Locate the Haiku API headers. The cross-compiler (or native g++) knows
        // its sysroot; Haiku OS headers live under:
        //   <sysroot>/system/develop/headers/os/          (base + SupportKit etc.)
        //   <sysroot>/system/develop/headers/os/media/    (SoundPlayer.h etc.)
        //   <sysroot>/system/develop/headers/os/midi2/    (MidiLocalConsumer.h etc.)
        //
        // On a native Haiku system -print-sysroot returns "" or "/", which is fine
        // because GCC's specs already add those paths; we still pass them explicitly
        // so cc-rs doesn't miss them.
        let cxx = std::env::var("CXX_x86_64_unknown_haiku")
            .or_else(|_| std::env::var("CXX_x86_64-unknown-haiku"))
            .or_else(|_| std::env::var("CXX"))
            .unwrap_or_else(|_| "g++".to_string());

        let sysroot = std::process::Command::new(&cxx)
            .arg("-print-sysroot")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "/")
            .unwrap_or_default();

        let headers = if sysroot.is_empty() {
            "/system/develop/headers".to_string()
        } else {
            format!("{}/system/develop/headers", sysroot)
        };

        cc::Build::new()
            .cpp(true)
            .file("src/haiku_audio.cpp")
            .file("src/haiku_midi.cpp")
            .include(&headers)
            .include(format!("{}/os", headers))
            .include(format!("{}/os/media", headers))
            .include(format!("{}/os/midi2", headers))
            .compile("haiku_shims");

        println!("cargo:rustc-link-lib=media");
        println!("cargo:rustc-link-lib=midi2");
        println!("cargo:rustc-link-lib=be");
        println!("cargo:rustc-link-lib=stdc++");
    }
}
