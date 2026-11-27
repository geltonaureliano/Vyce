//! The clean-up passes' switch, and the rule that keeps them out of the
//! translator's fingerprint (`fingerprint.rs`): off by default, and nothing
//! of them in what the shader cache keys on while they are off.

#[path = "../../fingerprint.rs"]
mod fingerprint;

use std::path::{Path, PathBuf};

#[test]
fn the_clean_up_is_off_unless_asked_for() {
    use super::bitcode::opt::{ALL, DEAD, FOLD};
    assert_eq!(super::parse_shader_opt(None), 0);
    assert_eq!(super::parse_shader_opt(Some("")), 0);
    assert_eq!(super::parse_shader_opt(Some(" 0 ")), 0);
    assert_eq!(super::parse_shader_opt(Some("lots")), 0);
    assert_eq!(super::parse_shader_opt(Some("all")), ALL);
    assert_eq!(super::parse_shader_opt(Some("ALL")), ALL);
    assert_eq!(super::parse_shader_opt(Some("3")), DEAD | FOLD);
    assert_eq!(super::parse_shader_opt(Some("0x3")), DEAD | FOLD);
    assert_eq!(super::parse_shader_opt(Some("4294967295")), ALL, "bits beyond the passes go");
}

/// Regions split as the build script splits them: their lines out, the rest
/// as it was.
#[test]
fn regions_leave_the_text_around_them_as_it_was() {
    let text = b"a\n// clean-up passes: begin (why)\nb\n\n    // clean-up passes: end\nc\n";
    let (kept, apart) = fingerprint::split(text).unwrap();
    assert_eq!(kept, b"a\nc\n");
    assert_eq!(apart, b"// clean-up passes: begin (why)\nb\n\n    // clean-up passes: end\n");
    assert_eq!(fingerprint::split(b"no regions\n").unwrap(), (b"no regions\n".to_vec(), Vec::new()));
    assert!(fingerprint::split(b"// clean-up passes: begin\nb\n").is_none(), "no end");
    assert!(fingerprint::split(b"// clean-up passes: end\n").is_none(), "no beginning");
    let nested = b"// clean-up passes: begin\n// clean-up passes: begin\n// clean-up passes: end\n";
    assert!(fingerprint::split(nested).is_none());
    assert!(fingerprint::cleanup_file(Path::new("src/emit/bitcode/opt.rs")));
    assert!(fingerprint::cleanup_file(Path::new("src\\emit\\bitcode\\opt.rs")));
    assert!(!fingerprint::cleanup_file(Path::new("src/emit/bitcode.rs")));
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Whatever of the translator names the clean-up passes is in a region (or
/// in their own file): the fingerprint the shader cache keys on is then the
/// one of the sources without them, and a build with them off keeps the
/// translations made before they came.
#[test]
fn the_translator_names_the_clean_up_only_in_its_regions() {
    const NAMES: [&str; 6] = ["shader_opt", "bitcode::opt", "opt::", "optimise(", "VICTORY_SHADER_OPT", "CLEANUP_SOURCES"];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let mut regions = 0;
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        // As the build script: tests and host checks make no translation.
        if name.ends_with("_tests.rs") || fingerprint::cleanup_file(&file) {
            continue;
        }
        let text = std::fs::read(&file).unwrap();
        let (kept, apart) = fingerprint::split(&text).unwrap_or_else(|| panic!("{}: an unbalanced region", file.display()));
        regions += usize::from(!apart.is_empty());
        if matches!(name.as_str(), "host.rs" | "validator.rs" | "opcode_audit.rs") {
            continue;
        }
        let kept = String::from_utf8_lossy(&kept);
        for n in NAMES {
            assert!(!kept.contains(n), "{}: `{n}` outside a clean-up region", file.display());
        }
    }
    assert_eq!(regions, 3, "the calls in emit/mod.rs, emit/shader.rs and emit/bitcode.rs");
}
