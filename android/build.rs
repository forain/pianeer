fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "android" {
        return;
    }

    println!("cargo:rustc-link-lib=amidi");
    println!("cargo:rerun-if-changed=java/com/pianeer/app/MidiOpener.java");

    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .expect("ANDROID_SDK_ROOT must be set when building for Android");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let java_src = format!("{}/java/com/pianeer/app/MidiOpener.java", manifest_dir);
    let android_jar = format!("{}/platforms/android-35/android.jar", sdk);
    let d8 = format!("{}/build-tools/35.0.1/d8", sdk);
    let classes_dir = format!("{}/midi_opener_classes", out_dir);

    std::fs::create_dir_all(&classes_dir).unwrap();

    let status = std::process::Command::new("javac")
        .args(["--release", "8", "-cp", &android_jar, "-d", &classes_dir, &java_src])
        .status()
        .expect("javac not found — install JDK");
    assert!(status.success(), "javac failed");

    let status = std::process::Command::new(&d8)
        .args([
            "--lib", &android_jar,
            "--output", &out_dir,
            &format!("{}/com/pianeer/app/MidiOpener.class", classes_dir),
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
