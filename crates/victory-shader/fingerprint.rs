//! What goes into the translator's fingerprint (`build.rs`), shared with the
//! test that checks the sources keep to it (`src/emit/cleanup_tests.rs`).
//!
//! The clean-up passes over translated programs (`src/emit/bitcode/opt.rs`,
//! from upstream Belobug's ae75a66) are off by default, and while they are
//! off a translation is the one made before they came. So they are
//! fingerprinted apart: their file, and the lines that call them, between a
//! line starting `// clean-up passes: begin` and one starting
//! `// clean-up passes: end` (both kept out too). The shader cache keys on
//! that second fingerprint only while the passes run, and a build with them
//! off keeps the translations a build without them made.

use std::path::Path;

/// The line that starts a clean-up region, trimmed.
pub const BEGIN: &str = "// clean-up passes: begin";
/// The line that ends one.
pub const END: &str = "// clean-up passes: end";

/// Whether a source file belongs to the clean-up passes as a whole.
pub fn cleanup_file(path: &Path) -> bool {
    path.ends_with("emit/bitcode/opt.rs")
}

/// `text` without its clean-up regions, and the regions' lines; `None` if a
/// region does not end, or one begins inside another.
pub fn split(text: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (mut kept, mut apart) = (Vec::new(), Vec::new());
    let mut inside = false;
    for line in text.split_inclusive(|&b| b == b'\n') {
        let trimmed = String::from_utf8_lossy(line);
        let trimmed = trimmed.trim_start();
        if trimmed.starts_with(BEGIN) {
            if inside {
                return None;
            }
            inside = true;
        }
        if inside {
            apart.extend_from_slice(line);
        } else {
            kept.extend_from_slice(line);
        }
        if trimmed.starts_with(END) {
            if !inside {
                return None;
            }
            inside = false;
        }
    }
    (!inside).then_some((kept, apart))
}
