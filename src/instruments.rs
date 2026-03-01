use std::path::{Path, PathBuf};

pub struct Instrument {
    pub name: String,
    pub variants: Vec<(String, PathBuf)>,   // (display_name, path)
    pub variant_idx: usize,
}

impl Instrument {
    pub fn path(&self) -> &PathBuf { &self.variants[self.variant_idx].1 }

    /// Returns variant display name only when >1 variant exists.
    pub fn variant_name(&self) -> Option<&str> {
        if self.variants.len() > 1 { Some(&self.variants[self.variant_idx].0) }
        else { None }
    }

    pub fn cycle_variant(&mut self, dir: i8) {
        if self.variants.is_empty() { return; }
        let n = self.variants.len() as i32;
        self.variant_idx = ((self.variant_idx as i32 + dir as i32).rem_euclid(n)) as usize;
    }
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

fn is_instrument_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("sfz") | Some("organ") | Some("nki") | Some("nkm") | Some("gig")
    )
}

/// True if `path` is an `.nki` file.
fn is_nki(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("nki")
}

/// Scan one (or two) levels of subdirectories in `samples_dir` for instrument files.
/// Files in the same directory are grouped as variants of a single Instrument entry.
/// Returns up to 16 instruments sorted alphabetically.
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

        let mut entries: Vec<_> = match std::fs::read_dir(&path) {
            Ok(e) => e.flatten().collect(),
            Err(_) => continue,
        };
        entries.sort_by_key(|e| e.file_name());

        // If any .nki files are present, suppress .nkm files (they are redundant banks
        // of the same programs).
        let has_nki = entries.iter().any(|e| is_nki(&e.path()));

        let mut direct_files: Vec<(String, PathBuf)> = Vec::new();
        for subentry in &entries {
            let file_path = subentry.path();
            if !is_instrument_file(&file_path) {
                continue;
            }
            // Skip .nkm when individual .nki files are available.
            if has_nki && file_path.extension().and_then(|e| e.to_str()) == Some("nkm") {
                continue;
            }
            let stem = file_path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            direct_files.push((stem, file_path));
        }

        if !direct_files.is_empty() {
            if direct_files.len() == 1 {
                let (stem, file_path) = direct_files.remove(0);
                let name = format!("{} / {}", subdir_name, stem);
                instruments.push(Instrument {
                    name,
                    variants: vec![(stem, file_path)],
                    variant_idx: 0,
                });
            } else {
                instruments.push(Instrument {
                    name: subdir_name.clone(),
                    variants: direct_files,
                    variant_idx: 0,
                });
            }
        } else {
            // No instrument files directly in this subdir — look one level deeper.
            let mut nested_subdirs: Vec<_> = entries
                .into_iter()
                .filter(|e| e.path().is_dir())
                .collect();
            nested_subdirs.sort_by_key(|e| e.file_name());

            for nested in nested_subdirs {
                let nested_path = nested.path();
                let nested_name = match nested_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                let mut sfz_files: Vec<_> = match std::fs::read_dir(&nested_path) {
                    Ok(e) => e.flatten()
                        .filter(|e| is_instrument_file(&e.path()))
                        .collect(),
                    Err(_) => continue,
                };
                if sfz_files.is_empty() {
                    continue;
                }
                sfz_files.sort_by_key(|e| e.file_name());

                // Prefer a file with "recommended" in the name (case-insensitive).
                let best = sfz_files.iter()
                    .find(|e| e.file_name().to_string_lossy().to_lowercase().contains("recommended"))
                    .unwrap_or(&sfz_files[0]);

                let file_path = best.path();
                let stem = match file_path.file_stem().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                let name = format!("{} / {} / {}", subdir_name, nested_name, stem);
                instruments.push(Instrument {
                    name,
                    variants: vec![(stem, file_path)],
                    variant_idx: 0,
                });
            }
        }
    }

    instruments.truncate(16);
    instruments
}
