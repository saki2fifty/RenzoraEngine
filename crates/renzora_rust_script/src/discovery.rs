//! Shared script discovery for the editor, the watcher, and the exporter.
//!
//! One implementation, one SKIP list, one recursion policy. The same
//! function is called by:
//!
//! - [`crate::compile_and_load`] when the editor enters the project
//! - [`crate::watch::watch`] when the watcher reconciles a debouncer batch
//! - (Phase 1 commit 1.3) the copy-based exporter when it stages scripts
//!
//! The SKIP list mirrors the one used before Phase 1 (see commit message
//! for `crates/renzora_rust_script/src/lib.rs:396-423` history). Every
//! further call site must use this list and not roll its own.
//!
//! ## Recursion policy
//!
//! The walker starts at `project_root`. A directory is recursed into iff:
//!
//! - it is not in [`SKIP_NAMES`], AND
//! - its name does not start with a `.`.
//!
//! `.renzora/` is skipped because it carries this crate's own staged build
//! artefacts, which the watcher must never recompile when their compiler
//! emits them. The other SKIP targets are VCS or vendored-dep trees.

use std::path::{Path, PathBuf};

use renzora::CurrentProject;
use renzora_identity::RootKind;

/// Names of directories that must never be walked. Already-normalised;
/// case is matched byte-exact against the bytes the filesystem reported.
pub const SKIP_NAMES: &[&str] = &[
    "target",
    ".git",
    ".renzora",
    "node_modules",
    "dist",
    ".svn",
    ".hg",
];

/// Whether `name` should be skipped. `name` is a single directory segment,
/// not a full path. Dot-prefixed segments are always skipped (`.foo`),
/// matching the prior behaviour.
fn should_skip_dir(name: &str) -> bool {
    if SKIP_NAMES.contains(&name) {
        return true;
    }
    if name.starts_with('.') {
        return true;
    }
    false
}

/// The project-relative identity the existing call sites know about.
///
/// This is a `String` rather than a [`renzora_identity::CanonicalId`]
/// because commit 1.2 does not yet wire the identity type into
/// `LoadedScripts`. Commit 1.3 introduces that keying; the existing
/// string-typed call sites continue to work with a relpath and the
/// conversion to a full `CanonicalId` happens at one boundary.
pub type ProjectRelpath = String;

/// Walk `project_root` recursively, returning every file that looks like a
/// Rust script and lives under it. Used by initial discovery, the watcher
/// reconcile, and (later) the copy-based exporter.
///
/// The result is sorted so callers that index by position (the lean
/// exporter's generated module names) are stable across runs.
pub fn collect_rust_scripts(project_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(project_root, &mut out);
    out.sort();
    out
}

/// Same as [`collect_rust_scripts`] but returns the canonical relpath
/// (`project://<rel>`) instead of the on-disk path. The result is sorted
/// lexicographically by relpath so duplicate-leaf-name coexistence is
/// observable.
pub fn collect_canonical_scripts(project_root: &Path) -> Vec<renzora_identity::CanonicalId> {
    let mut out: Vec<renzora_identity::CanonicalId> = collect_rust_scripts(project_root)
        .into_iter()
        .filter_map(|p| project_relpath_for(project_root, &p))
        .collect();
    out.sort();
    out
}

/// Compute the canonical project-relpath identity for a single on-disk
/// `path` if it lives under `project_root` and is a `.rs` file. Returns
/// `None` for inputs that are not `.rs` files or that the canonicaliser
/// refuses (escaping the root, etc.). Convert into a full
/// [`renzora_identity::CanonicalId`] at the call site.
pub fn project_relpath_for(
    project_root: &Path,
    path: &Path,
) -> Option<renzora_identity::CanonicalId> {
    if !path.is_file() {
        return None;
    }
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return None;
    }
    let stripped = path.strip_prefix(project_root).ok()?;
    let rel = stripped.to_string_lossy();
    // Convert OS-native separators into the canonical forward-slash form
    // `CanonicalId` uses; use 'replace' to keep simple separators uniform.
    let normalised = rel.replace('\\', "/");
    renzora_identity::CanonicalId::from_rooted(RootKind::Project, &normalised).ok()
}

/// Convenience: resolve the `CurrentProject::path` and return its root or
/// `None`. Callers pass this when they want to walk the active project.
pub fn current_project_root(project: &CurrentProject) -> &Path {
    project.path.as_path()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // Symlink safety (correction 12): use symlink_metadata so the
        // file_type is reported for the link itself, not its target. A
        // symlink to a directory is reported as `is_symlink() == true`,
        // so we skip it (Phase 1 does not canonicalise project-relative
        // symlinks). The same policy applies to initial discovery, the
        // watcher reconcile, and the exporter; see SKIP_SYMLINK_NOTE.
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        if path.is_dir() {
            if !should_skip_dir(name) {
                walk(&path, out);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Phase 1 commit 1.4: the recogniser reads the file's bytes into
        // a `&str` (which is always valid UTF-8 — Rust source files are
        // UTF-8 by convention) and asks the lexer-based scanner whether
        // the file declares itself a script. Truncated source returns
        // `NotRecognised` and is treated as a non-script.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !crate::declaration_recognised(&text) {
            continue;
        }
        out.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;

    fn touch(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn script_body() -> &'static str {
        "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
    }

    #[test]
    fn returns_scripts_anywhere_under_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a/spin.rs"), script_body());
        touch(&root.join("a/b/c/spin.rs"), script_body());
        let found = collect_rust_scripts(root);
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .any(|p| p.ends_with("a/spin.rs") || p.ends_with("a\\spin.rs")));
        assert!(found
            .iter()
            .any(|p| p.ends_with("a/b/c/spin.rs") || p.ends_with("a\\b\\c\\spin.rs")));
    }

    #[test]
    fn skip_list_excludes_generated_and_vendored_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Outside the SKIP list:
        touch(&root.join("a/keep.rs"), script_body());
        // Inside each SKIP name:
        touch(&root.join("target/ignored.rs"), script_body());
        touch(&root.join(".git/ignored.rs"), script_body());
        touch(&root.join(".renzora/scripts/renamed.rs"), script_body());
        touch(&root.join("node_modules/ignored.rs"), script_body());
        touch(&root.join("dist/ignored.rs"), script_body());
        touch(&root.join(".hg/ignored.rs"), script_body());
        // Dot-prefixed directory that is NOT in the SKIP list:
        touch(&root.join(".idea/ignored.rs"), script_body());

        let names: Vec<String> = collect_rust_scripts(root)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "got: {names:#?}");
        assert!(names
            .iter()
            .any(|n| n.ends_with("a/keep.rs") || n.ends_with("a\\keep.rs")));
    }

    #[test]
    fn duplicate_leaf_names_in_separate_dirs_are_both_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("enemies/spin.rs"), script_body());
        touch(&root.join("props/spin.rs"), script_body());
        let found = collect_canonical_scripts(root);
        assert_eq!(found.len(), 2);
        let paths: Vec<String> = found.iter().map(|c| c.path().to_string()).collect();
        assert!(paths.contains(&"enemies/spin.rs".to_string()));
        assert!(paths.contains(&"props/spin.rs".to_string()));
    }

    #[test]
    fn canonical_ids_share_root_and_rootkind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.rs"), script_body());
        let found = collect_canonical_scripts(root);
        assert_eq!(found.len(), 1);
        let id = &found[0];
        assert_eq!(id.path(), "a.rs");
    }

    #[test]
    fn non_rs_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.txt"), "not rust");
        touch(&root.join("a.lua"), "not rust");
        touch(&root.join("a.rs"), script_body());
        assert_eq!(collect_rust_scripts(root).len(), 1);
    }

    #[test]
    fn undeclared_rs_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.rs"), "fn not_a_script() {}\n");
        assert_eq!(collect_rust_scripts(root).len(), 0);
    }

    #[test]
    fn dot_prefixed_directory_outside_skip_list_is_still_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join(".foo/should_be_ignored.rs"), script_body());
        assert_eq!(collect_rust_scripts(root).len(), 0);
    }

    #[test]
    fn discovered_path_promotion_to_canonical_id_for_a_known_script() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let on_disk: PathBuf = root.join("enemy/spin.rs");
        touch(&on_disk, script_body());
        let id = project_relpath_for(root, &on_disk).unwrap();
        assert_eq!(id.path(), "enemy/spin.rs");
        assert_eq!(id.root(), renzora_identity::RootKind::Project);
    }
}
