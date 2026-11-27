//! Fingerprints the translator's sources (`SOURCES` in `lib.rs`): a kept
//! translation is good for every build of the same sources, so a change
//! elsewhere in the program leaves the shader cache valid. The clean-up
//! passes are fingerprinted apart (`fingerprint.rs`,
//! `emit::CLEANUP_SOURCES`): off, they translate nothing.

use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};

mod fingerprint;

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

/// A fingerprint in two 64-bit halves, of file names and texts.
struct Print([DefaultHasher; 2]);

impl Print {
    fn new() -> Self {
        let mut halves = [DefaultHasher::new(), DefaultHasher::new()];
        halves[1].write_u8(1);
        Print(halves)
    }

    fn add(&mut self, name: &str, text: &[u8]) {
        for half in &mut self.0 {
            half.write(name.as_bytes());
            half.write_usize(text.len());
            half.write(text);
        }
    }

    fn hex(&self) -> String {
        format!("{:016x}{:016x}", self.0[0].finish(), self.0[1].finish())
    }
}

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=fingerprint.rs");
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    files.sort();
    let (mut translator, mut cleanup) = (Print::new(), Print::new());
    for file in files {
        let text = std::fs::read(&file).expect("a source file");
        // A checkout's line endings do not change the translation.
        let text: Vec<u8> = text.into_iter().filter(|&b| b != b'\r').collect();
        let name = file.to_string_lossy().replace('\\', "/");
        if fingerprint::cleanup_file(&file) {
            cleanup.add(&name, &text);
            continue;
        }
        let Some((kept, apart)) = fingerprint::split(&text) else {
            panic!("{name}: a clean-up region that does not end, or one inside another");
        };
        translator.add(&name, &kept);
        if !apart.is_empty() {
            cleanup.add(&name, &apart);
        }
    }
    println!("cargo:rustc-env=VICTORY_SHADER_SOURCES={}", translator.hex());
    println!("cargo:rustc-env=VICTORY_SHADER_CLEANUP_SOURCES={}", cleanup.hex());
}
