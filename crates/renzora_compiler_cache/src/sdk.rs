//! Installed source-SDK layout shared by packaging and compiler fingerprints.

use std::path::{Path, PathBuf};

pub use renzora_rust_sdk::{CONTENT_ROOT_LAYOUT, CONTENT_ROOT_MARKER};

/// Include sibling identity and derive sources when hashing a packaged SDK.
/// Ordinary source-checkout callers retain their explicitly supplied root.
pub fn content_root(entry: &Path) -> PathBuf {
    let packaged = std::fs::read(entry.join(CONTENT_ROOT_MARKER))
        .is_ok_and(|bytes| bytes == CONTENT_ROOT_LAYOUT);
    if packaged
        && entry
            .file_name()
            .is_some_and(|name| name == "renzora_plugin")
    {
        if let Some(crates) = entry
            .parent()
            .filter(|path| path.file_name().is_some_and(|name| name == "crates"))
        {
            if let Some(root) = crates
                .parent()
                .filter(|root| root.join("Cargo.toml").is_file())
            {
                return root.to_path_buf();
            }
        }
    }
    entry.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_sdk_hash_includes_sibling_sources() {
        let temp = tempfile::tempdir().expect("SDK root");
        let entry = temp.path().join("crates/renzora_plugin");
        let sibling = temp.path().join("crates/renzora_identity");
        std::fs::create_dir_all(&entry).expect("entry");
        std::fs::create_dir_all(&sibling).expect("sibling");
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").expect("workspace");
        assert_eq!(content_root(&entry), entry);
        std::fs::write(entry.join(CONTENT_ROOT_MARKER), CONTENT_ROOT_LAYOUT).expect("marker");
        assert_eq!(content_root(&entry), temp.path());
        let before = crate::compiler::hash_directory(&content_root(&entry));
        std::fs::write(sibling.join("lib.rs"), "changed identity").expect("sibling edit");
        assert_ne!(
            before,
            crate::compiler::hash_directory(&content_root(&entry))
        );
    }
}
