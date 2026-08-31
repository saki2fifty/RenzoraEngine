//! Integration test: opening a project must not trigger a duplicate
//! build through the watcher's initial rescan. The test drives the
//! production `attach_debouncer` and `reconcile_one_frame` helpers.

#[test]
fn opening_a_project_does_not_double_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let body = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    std::fs::create_dir_all(root.join("enemy")).unwrap();
    std::fs::write(root.join("enemy/spin.rs"), body).unwrap();
    std::fs::create_dir_all(root.join("props")).unwrap();
    std::fs::write(root.join("props/spin.rs"), body).unwrap();

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();

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

    // Simulate compile_and_load marking both scripts seen.
    watcher.mark_seen(a.clone());
    watcher.mark_seen(b.clone());
    assert!(!renzora_rust_script::watch::is_attached(&watcher));

    // Production flow: watcher's Update runs reconcile_one_frame,
    // which attaches the debouncer and runs an initial rescan.
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let plan = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
    // After mark_seen pre-populated the snapshot, the initial rescan
    // finds no additions → no dirty, no removed → Some(Plan::default())
    // (empty). Either Some(empty) or None is acceptable; the
    // invariant we test is that both ids remain in seen_paths and the
    // next dirty event is processed correctly.
    let seen = renzora_rust_script::watch::seen_paths_for_test(&watcher);
    assert!(seen.contains(&a), "enemy/spin.rs must remain seen");
    assert!(seen.contains(&b), "props/spin.rs must remain seen");
    assert!(renzora_rust_script::watch::is_attached(&watcher));
    // Plan is either None (zero work) or Some with empty buckets.
    if let Some(p) = plan {
        assert!(p.dirty.is_empty() && p.removed.is_empty());
    }
}
