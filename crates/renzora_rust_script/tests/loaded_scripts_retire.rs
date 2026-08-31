//! Integration tests for `LoadedScripts::remove` and the watcher retire
//! path. Phase 1 commit 1.2 promised deletion retirement; these tests
//! drive the production `LoadedScripts::remove` directly, not a
//! hand-written index replica.

#[test]
fn remove_drops_canonical_lookup_and_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
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
    assert!(loaded.is_loaded(&a));
    assert!(loaded.is_loaded(&b));
    let bare = std::path::Path::new("spin.rs");
    match loaded.resolve(bare, root) {
        renzora_rust_script::script_resolve::ResolvedScript::Ambiguous(ids) => {
            assert_eq!(ids.len(), 2);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }

    loaded.remove(&a);
    assert!(!loaded.is_loaded(&a));
    assert!(loaded.is_loaded(&b));
    match loaded.resolve(bare, root) {
        renzora_rust_script::script_resolve::ResolvedScript::Unique(id) => {
            assert_eq!(id, b);
        }
        other => panic!("expected Unique(b), got {other:?}"),
    }
}

#[test]
fn remove_unambiguous_alias_becomes_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "only/spin.rs",
    )
    .unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(id.clone(), f);
    let bare = std::path::Path::new("spin.rs");
    match loaded.resolve(bare, root) {
        renzora_rust_script::script_resolve::ResolvedScript::Unique(_) => {}
        other => panic!("expected Unique, got {other:?}"),
    }
    loaded.remove(&id);
    match loaded.resolve(bare, root) {
        renzora_rust_script::script_resolve::ResolvedScript::NotFound => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn duplicate_leaf_ambiguity_resolves_to_unique_after_one_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
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
    match loaded.resolve(bare, root) {
        renzora_rust_script::script_resolve::ResolvedScript::Ambiguous(_) => {}
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    loaded.remove(&a);
    match loaded.resolve(bare, root) {
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
    // The bare alias index has exactly one entry.
    let entries = loaded.alias_entries_for_test("spin.rs");
    assert_eq!(entries.len(), 1);
}
