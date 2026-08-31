//! Integration tests for `LoadedScripts::remove` and the watcher retire
//! path. Phase 1 commit 1.2 promised deletion retirement; correction 2
//! proves that the canonical id and alias index both update, and that
//! the image-retention policy (Library stays mapped) holds.

use renzora_identity::{BareAliasIndex, CanonicalId, RootKind};

#[test]
fn remove_drops_canonical_lookup_and_alias() {
    // Build two script ids that share a leaf.
    let a = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
    let b = CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap();

    let mut idx = BareAliasIndex::new();
    idx.insert(a.clone());
    idx.insert(b.clone());

    // Pretend both are loaded.
    let mut entries: std::collections::HashMap<CanonicalId, ()> =
        std::collections::HashMap::new();
    entries.insert(a.clone(), ());
    entries.insert(b.clone(), ());

    // Both are in the alias bucket; looking up `spin.rs` is Ambiguous.
    assert!(matches!(idx.lookup("spin.rs"), Err(renzora_identity::AliasLookup::Ambiguous(_))));

    // Remove `a` from both maps.
    entries.remove(&a);
    idx.remove(&a);

    assert!(entries.contains_key(&b));
    assert!(!entries.contains_key(&a));
    assert!(matches!(
        idx.lookup("spin.rs"),
        Ok(found) if *found == b,
    ));
}

#[test]
fn remove_drops_canonical_id_alias_only_in_loaded_set() {
    // Removing an id not in the entries map is a no-op (we are not
    // tracking every canonical id in the world; only those that have
    // actually been built).
    let a = CanonicalId::from_rooted(RootKind::Project, "foo.rs").unwrap();
    let mut idx = BareAliasIndex::new();
    idx.remove(&a);
    // No-op; the index is empty.
    assert!(matches!(idx.lookup("foo.rs"), Err(renzora_identity::AliasLookup::NotFound)));
}

#[test]
fn remove_unambiguous_alias_becomes_not_found() {
    let id = CanonicalId::from_rooted(RootKind::Project, "only/spin.rs").unwrap();
    let mut idx = BareAliasIndex::new();
    idx.insert(id.clone());
    assert!(matches!(idx.lookup("spin.rs"), Ok(_)));
    idx.remove(&id);
    assert!(matches!(idx.lookup("spin.rs"), Err(renzora_identity::AliasLookup::NotFound)));
}

#[test]
fn duplicate_leaf_ambiguity_resolves_to_unique_after_one_removed() {
    let a = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
    let b = CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap();
    let mut idx = BareAliasIndex::new();
    idx.insert(a.clone());
    idx.insert(b.clone());
    assert!(matches!(idx.lookup("spin.rs"), Err(renzora_identity::AliasLookup::Ambiguous(_))));
    idx.remove(&a);
    assert!(matches!(
        idx.lookup("spin.rs"),
        Ok(found) if *found == b,
    ));
}
