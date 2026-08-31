//! Integration tests for the script watcher's idle behaviour.
//!
//! Phase 1 commit 1.2 promised an event-driven watcher; correction 1
//! finishes that promise by ensuring the watcher's reconcile path does no
//! work on idle frames. The `dispatch_no_full_rescan_on_idle_update` test
//! drives the watcher's per-frame reconcile logic directly using the
//! `pub(crate)` helpers in `watch.rs`. The discovery counter is exposed
//! through `renzora_rust_script::discovery::collect_call_count`.

#[test]
fn dispatch_no_full_rescan_on_idle_update() {
    // A small project tree the watcher can attach to. The tree contains
    // a single declared script so the initial rescan has something to
    // find.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let script = root.join("a.rs");
    std::fs::write(&script, "").unwrap();

    // Fresh watcher + reset call counter.
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::discovery::reset_collect_call_count();

    // Frame 1: initial attach + initial rescan. This is the only frame
    // that may trigger a discovery call.
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    assert!(
        renzora_rust_script::watch::is_attached(&watcher),
        "initial attach should succeed"
    );
    let initial_calls = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
    assert!(
        initial_calls >= 1,
        "initial attach should trigger at least one discovery call (got {initial_calls})"
    );
    // The next frame should NOT need the initial rescan.
    assert!(
        !renzora_rust_script::watch::needs_initial_rescan(&watcher),
        "needs_initial_rescan should be cleared after the first frame"
    );

    // Frames 2..=32: idle Update frames. Each must perform zero
    // discovery calls. We accumulate per-frame counts to validate the
    // total of discovery work across idle frames is zero.
    let mut idle_total = 0usize;
    for _ in 0..31 {
        let calls = renzora_rust_script::watch::reconcile_one_frame(&mut watcher, root);
        idle_total += calls;
        assert_eq!(
            calls, 0,
            "idle Update frames must perform zero discovery calls"
        );
    }
    assert_eq!(
        idle_total, 0,
        "idle Update frames combined must perform zero discovery calls"
    );
}
