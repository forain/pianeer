use std::path::{Path, PathBuf};

pub struct Instrument {
    pub name: String,
    pub path: PathBuf,
}

/// Find the `samples/` directory by checking a few common locations.
pub fn find_samples_dir() -> Option<PathBuf> {
    // 1. CWD/samples/
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("samples");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    // 2. <binary_dir>/samples/
    if let Ok(exe) = std::env::current_exe() {
        if let Some(binary_dir) = exe.parent() {
            let candidate = binary_dir.join("samples");
            if candidate.is_dir() {
                return Some(candidate);
            }

            // 3. <binary_dir>/../../samples/ (target/release builds)
            let candidate = binary_dir.join("../../samples");
            if candidate.is_dir() {
                return Some(candidate.canonicalize().unwrap_or(candidate));
            }
        }
    }

    None
}

/// Scan one level of subdirectories in `samples_dir` for `.sfz` and `.organ` files.
/// Returns up to 9 instruments sorted alphabetically.
pub fn discover(samples_dir: &Path) -> Vec<Instrument> {
    let mut instruments = Vec::new();

    let entries = match std::fs::read_dir(samples_dir) {
        Ok(e) => e,
        Err(_) => return instruments,
    };

    let mut subdirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    subdirs.sort_by_key(|e| e.file_name());

    for entry in subdirs {
        let path = entry.path();
        let subdir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let mut files: Vec<_> = match std::fs::read_dir(&path) {
            Ok(e) => e.flatten().collect(),
            Err(_) => continue,
        };
        files.sort_by_key(|e| e.file_name());

        for subentry in files {
            let file_path = subentry.path();
            if let Some(ext) = file_path.extension().and_then(|e| e.to_str()) {
                if ext == "sfz" || ext == "organ" {
                    let filename = match file_path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    instruments.push(Instrument {
                        name: format!("{} / {}", subdir_name, filename),
                        path: file_path,
                    });
                }
            }
        }
    }

    instruments.truncate(9);
    instruments
}
