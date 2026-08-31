//! End-to-end test for copy-based export.
//!
//! Tests build a synthetic project, stage it through the production
//! `stage_prebuilt_scripts`, then parse the resulting manifest using
//! the same logic the runtime's loader uses. The build-dir hash used
//! to pre-stage artefacts is `renzora_rust_script::script_resolve::
//! build_dir_name` — the SAME function the exporter calls — so the
//! test never reproduces the hash algorithm.

use std::fs;
use std::path::PathBuf;

use renzora_identity::CanonicalId;
use renzora_rust_script::script_resolve::{
    build_dir_marker_path, build_dir_name, write_build_dir_marker,
};

fn script_body() -> &'static str {
    "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
}

fn stage_lib(project: &std::path::Path, id: &CanonicalId, lib_ext: &str) -> PathBuf {
    // Place a placeholder library inside the editor's build dir for
    // this id, keyed off the SAME hash the production exporter uses.
    let dir_name = build_dir_name(id);
    let dir = project.join(".renzora").join("scripts").join(&dir_name);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src").join("lib.rs"), script_body()).unwrap();
    let lib = dir.join(format!("{}-{}.{lib_ext}", id.bare_leaf(), "0"));
    fs::write(&lib, b"placeholder\n").unwrap();
    // Marker must match — the exporter verifies it.
    write_build_dir_marker(project, id).unwrap();
    lib
}

#[test]
fn copy_export_round_trip_with_duplicate_leaf_names() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemies")).unwrap();
    fs::create_dir_all(project.join("props")).unwrap();
    fs::write(project.join("enemies/spin.rs"), script_body()).unwrap();
    fs::write(project.join("props/spin.rs"), script_body()).unwrap();
    fs::create_dir_all(project.join("unrelated")).unwrap();
    fs::write(project.join("unrelated/other.rs"), "fn helper() {}\n").unwrap();

    let enemy_id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemies/spin.rs",
    )
    .unwrap();
    let props_id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();
    stage_lib(project, &enemy_id, "so");
    stage_lib(project, &props_id, "so");

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

    let manifest = fs::read_to_string(
        output
            .path()
            .join("scripts")
            .join(renzora_rust_script::PREBUILT_MANIFEST),
    )
    .unwrap();

    let enemy_key = enemy_id.to_scheme_path();
    let props_key = props_id.to_scheme_path();
    assert!(manifest.contains(&enemy_key), "manifest must contain {enemy_key}: {manifest}");
    assert!(manifest.contains(&props_key), "manifest must contain {props_key}: {manifest}");

    // Correction H: no bare-leaf rows for ambiguous leaves.
    let bare_lines: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("spin.rs\t"))
        .collect();
    assert!(
        bare_lines.is_empty(),
        "duplicate-leaf aliases must NOT be emitted: {manifest}"
    );
}

#[test]
fn copy_export_unique_leaf_emits_only_canonical_row() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();

    let id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    stage_lib(project, &id, "so");

    let output = tempfile::tempdir().unwrap();
    let mut progress = |_s: String| {};
    let staged = renzora_export::build::stage_prebuilt_scripts(
        project,
        output.path(),
        "so",
        &mut progress,
    )
    .expect("stage ok");
    assert_eq!(staged, 1);

    let manifest = fs::read_to_string(
        output
            .path()
            .join("scripts")
            .join(renzora_rust_script::PREBUILT_MANIFEST),
    )
    .unwrap();
    // Exactly one canonical row. Bare-leaf alias is derived by
    // LoadedScripts::insert, not emitted as a competing canonical id.
    let canonical_lines: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("project://"))
        .collect();
    assert_eq!(canonical_lines.len(), 1);
    assert!(manifest.contains(&id.to_scheme_path()));
    assert!(!manifest.lines().any(|l| l.starts_with("spin.rs\t")));
}

#[test]
fn copy_export_refuses_stale_marker() {
    // Correction K: marker mismatch must be rejected.
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();

    let id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();

    // Build a build dir but write a STALE marker (different id).
    let dir_name = build_dir_name(&id);
    let dir = project.join(".renzora").join("scripts").join(&dir_name);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src").join("lib.rs"), script_body()).unwrap();
    fs::write(dir.join(format!("spin-0.so")), b"placeholder\n").unwrap();
    let stale_marker = build_dir_marker_path(project, &id);
    fs::create_dir_all(stale_marker.parent().unwrap()).unwrap();
    fs::write(&stale_marker, "project://other/path.rs").unwrap();

    let output = tempfile::tempdir().unwrap();
    let mut progress = |_s: String| {};
    let staged = renzora_export::build::stage_prebuilt_scripts(
        project,
        output.path(),
        "so",
        &mut progress,
    )
    .expect("stage ok");
    assert_eq!(staged, 0, "stale-marker directory must be refused");

    let scripts_dir = output.path().join("scripts");
    assert!(!scripts_dir.join(renzora_rust_script::PREBUILT_MANIFEST).exists());
}
