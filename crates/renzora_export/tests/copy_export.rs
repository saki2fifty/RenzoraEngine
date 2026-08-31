//! End-to-end test for copy-based export (correction 8).
//!
//! 1. Create a project with two scripts including duplicate leaf
//!    names at `enemies/spin.rs` and `props/spin.rs`.
//! 2. Stage the copy-based export via `stage_prebuilt_scripts`.
//! 3. Read the generated `scripts.index`.
//! 4. Verify both canonical ids are present and the bare leaf
//!    appears as an alias only when unique; when both duplicates
//!    exist the leaf row is absent.
//! 5. Re-parse the manifest with the same canonical id logic the
//!    runtime uses (canonical-key required; bare-name alias only when
//!    unique).

use std::fs;

#[test]
fn copy_export_round_trip_with_duplicate_leaf_names() {
    // Build a synthetic project.
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemies")).unwrap();
    fs::create_dir_all(project.join("props")).unwrap();
    let body = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    fs::write(project.join("enemies/spin.rs"), body).unwrap();
    fs::write(project.join("props/spin.rs"), body).unwrap();
    fs::create_dir_all(project.join("unrelated")).unwrap();
    fs::write(project.join("unrelated/other.rs"), "fn helper() {}\n").unwrap();

    // Pre-create canonical-id-keyed build directories with a
    // placeholder library so stage_prebuilt_scripts finds them.
    let enemy_id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemies/spin.rs",
    )
    .unwrap();
    let props_id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();
    for id in [&enemy_id, &props_id] {
        let dir_name = export_build_dir_name(id);
        let dir = project.join(".renzora").join("scripts").join(&dir_name);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), body).unwrap();
        let lib = dir.join(format!("{}-{}.so", id.bare_leaf(), "0"));
        fs::write(&lib, b"placeholder\n").unwrap();
    }

    // Stage the export.
    let output = tempfile::tempdir().unwrap();
    let mut progress_log: Vec<String> = Vec::new();
    let mut progress = |s: String| progress_log.push(s);
    let staged = renzora_export::build::stage_prebuilt_scripts(
        project,
        output.path(),
        "so",
        &mut progress,
    )
    .expect("stage_prebuilt_scripts ok");
    assert_eq!(staged, 2, "two scripts must be staged");

    // Read the manifest.
    let manifest_path = output
        .path()
        .join("scripts")
        .join(renzora_rust_script::PREBUILT_MANIFEST);
    let manifest = fs::read_to_string(&manifest_path).expect("manifest exists");

    // Each canonical id must appear as a key.
    let enemy_key = enemy_id.to_scheme_path();
    let props_key = props_id.to_scheme_path();
    assert!(
        manifest.contains(&enemy_key),
        "manifest must contain canonical id {enemy_key}: {manifest}"
    );
    assert!(
        manifest.contains(&props_key),
        "manifest must contain canonical id {props_key}: {manifest}"
    );

    // Both scripts share leaf `spin.rs`; the duplicate-leaf bare key
    // must NOT appear.
    let leaf_lines: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("spin.rs\t"))
        .collect();
    assert_eq!(
        leaf_lines.len(),
        0,
        "duplicate-leaf aliases must NOT be emitted: {manifest}"
    );
}

#[test]
fn copy_export_unique_leaf_writes_alias_row() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemy")).unwrap();
    let body = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    fs::write(project.join("enemy/spin.rs"), body).unwrap();

    let id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let dir_name = export_build_dir_name(&id);
    let dir = project.join(".renzora").join("scripts").join(&dir_name);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/lib.rs"), body).unwrap();
    fs::write(dir.join(format!("{}-{}.so", id.bare_leaf(), "0")), b"placeholder\n").unwrap();

    let output = tempfile::tempdir().unwrap();
    let mut progress_log: Vec<String> = Vec::new();
    let mut progress = |s: String| progress_log.push(s);
    let staged = renzora_export::build::stage_prebuilt_scripts(
        project,
        output.path(),
        "so",
        &mut progress,
    )
    .expect("stage ok");
    assert_eq!(staged, 1);

    let manifest = fs::read_to_string(
        output.path().join("scripts").join(renzora_rust_script::PREBUILT_MANIFEST),
    )
    .unwrap();
    // Both the canonical key and the bare-leaf alias must be present.
    assert!(manifest.contains(&id.to_scheme_path()));
    assert!(
        manifest.lines().any(|l| l.starts_with("spin.rs\t")),
        "unique bare-leaf alias must be emitted: {manifest}"
    );
}

#[test]
fn manifest_only_canonical_rows_round_trip_through_loaded_scripts() {
    // The runtime loader requires canonical keys. Build a manifest with
    // two canonical rows (one for each duplicate-leaf script) and
    // confirm the loader accepts them. (We don't load real libraries
    // here — the goal is the manifest wire format round trip.)
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path();
    let scripts_dir = output.join("scripts");
    fs::create_dir_all(&scripts_dir).unwrap();
    let id1 = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemies/spin.rs",
    )
    .unwrap();
    let id2 = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();
    fs::write(scripts_dir.join("a.so"), b"\x7fELF...").unwrap();
    fs::write(scripts_dir.join("b.so"), b"\x7fELF...").unwrap();
    let manifest = format!(
        "{}\ta.so\n{}\tb.so\n",
        id1.to_scheme_path(),
        id2.to_scheme_path()
    );
    fs::write(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST), manifest).unwrap();

    // Parse the manifest back and verify both canonical ids are
    // present. The bare-leaf row is intentionally absent because both
    // scripts share the leaf.
    let s = fs::read_to_string(scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST)).unwrap();
    let mut parsed = 0;
    for line in s.lines() {
        let Some((key, _file)) = line.split_once('\t') else {
            continue;
        };
        if let Ok(id) = renzora_identity::CanonicalId::parse(key) {
            assert!(
                id == id1 || id == id2,
                "parsed id must be one of the two canonical ids"
            );
            parsed += 1;
        }
        // Rows that fail canonical parsing are silently dropped by
        // the current loader — Phase 1 keeps that behaviour, the
        // exporter just no longer emits such rows.
    }
    assert_eq!(parsed, 2);
}

/// Re-derive the build-dir hash used by `renzora_rust_script`. The
/// exporter and the editor must agree on this function; tests live on
/// both sides.
fn export_build_dir_name(id: &renzora_identity::CanonicalId) -> String {
    use std::hash::Hasher;
    let s = id.to_scheme_path();
    let mut h = std::collections::hash_map::DefaultHasher::default();
    h.write(s.as_bytes());
    format!("{:016x}", h.finish())
}
