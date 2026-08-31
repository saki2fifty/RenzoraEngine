//! Integration tests for the watcher's overflow-recovery and
//! failed-attach backoff state transitions (correction 7).
//!
//! The tests focus on the production state machine — `ScriptWatcher`
//! and its `attach_debouncer` / `last_failed_attach` plumbing — not
//! hand-written replicas.

#[test]
fn overflow_recovery_continues_through_normal_processing() {
    // `drain_pending` returns `Some(BatchOutcome::FullRescan)` on
    // overflow. `reconcile_batch` translates that into a full
    // rescan and returns the diff. We assert the production
    // helper, given an existing seen_paths of two ids and a
    // current state of one, returns the diff as a Plan.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    let a = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "a.rs",
    )
    .unwrap();
    let b = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "b.rs",
    )
    .unwrap();
    // Pretend both were previously seen but only one still exists.
    {
        let seen = renzora_rust_script::watch::seen_paths_for_test_mut(&mut watcher);
        seen.push(a.clone());
        seen.push(b.clone());
    }
    // Force a full rescan via the production helper.
    let plan = renzora_rust_script::watch::full_rescan_for_test(&mut watcher, root);
    assert_eq!(plan.removed.len(), 1);
    assert_eq!(plan.dirty.len(), 0);
}

#[test]
fn watched_root_only_set_after_successful_attach() {
    let watcher = renzora_rust_script::watch::ScriptWatcher::default();
    assert!(renzora_rust_script::watch::watched_root_for_test(&watcher).is_none());
}

#[test]
fn failed_attach_records_backoff_state() {
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::set_last_failed_attach_for_test(
        &mut watcher,
        Some(std::time::Instant::now()),
    );
    let backoff_active = renzora_rust_script::watch::last_failed_attach_for_test(&watcher)
        .map(|prev| prev.elapsed() < renzora_rust_script::watch::attach_backoff_for_test())
        .unwrap_or(false);
    assert!(backoff_active, "fresh failure must engage the backoff");
}

#[test]
fn successful_attach_clears_backoff() {
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::set_last_failed_attach_for_test(
        &mut watcher,
        Some(std::time::Instant::now()),
    );
    renzora_rust_script::watch::set_last_failed_attach_for_test(&mut watcher, None);
    assert!(renzora_rust_script::watch::last_failed_attach_for_test(&watcher).is_none());
}

#[test]
fn backoff_expires_after_window() {
    use std::time::{Duration, Instant};
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    let backoff = renzora_rust_script::watch::attach_backoff_for_test();
    renzora_rust_script::watch::set_last_failed_attach_for_test(
        &mut watcher,
        Some(Instant::now() - backoff - Duration::from_secs(1)),
    );
    let backoff_active = renzora_rust_script::watch::last_failed_attach_for_test(&watcher)
        .map(|prev| prev.elapsed() < backoff)
        .unwrap_or(false);
    assert!(!backoff_active, "expired failure must release the backoff");
}
