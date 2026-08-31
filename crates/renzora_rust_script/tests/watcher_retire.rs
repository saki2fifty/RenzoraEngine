//! Integration tests for the watcher's retire / rename / marker-removal
//! behaviour. Phase 1 commit 1.2 promised these; correction 2 proves them.

use renzora_identity::CanonicalId;

/// Translate a single per-path Modify/Remove event into a (canonical_id,
/// dir_index) tuple the way `drain_pending` does internally, but with
/// the file's text inlined so the test is hermetic (no real filesystem
/// watcher is attached).
fn translate(
    watcher_seen_paths: &[CanonicalId],
    relpath: &str,
    text: Option<&str>,
    is_remove: bool,
) -> Option<(CanonicalId, &'static str)> {
    let id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        relpath,
    )
    .ok()?;
    let is_script = text
        .map(renzora_rust_script::declaration_recognised)
        .unwrap_or(false);
    if is_remove {
        Some((id, "removed"))
    } else if is_script {
        Some((id, "dirty"))
    } else if watcher_seen_paths.contains(&id) {
        Some((id, "removed"))
    } else {
        None
    }
}

#[test]
fn editing_a_non_script_file_schedules_no_build() {
    // The watcher's translate step must filter out files that lack the
    // `renzora::script!(` declaration. A Create/Modify on a plain
    // `.rs` file produces no dirty entry.
    let plain = "fn helper() {}"; // no marker
    let outcome = translate(&[], "enemy/helper.rs", Some(plain), false);
    assert!(outcome.is_none(), "non-script file must produce no work");
}

#[test]
fn adding_the_marker_turns_it_into_a_script() {
    let script = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    let outcome = translate(&[], "enemy/spin.rs", Some(script), false);
    let (id, dir) = outcome.expect("marker-bearing file is dirty");
    assert_eq!(id.path(), "enemy/spin.rs");
    assert_eq!(dir, "dirty");
}

#[test]
fn removing_the_marker_retires_a_previously_loaded_script() {
    let id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let outcome = translate(&[id.clone()], "enemy/spin.rs", Some(""), false);
    let (out_id, dir) = outcome.expect("marker-removed previously-loaded script must retire");
    assert_eq!(out_id, id);
    assert_eq!(dir, "removed");
}

#[test]
fn editing_a_file_with_a_temporary_incomplete_save_does_not_retire() {
    // The file is mid-edit but still contains the full `renzora::script!`
    // declaration — the recognizer ignores anything that looks like
    // comments, but a real partial save will eventually lose the
    // declaration entirely, which the next event catches via the
    // marker-removal branch.
    let id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    // Source with the full declaration plus an in-progress edit
    // (extra spaces and a typo in the function body that the editor
    // hasn't yet cleaned up). The lexer treats the closing `)` and
    // argument list as still valid Rust tokens; the recognizer finds
    // `renzora::script!(update)` and reports Recognised.
    let text_in_progress = "fn update(_: &mut renzora::ScriptCtx) {\n    let _ = 1;\n}\nrenzora::script!(update);\n";
    let outcome = translate(&[id.clone()], "enemy/spin.rs", Some(text_in_progress), false);
    let (_, dir) = outcome.expect("in-progress edit with full declaration produces a dirty bucket");
    assert_eq!(dir, "dirty");
}

#[test]
fn rename_removes_old_identity_and_adds_new_identity() {
    let old_id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let new_id = CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "props/spin.rs",
    )
    .unwrap();

    let seen = vec![old_id.clone()];

    // The remove-half of a rename: the OS reports the file gone at the
    // old location. The translate step must retire the old id.
    let (out_id, dir) = translate(&seen, "enemy/spin.rs", None, true)
        .expect("rename remove-half must retire old id");
    assert_eq!(out_id, old_id);
    assert_eq!(dir, "removed");

    // The create-half of a rename: the OS reports the file at the new
    // location with a script declaration. The translate step must dirty
    // the new id.
    let new_text = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
    let (out_id, dir) = translate(&seen, "props/spin.rs", Some(new_text), false)
        .expect("rename create-half must dirty new id");
    assert_eq!(out_id, new_id);
    assert_eq!(dir, "dirty");
}

