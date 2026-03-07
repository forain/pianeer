fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "android" {
        return;
    }

    println!("cargo:rustc-link-lib=amidi");
    println!("cargo:rerun-if-changed=kotlin/com/pianeer/app/MidiOpener.kt");
    println!("cargo:rerun-if-changed=kotlin/com/pianeer/app/WindowHelper.kt");
    println!("cargo:rerun-if-changed=kotlin/com/pianeer/app/TextInputOverlay.kt");

    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .expect("ANDROID_SDK_ROOT must be set when building for Android");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let kotlin_dir = format!("{}/kotlin/com/pianeer/app", manifest_dir);
    let android_jar = format!("{}/platforms/android-35/android.jar", sdk);
    let d8 = format!("{}/build-tools/35.0.1/d8", sdk);
    let compiled_jar = format!("{}/pianeer_kotlin.jar", out_dir);

    // Collect all .kt files in the package directory.
    let kt_files: Vec<String> = std::fs::read_dir(&kotlin_dir)
        .expect("kotlin source dir not found")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("kt"))
        .map(|p| p.to_str().unwrap().to_string())
        .collect();

    // Compile all .kt files to a single jar with kotlinc.
    let mut kotlinc_args = vec![
        "-cp".to_string(), android_jar.clone(),
        "-d".to_string(), compiled_jar.clone(),
        "-jvm-target".to_string(), "1.8".to_string(),
    ];
    kotlinc_args.extend(kt_files);

    let status = std::process::Command::new("kotlinc")
        .args(&kotlinc_args)
        .status()
        .expect("kotlinc not found — install Kotlin (e.g. `pacman -S kotlin`)");
    assert!(status.success(), "kotlinc failed");

    // Dex the compiled jar.
    let status = std::process::Command::new(&d8)
        .args(&[
            "--lib", &android_jar,
            "--output", &out_dir,
            &compiled_jar,
        ])
        .status()
        .expect("d8 not found");
    assert!(status.success(), "d8 failed");

    std::fs::rename(
        format!("{}/classes.dex", out_dir),
        format!("{}/midi_opener.dex", out_dir),
    )
    .unwrap();
}
