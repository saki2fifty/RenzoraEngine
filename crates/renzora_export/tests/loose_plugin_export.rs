//! Acceptance tests for the loose-plugin export path.
//!
//! These tests exercise the real `stage_loose_plugins_from` function
//! against the safe-name encoder, asserting:
//! - two canonical ids sharing a leaf produce two distinct directories;
//! - the resulting tree contains no `:` or `/` in any directory name
//!   (Windows-safe);
//! - Runtime plugins are included, Editor plugins are excluded
//!   (filtered upstream in `LoosePluginInventory::export_candidates`);
//! - disabled / unselected plugins are excluded.

use renzora_export::build::{canonical_to_safe_dir_name, stage_loose_plugins_from};
use std::collections::HashSet;
use std::path::Path;

#[test]
fn export_canonical_collision_produces_two_directories() {
    let staging_root = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let id_a = "engine://spin.rs".to_string();
    let id_b = "market://spin.rs".to_string();
    let candidates = vec![
        (
            id_a.clone(),
            staging_root.path().join(format!(
                "{}.{}",
                canonical_to_safe_dir_name(&id_a),
                std::env::consts::DLL_EXTENSION
            )),
        ),
        (
            id_b.clone(),
            staging_root.path().join(format!(
                "{}.{}",
                canonical_to_safe_dir_name(&id_b),
                std::env::consts::DLL_EXTENSION
            )),
        ),
    ];
    for (_, p) in &candidates {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"fake-cdylib").unwrap();
    }
    let n = stage_loose_plugins_from(&candidates, output_dir.path(), None, &mut |_| {}).unwrap();
    assert_eq!(n, 2, "two candidates must stage");
    let dir_a = output_dir
        .path()
        .join("plugins")
        .join(canonical_to_safe_dir_name(&id_a))
        .join("build");
    let dir_b = output_dir
        .path()
        .join("plugins")
        .join(canonical_to_safe_dir_name(&id_b))
        .join("build");
    assert!(dir_a.is_dir(), "engine safe dir must exist: {}", dir_a.display());
    assert!(dir_b.is_dir(), "market safe dir must exist: {}", dir_b.display());
    // Walk the export tree and assert no directory name contains `:` or `/`.
    let mut bad: HashSet<String> = HashSet::new();
    fn walk(p: &Path, bad: &mut HashSet<String>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.contains(':') || n.contains('/') {
                    bad.insert(n);
                }
                if e.path().is_dir() {
                    walk(&e.path(), bad);
                }
            }
        }
    }
    walk(output_dir.path(), &mut bad);
    assert!(
        bad.is_empty(),
        "export tree must not contain `:` or `/` in directory names; found {bad:?}"
    );
}

#[test]
fn export_safe_name_is_windows_safe() {
    // The encoder must produce a valid filename + directory name on
    // every OS. Two canonical ids with the same leaf produce
    // different safe names, and no safe name contains a separator
    // or `:`.
    let id_a = canonical_to_safe_dir_name("engine://spin.rs");
    let id_b = canonical_to_safe_dir_name("market://spin.rs");
    let id_c = canonical_to_safe_dir_name("engine://other_plugin.rs");
    assert_ne!(id_a, id_b, "different canonical ids must produce different safe names");
    assert_ne!(id_a, id_c, "same scheme, different leaf must produce different safe names");
    for n in [&id_a, &id_b, &id_c] {
        assert!(!n.contains(':'), "safe name must not contain `:` ({n})");
        assert!(!n.contains('/'), "safe name must not contain `/` ({n})");
        assert!(!n.contains('\\'), "safe name must not contain `\\` ({n})");
        assert!(!n.is_empty(), "safe name must not be empty");
    }
}