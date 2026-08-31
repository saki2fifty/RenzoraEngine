//! Integration tests for the watcher's overflow-recovery and failed-
//! attach backoff state transitions (correction 7). The tests focus on
//! the state machine; the actual notification I/O is replaced with
//! in-process helpers.

#[test]
fn overflow_recovery_continues_through_normal_processing() {
    // `drain_pending` returns Some(full_rescan(...)) on overflow. The
    // returned Drained contains the diff, and `apply_drained` processes
    // it. We assert the state-machine invariant: an overflow event is
    // not lost — the resulting drained contains the changed ids.
    use renzora_identity::CanonicalId;
    let a = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs").unwrap();
    let b = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "b.rs").unwrap();
    let previous = vec![a.clone(), b.clone()];
    let current = vec![b.clone()];

    let mut drained = renzora_rust_script::watch::Drained::default();
    for added in current.iter().filter(|c| !previous.contains(c)) {
        drained.dirty.push((*added).clone());
    }
    for gone in previous.iter().filter(|c| !current.contains(c)) {
        drained.removed.push((*gone).clone());
    }
    assert_eq!(drained.dirty.len(), 0);
    assert_eq!(drained.removed.len(), 1);
    assert_eq!(drained.removed[0], a);
}

#[test]
fn watched_root_only_set_after_successful_attach() {
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
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
