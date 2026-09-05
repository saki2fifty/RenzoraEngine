//! Recognize retired Rust-ABI plugins so standalone builds do not compile them.

use std::path::Path;

/// Distinguish a legacy Rust dylib from the supported standalone C-ABI cdylib.
pub fn is_native(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    text.lines()
        .filter(|line| line.trim_start().starts_with("crate-type"))
        .any(|line| line.contains("\"dylib\""))
}
