//! Fingerprints the translator's sources (`SOURCES` in `lib.rs`): a kept
//! translation is good for every build of the same sources, so a change
//! elsewhere in the program leaves the shader cache valid.

use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};

/// Sources that make no translation: tests, and the checks on a host device.
fn skipped(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.ends_with("_tests.rs")
        || matches!(name.as_str(), "host.rs" | "validator.rs" | "opcode_audit.rs")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("the crate's sources");
    for entry in entries {
        let path = entry.expect("the crate's sources").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") && !skipped(&path) {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    files.sort();
    let mut halves = [DefaultHasher::new(), DefaultHasher::new()];
    halves[1].write_u8(1);
    for file in files {
        let text = std::fs::read(&file).expect("a source file");
        // A checkout's line endings do not change the translation.
        let text: Vec<u8> = text.into_iter().filter(|&b| b != b'\r').collect();
        let name = file.to_string_lossy().replace('\\', "/");
        for half in &mut halves {
            half.write(name.as_bytes());
            half.write_usize(text.len());
            half.write(&text);
        }
    }
    println!(
        "cargo:rustc-env=VICTORY_SHADER_SOURCES={:016x}{:016x}",
        halves[0].finish(),
        halves[1].finish()
    );
}
