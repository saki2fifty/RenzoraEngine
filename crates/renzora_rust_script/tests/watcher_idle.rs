//! Integration tests for the script watcher's idle behaviour.
//!
//! Idle frames must do no discovery work. The test drives the production
//! `reconcile_one_frame` helper and asserts it returns `None` on idle
//! frames. The `needs_initial_rescan` flag and the root-level file
//! edits must NOT trigger a full project rescan (corrections A and N).

#[test]
fn idle_frames_produce_no_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();

    // Frame 1: production attach seeds seen_paths from a discovery walk.
    // No separate initial rescan is needed.
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let first = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
    // After attach, no events have arrived and the initial rescan is a
    // no-op when seen_paths is already populated. The plan may be None
    // or an empty Plan; either way no discovery work is repeated.
    if let Some(ref p) = first {
        assert!(p.dirty.is_empty() && p.removed.is_empty());
    }
    assert!(
        !renzora_rust_script::watch::needs_initial_rescan(&watcher),
        "needs_initial_rescan cleared after first frame"
    );

    // Frames 2..=32: no events arrived, so reconcile_batch returns
    // None. Idle frames produce no plan and no discovery work.
    for _ in 0..31 {
        let plan = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
        assert!(
            plan.is_none(),
            "idle frames must produce no plan (correction A/N)"
        );
    }
}

#[test]
fn root_level_file_edit_does_not_force_full_rescan() {
    // Correction N: editing a normal root-level .rs file uses targeted
    // reconciliation. Only directory topology changes force a full
    // rescan.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let _ = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);

    let path = root.join("a.rs");
    renzora_rust_script::watch::inject_event_for_test(
        &mut watcher,
        notify_debouncer_full::DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: notify_debouncer_full::notify::EventKind::Modify(
                    notify_debouncer_full::notify::event::ModifyKind::Any,
                ),
                paths: vec![path],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        ),
    );
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root)
        .expect("a Modify event at root produces a targeted plan");
    assert_eq!(plan.dirty.len(), 1);
    assert_eq!(plan.dirty[0].path(), "a.rs");
}

#[test]
fn root_level_directory_change_forces_full_rescan() {
    // Directory topology changes at the project root force a full
    // rescan. File-level edits at the root do not.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("subdir")).unwrap();
    std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();
    std::fs::write(root.join("subdir/b.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let _ = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);

    // Directory create at project root: full rescan is required.
    let path = root.join("subdir");
    renzora_rust_script::watch::inject_event_for_test(
        &mut watcher,
        notify_debouncer_full::DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: notify_debouncer_full::notify::EventKind::Create(
                    notify_debouncer_full::notify::event::CreateKind::Folder,
                ),
                paths: vec![path],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        ),
    );
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root);
    assert!(plan.is_some(), "directory create at root produces a plan");
}
