//! Integration tests for the watcher's retire / rename / marker-removal
//! behaviour.
//!
//! These tests drive the production reconcile_batch function directly —
//! the same helper the production `watch` Bevy system calls. They do not
//! maintain a parallel translate() function or a parallel state-machine
//! implementation. Each test sends raw events into the production batch
//! helper and asserts the resulting Plan.

use std::fs;
use std::path::PathBuf;

use renzora_identity::CanonicalId;

fn script_body() -> &'static str {
    "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
}

fn write_script(path: &PathBuf, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Construct a DebouncedEvent whose path list contains one entry.
fn event_with_path(path: PathBuf) -> notify_debouncer_full::DebouncedEvent {
    notify_debouncer_full::DebouncedEvent::new(
        notify_debouncer_full::notify::Event {
            kind: notify_debouncer_full::notify::EventKind::Create(
                notify_debouncer_full::notify::event::CreateKind::File,
            ),
            paths: vec![path],
            attrs: Default::default(),
        },
        std::time::Instant::now(),
    )
}

fn event_removed(path: PathBuf) -> notify_debouncer_full::DebouncedEvent {
    notify_debouncer_full::DebouncedEvent::new(
        notify_debouncer_full::notify::Event {
            kind: notify_debouncer_full::notify::EventKind::Remove(
                notify_debouncer_full::notify::event::RemoveKind::File,
            ),
            paths: vec![path],
            attrs: Default::default(),
        },
        std::time::Instant::now(),
    )
}

#[test]
fn editing_a_non_script_file_schedules_no_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let helper = root.join("enemy/helper.rs");
    write_script(&helper, "fn helper() {}\n"); // no declaration

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);

    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_with_path(helper));
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root);
    assert!(
        plan.is_none(),
        "non-script file edits produce no plan (got {:?})",
        plan
    );
}

#[test]
fn adding_the_marker_turns_it_into_a_script() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let spin = root.join("enemy/spin.rs");
    write_script(&spin, script_body());

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);

    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_with_path(spin));
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root)
        .expect("a Create event for a declared script produces a plan");
    assert_eq!(plan.dirty.len(), 1);
    assert_eq!(plan.dirty[0].path(), "enemy/spin.rs");
    assert!(plan.removed.is_empty());
}

#[test]
fn removing_the_marker_retires_a_previously_loaded_script() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let spin = root.join("enemy/spin.rs");
    write_script(&spin, script_body());

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "enemy/spin.rs").unwrap();
    watcher.mark_seen(id.clone());

    // Lose the declaration by overwriting the file with empty source.
    fs::write(&spin, "").unwrap();
    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_with_path(spin));
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root)
        .expect("marker-removal on a previously-loaded script produces a plan");
    assert!(plan.removed.contains(&id));
    assert!(!plan.dirty.contains(&id));
}

#[test]
fn rename_removes_old_identity_and_adds_new_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let old = root.join("enemy/spin.rs");
    let new = root.join("props/spin.rs");
    write_script(&old, script_body());

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let old_id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "enemy/spin.rs").unwrap();
    watcher.mark_seen(old_id.clone());

    // Simulate an atomic rename: the old path is reported missing, the
    // new path is reported present with the declaration.
    fs::remove_file(&old).unwrap();
    write_script(&new, script_body());
    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_removed(old));
    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_with_path(new));

    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root)
        .expect("rename batch produces a plan");
    let new_id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "props/spin.rs").unwrap();
    assert!(plan.removed.contains(&old_id));
    assert!(plan.dirty.contains(&new_id));
    assert!(!plan.dirty.contains(&old_id));
    assert!(!plan.removed.contains(&new_id));
}

#[test]
fn delete_of_known_path_retires_id_via_lexical_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let spin = root.join("enemy/spin.rs");
    write_script(&spin, script_body());

    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    renzora_rust_script::watch::attach_debouncer_for_test(&mut watcher, root);
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "enemy/spin.rs").unwrap();
    watcher.mark_seen(id.clone());

    fs::remove_file(&spin).unwrap();
    renzora_rust_script::watch::inject_event_for_test(&mut watcher, event_removed(spin));
    let plan = renzora_rust_script::watch::reconcile_batch_for_test(&mut watcher, root)
        .expect("delete of a known id produces a plan");
    assert!(plan.removed.contains(&id));
}
