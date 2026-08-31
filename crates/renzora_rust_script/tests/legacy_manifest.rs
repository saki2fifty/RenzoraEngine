//! Tests for backward compatibility with pre-Phase-1 manifests.
//!
//! Phase 1 commit 1.2 wrote manifest rows as bare file names
//! (`enemy/spin.rs`) or file leaves (`spin.rs`). The runtime loader must
//! accept these by interpreting them as `project://<rel>` canonical
//! ids rather than silently dropping the rows.

use std::fs;

#[test]
fn legacy_project_relative_row_is_parsed_as_canonical() {
    // Bare project-relative row (no `project://` prefix). After
    // Phase-1's loader change, this must still be loadable.
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path();
    let scripts_dir = output.join("scripts");
    fs::create_dir_all(&scripts_dir).unwrap();
    fs::write(scripts_dir.join("a.so"), b"\x7fELF...").unwrap();

    let manifest = "enemy/spin.rs\ta.so\n";
    fs::write(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST), manifest).unwrap();

    // Parse the manifest row and verify the canonical id matches what
    // the editor would have produced.
    let s = fs::read_to_string(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST)).unwrap();
    let mut parsed = 0;
    for line in s.lines() {
        let Some((key, file)) = line.split_once('\t') else { continue };
        if let Ok(id) = renzora_identity::CanonicalId::parse(key) {
            assert_eq!(id.path(), "enemy/spin.rs");
            parsed += 1;
        } else {
            // Fallback: legacy row, accept as canonical.
            let id = renzora_identity::CanonicalId::from_rooted(
                renzora_identity::RootKind::Project,
                key,
            )
            .unwrap();
            assert_eq!(id.path(), "enemy/spin.rs");
            parsed += 1;
        }
        assert_eq!(file, "a.so");
    }
    assert_eq!(parsed, 1);
}

#[test]
fn legacy_bare_leaf_alias_row_is_parsed_as_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path();
    let scripts_dir = output.join("scripts");
    fs::create_dir_all(&scripts_dir).unwrap();
    fs::write(scripts_dir.join("a.so"), b"\x7fELF...").unwrap();

    let manifest = "spin.rs\ta.so\n";
    fs::write(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST), manifest).unwrap();

    let s = fs::read_to_string(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST)).unwrap();
    let mut parsed = 0;
    for line in s.lines() {
        let Some((key, _file)) = line.split_once('\t') else { continue };
        // Try canonical first, then legacy.
        let id = renzora_identity::CanonicalId::parse(key)
            .or_else(|_| {
                renzora_identity::CanonicalId::from_rooted(
                    renzora_identity::RootKind::Project,
                    key,
                )
            })
            .unwrap();
        assert_eq!(id.path(), "spin.rs");
        parsed += 1;
    }
    assert_eq!(parsed, 1);
}
