//! End-to-end test for copy-based export.
//!
//! Phase 4 changed the source of compiled script artifacts from the
//! legacy project-local `<project>/.renzora/scripts/<dir>/` directory
//! to the shared `renzora_compiler_cache::BuildService` cache. The
//! export reads the active published artifact for each canonical id
//! from the cache and copies it into the export's `<output>/scripts/`
//! directory, recording a `scripts.index` manifest the runtime's
//! `load_prebuilt_scripts` reads on startup.
//!
//! These tests construct a real `BuildService` with a tempdir cache
//! root, seed the cache with a placeholder artifact for each
//! canonical id, then drive `stage_prebuilt_scripts` against the
//! same service. The artifacts are arbitrary bytes — the export
//! reads them as files without parsing.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use renzora_compiler_cache::{
    service::BuildServiceConfig,
    staging::ActivePointer,
    types::{ArtifactKind, BuildProfile, PublishedGeneration},
    BuildService,
};
use renzora_identity::CanonicalId;

fn script_body() -> &'static str {
    "fn update(_: &renzora_plugin::script::Ctx, _r: &mut renzora_plugin::script::ScriptReply) -> Result<(), String> { Ok(()) }\nrenzora_plugin::rust_script!(update);\n"
}

/// Build a `BuildService` against a tempdir cache root. The SDK path
/// is irrelevant — these tests never trigger an actual compile. They
/// seed the cache directly.
fn build_test_service(cache_root: &std::path::Path) -> Arc<BuildService> {
    let sdk = cache_root.join("sdk");
    fs::create_dir_all(&sdk).unwrap();
    let cfg = BuildServiceConfig {
        cache_root: cache_root.to_path_buf(),
        profile: BuildProfile::Dist,
        sdk_path: sdk,
        toolchain_stamp: "test".into(),
        compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
        n_workers: Some(1),
        n_children: Some(1),
        shutdown_deadline: std::time::Duration::from_secs(5),
        required_symbols_by_kind: std::collections::HashMap::from([(
            ArtifactKind::Tier1Script,
            vec![b"renzora_plugin_tier1_script_desc\0".to_vec()],
        )]),
    };
    BuildService::new(cfg).expect("BuildService::new")
}

/// Seed the cache with an `active.bin` pointer and a placeholder
/// artifact. The export reads the active pointer via the public
/// `BuildService::cache::read_active` API and resolves the artifact
/// path via `BuildService::cache::artifact_path`, so we must use the
/// same encoding for the active pointer the cache expects.
fn seed_cache(
    cache: &renzora_compiler_cache::ArtifactCache,
    id: &CanonicalId,
    lib_ext: &str,
) -> PathBuf {
    let active = ActivePointer {
        generation: PublishedGeneration(1),
        fingerprint_hash: [0; 32],
        compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
    };
    cache.write_active(id, &active, true).expect("write_active");

    let artifact_path = cache.artifact_path(id, PublishedGeneration(1), lib_ext);
    if let Some(parent) = artifact_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&artifact_path, b"placeholder artifact\n").unwrap();
    artifact_path
}

#[allow(dead_code)]
fn safe_dir_name(id: &CanonicalId) -> String {
    let mut out = String::new();
    for ch in id.to_scheme_path().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

fn stage_lib_in_cache(
    service: &BuildService,
    _cache_root: &std::path::Path,
    id: &CanonicalId,
    lib_ext: &str,
) {
    let artifact = seed_cache(service.cache(), id, lib_ext);
    assert!(artifact.is_file());
}

#[test]
fn copy_export_round_trip_with_duplicate_leaf_names() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(project.join("enemies")).unwrap();
    fs::create_dir_all(project.join("props")).unwrap();
    fs::write(project.join("enemies/spin.rs"), script_body()).unwrap();
    fs::write(project.join("props/spin.rs"), script_body()).unwrap();
    fs::create_dir_all(project.join("unrelated")).unwrap();
    fs::write(project.join("unrelated/other.rs"), "fn helper() {}\n").unwrap();

    let cache_root = tmp.path().join("cache");
    let service = build_test_service(&cache_root);

    let enemy_id =
        CanonicalId::from_rooted(renzora_identity::RootKind::Project, "enemies/spin.rs").unwrap();
    let props_id =
        CanonicalId::from_rooted(renzora_identity::RootKind::Project, "props/spin.rs").unwrap();
    stage_lib_in_cache(&service, &cache_root, &enemy_id, "so");
    stage_lib_in_cache(&service, &cache_root, &props_id, "so");

    let output = tempfile::tempdir().unwrap();
    let mut progress_log: Vec<String> = Vec::new();
    let mut progress = |s: String| progress_log.push(s);
    let staged = renzora_export::build::stage_prebuilt_scripts(
        &project,
        output.path(),
        "so",
        &service,
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
    assert!(
        manifest.contains(&enemy_key),
        "manifest must contain {enemy_key}: {manifest}"
    );
    assert!(
        manifest.contains(&props_key),
        "manifest must contain {props_key}: {manifest}"
    );

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
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();

    let cache_root = tmp.path().join("cache");
    let service = build_test_service(&cache_root);

    let id =
        CanonicalId::from_rooted(renzora_identity::RootKind::Project, "enemy/spin.rs").unwrap();
    stage_lib_in_cache(&service, &cache_root, &id, "so");

    let output = tempfile::tempdir().unwrap();
    let mut progress = |_s: String| {};
    let staged = renzora_export::build::stage_prebuilt_scripts(
        &project,
        output.path(),
        "so",
        &service,
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
fn copy_export_missing_published_artifact_is_reported() {
    // A canonical id the editor has never compiled has no active.bin
    // in the cache; the export must skip it (not fail).
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();

    let cache_root = tmp.path().join("cache");
    let service = build_test_service(&cache_root);
    // Note: no stage_lib_in_cache — the script has no compiled artifact.

    let output = tempfile::tempdir().unwrap();
    let mut progress_log: Vec<String> = Vec::new();
    let mut progress = |s: String| progress_log.push(s);
    let staged = renzora_export::build::stage_prebuilt_scripts(
        &project,
        output.path(),
        "so",
        &service,
        &mut progress,
    )
    .expect("stage ok");
    assert_eq!(staged, 0, "uncompiled script must not be staged");
    assert!(progress_log
        .iter()
        .any(|s| s.contains("no compiled library") || s.contains("no published artifact")));
}
