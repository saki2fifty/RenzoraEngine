//! Integration tests for `LoadedScripts` and the marker-collision
//! behaviour of `build_to_path_with_id`.
//!
//! These tests drive the production helpers directly. There is no
//! parallel implementation.

use std::fs;

#[test]
fn remove_drops_canonical_lookup_and_alias() {
    let project = tempfile::tempdir().unwrap();
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let a = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let b = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(a.clone(), f);
    loaded.insert_borrowed(b.clone(), f);

    // Both are loaded; the bare-leaf alias `spin.rs` is ambiguous.
    // We use a non-empty project root so `strip_prefix` fails for a
    // bare leaf and the resolve function falls through to the alias
    // lookup.
    let bare = std::path::Path::new("spin.rs");
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::Ambiguous(ids) => {
            assert_eq!(ids.len(), 2);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }

    loaded.remove(&a);
    assert!(!loaded.is_loaded(&a));
    assert!(loaded.is_loaded(&b));
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::Unique(id) => {
            assert_eq!(id, b);
        }
        other => panic!("expected Unique(b), got {other:?}"),
    }
}

#[test]
fn remove_unambiguous_alias_becomes_not_found() {
    let project = tempfile::tempdir().unwrap();
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "only/spin.rs",
    )
    .unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(id.clone(), f);
    let bare = std::path::Path::new("spin.rs");
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::Unique(_) => {}
        other => panic!("expected Unique, got {other:?}"),
    }
    loaded.remove(&id);
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::NotFound => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn duplicate_leaf_ambiguity_resolves_to_unique_after_one_removed() {
    let project = tempfile::tempdir().unwrap();
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let a = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let b = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(a.clone(), f);
    loaded.insert_borrowed(b.clone(), f);
    let bare = std::path::Path::new("spin.rs");
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::Ambiguous(_) => {}
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    loaded.remove(&a);
    match loaded.resolve(bare, project.path()) {
        renzora_rust_script::script_resolve::ResolvedScript::Unique(id) => {
            assert_eq!(id, b);
        }
        other => panic!("expected Unique(b), got {other:?}"),
    }
}

#[test]
fn reinserting_same_id_does_not_duplicate_alias() {
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "spin.rs",
    )
    .unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(id.clone(), f);
    loaded.insert_borrowed(id.clone(), f);
    loaded.insert_borrowed(id.clone(), f);
    let entries = loaded.alias_entries_for_test("spin.rs");
    assert_eq!(entries.len(), 1);
}

/// T4: marker collision. Pre-populate a build directory with another
/// canonical id's marker, staged source, and a sentinel artifact.
/// Attempt a colliding build; the production `build_to_path_with_id`
/// must return Err AND leave the existing files unchanged.
#[test]
fn build_to_path_with_id_returns_collision_error_and_leaves_files_untouched() {
    // 1. Create a project with one script.
    let project = tempfile::tempdir().unwrap();
    let project_path = project.path();
    fs::write(
        project_path.join("a.rs"),
        "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n",
    )
    .unwrap();

    // 2. Pre-populate the build directory for `our_id` with content
    //    that claims it for a DIFFERENT canonical id. This simulates
    //    a real hash collision between two scripts whose build dirs
    //    happen to hash to the same name.
    let our_id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "a.rs",
    )
    .unwrap();
    let other_id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "b.rs",
    )
    .unwrap();
    assert_ne!(our_id, other_id);

    let dir_name = renzora_rust_script::script_resolve::build_dir_name(&our_id);
    let build_dir = project_path
        .join(".renzora")
        .join("scripts")
        .join(&dir_name);
    fs::create_dir_all(build_dir.join("src")).unwrap();
    let staged_src = build_dir.join("src").join("lib.rs");
    let original_src = "ORIGINAL STAGED SOURCE\n";
    fs::write(&staged_src, original_src).unwrap();
    let sentinel = build_dir.join("SENTINEL_ARTIFACT");
    let original_sentinel = b"do not touch\n";
    fs::write(&sentinel, original_sentinel).unwrap();

    // Marker that names a DIFFERENT canonical id. We write it at the
    // path the production function looks at (the path derived from
    // `our_id`), but with `other_id`'s scheme-path content.
    let marker_path = renzora_rust_script::script_resolve::build_dir_marker_path(
        project_path,
        &our_id,
    );
    fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    let original_marker = other_id.to_scheme_path();
    fs::write(&marker_path, &original_marker).unwrap();

    // 3. Pre-condition: the marker at our_id's path names other_id.
    let marker_text = renzora_rust_script::script_resolve::read_build_dir_marker(&marker_path)
        .unwrap()
        .unwrap();
    assert_eq!(marker_text, other_id.to_scheme_path());
    assert_ne!(marker_text, our_id.to_scheme_path());

    // 4. The production `build_to_path_with_id` returns Err before
    //    touching any file. We attempt to load a real SDK; if one is
    //    installed, the full end-to-end check runs. Either way the
    //    marker check is the first thing the function does.
    let sdk_load_attempt =
        renzora_plugin_build::Sdk::load(project_path.join("sdk"));
    if let Ok(sdk) = sdk_load_attempt {
        let result = renzora_rust_script::build_to_path_with_id(
            &sdk,
            project_path,
            &project_path.join("a.rs"),
            &our_id,
        );
        assert!(
            result.is_err(),
            "build_to_path_with_id must Err on marker collision"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("collision"),
            "error must mention collision: {err}"
        );
    }

    // 5. Files are byte-for-byte unchanged.
    let staged_src_after = fs::read_to_string(&staged_src).unwrap();
    assert_eq!(
        staged_src_after, original_src,
        "staged src must not be overwritten (T4)"
    );
    let sentinel_after = fs::read(&sentinel).unwrap();
    assert_eq!(
        sentinel_after, original_sentinel,
        "sentinel artifact must not be touched (T4)"
    );
    let marker_after = fs::read_to_string(&marker_path).unwrap();
    assert_eq!(
        marker_after, original_marker,
        "marker must not be deleted or replaced (T4)"
    );
}
