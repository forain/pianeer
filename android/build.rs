fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "android" {
        return;
    }

    println!("cargo:rustc-link-lib=amidi");
    println!("cargo:rerun-if-changed=java/com/pianeer/app/MidiOpener.java");
    println!("cargo:rerun-if-changed=java/com/pianeer/app/WindowHelper.java");

    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .expect("ANDROID_SDK_ROOT must be set when building for Android");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    let java_dir = format!("{}/java/com/pianeer/app", manifest_dir);
    let android_jar = format!("{}/platforms/android-35/android.jar", sdk);
    let d8 = format!("{}/build-tools/35.0.1/d8", sdk);
    let classes_dir = format!("{}/midi_opener_classes", out_dir);

    std::fs::create_dir_all(&classes_dir).unwrap();

    // Compile all .java files in the package directory together.
    let java_files: Vec<String> = std::fs::read_dir(&java_dir)
        .expect("java source dir not found")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("java"))
        .map(|p| p.to_str().unwrap().to_string())
        .collect();

    let mut javac_args = vec![
        "--release".to_string(), "8".to_string(),
        "-cp".to_string(), android_jar.clone(),
        "-d".to_string(), classes_dir.clone(),
    ];
    javac_args.extend(java_files);

    let status = std::process::Command::new("javac")
        .args(&javac_args)
        .status()
        .expect("javac not found — install JDK");
    assert!(status.success(), "javac failed");

    // Collect all .class files from the package (includes inner classes like MidiOpener$DeviceWatcher).
    let pkg_dir = format!("{}/com/pianeer/app", classes_dir);
    let mut class_files: Vec<String> = std::fs::read_dir(&pkg_dir)
        .expect("class dir not found")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("class"))
        .map(|p| p.to_str().unwrap().to_string())
        .collect();
    class_files.sort(); // deterministic ordering

    let mut d8_args = vec![
        "--lib".to_string(), android_jar.clone(),
        "--output".to_string(), out_dir.clone(),
    ];
    d8_args.extend(class_files);

    let status = std::process::Command::new(&d8)
        .args(&d8_args)
        .status()
        .expect("d8 not found");
    assert!(status.success(), "d8 failed");

    std::fs::rename(
        format!("{}/classes.dex", out_dir),
        format!("{}/midi_opener.dex", out_dir),
    )
    .unwrap();
}
