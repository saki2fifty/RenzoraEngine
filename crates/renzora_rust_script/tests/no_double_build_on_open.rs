//! Integration test: opening a project must not trigger a duplicate
//! build through the watcher's initial rescan. The contract is that
//! `compile_and_load` marks each script seen, the watcher then attaches
//! and seeds its own `seen_paths` snapshot, and the next reconcile's
//! `full_rescan` finds every script already present (zero dirty).

#[test]
fn opening_a_project_does_not_double_build() {
    // Two scripts in separate folders with the same leaf name.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let body = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    std::fs::create_dir_all(root.join("enemy")).unwrap();
    std::fs::write(root.join("enemy/spin.rs"), body).unwrap();
    std::fs::create_dir_all(root.join("props")).unwrap();
    std::fs::write(root.join("props/spin.rs"), body).unwrap();

    // Simulate the post-open state. `compile_and_load` already marked
    // each script seen. The watcher will attach and seed its own
    // snapshot from `discovery::collect_canonical_scripts`.
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();

    // Drive the post-open flow:
    //   1. compile_and_load marks each script seen.
    //   2. The watcher's Update system runs `watch()` on the next
    //      frame, which detects the project-path change and calls
    //      attach_debouncer.
    //   3. attach_debouncer sets `seen_paths` and
    //      `needs_initial_rescan = true`.
    //   4. The next reconcile's full_rescan reports zero dirty.
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
    watcher.mark_seen(a.clone());
    watcher.mark_seen(b.clone());

    // The watcher has not been attached yet (no debouncer, no watched_root).
    assert!(!renzora_rust_script::watch::is_attached(&watcher));

    // The next watcher's Update fires; reconcile_one_frame attaches the
    // debouncer, seeds seen_paths from the discovery walk, and runs
    // the initial rescan. That rescan must report zero dirty
    // (everything is already in the snapshot compile_and_load seeded).
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let calls = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
    assert_eq!(
        calls, 0,
        "no duplicate build on project open: every script is already in seen_paths"
    );
    // attach_debouncer seeds the snapshot directly; the initial
    // rescan finds nothing new.
    assert!(renzora_rust_script::watch::is_attached(&watcher));
    // Sanity: the snapshot contains both ids.
    assert!(renzora_rust_script::watch::seen_paths_for_test(&watcher).contains(&a));
    assert!(renzora_rust_script::watch::seen_paths_for_test(&watcher).contains(&b));
}
