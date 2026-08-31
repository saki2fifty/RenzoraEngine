//! Canonical-identity–keyed storage for the script loader, plus resolution
//! helpers that turn a `ScriptComponent::script_path` into a `CanonicalId`
//! (full identity first, bare leaf alias as a fallback).
//!
//! Phase 1 commit 1.3 changes `LoadedScripts::entries` from `String`
//! (file name) to `renzora_identity::CanonicalId`. Migration:
//!
//! - `compile_and_load` and the watcher insert with the canonical id of the
//!   source file they compiled.
//! - The prebuilt-manifest loader writes/reads canonical id keys (and the
//!   unobvious duplicate bare leaf coexists as two canonical id keys under
//!   one library, matching the documented "keys outnumber libraries" shape).
//! - The script dispatch resolves a `script_path` (which may be absolute,
//!   project-relative, or a bare leaf) to a canonical id using this module.
//!
//! A side `BareAliasIndex` keeps dispatch fast and supports the "bare leaf
//! alias resolves only when unique" rule. Ambiguity is reported at lookup
//! and is NOT a build failure.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use libloading::Library;
use renzora_identity::{AliasLookup, BareAliasIndex, CanonicalId, RootKind};

/// Every loaded script image and its entry point. `ManuallyDrop` for the
/// same reason it always has been — see the original `LoadedScripts` doc.
///
/// Keyed by `CanonicalId` since Phase 1 commit 1.3.
#[derive(Default)]
pub struct LoadedScripts {
    pub(crate) entries: HashMap<CanonicalId, ScriptFn>,
    /// Bare-leaf alias index keyed off the same entries; rebuilt on
    /// `insert`/`remove`. Two scripts at `enemies/spin.rs` and
    /// `props/spin.rs` both register leaves `spin.rs` here; the index
    /// returns `Ambiguous` for them while the canonical id keys remain
    /// two distinct entries in `entries`.
    pub(crate) alias_index: BareAliasIndex,
    _images: Vec<std::mem::ManuallyDrop<Library>>,
}

/// The signature of a loaded script's entry point.
pub type ScriptFn = fn(&mut bevy::prelude::World, bevy::prelude::Entity);

/// What dispatch turned the user's `script_path` into. Three outcomes:
/// resolved to one canonical id, ambiguous (bare leaf matched multiple),
/// or unresolvable.
#[derive(Debug, Clone)]
pub enum ResolvedScript {
    /// Found exactly one canonical id for the script path.
    Unique(CanonicalId),
    /// Bare leaf matched multiple scripts. The dispatch skips and logs;
    /// the user must specify the full path.
    Ambiguous(Vec<CanonicalId>),
    /// Path could not be turned into a canonical id (e.g. falls outside
    /// the project root or the bare leaf is unknown). The dispatch skips
    /// and logs.
    NotFound,
}

impl LoadedScripts {
    /// Point a canonical id at a newly loaded image.
    pub fn insert(&mut self, id: CanonicalId, f: ScriptFn, lib: Library) {
        // Inserting replaces the previous function pointer, but the image
        // it lived in is kept — see `watch` for why unmapping is not an
        // option. We leak the previous `Library` here too by letting
        // `ManuallyDrop` keep its position in `_images`.
        self.alias_index.insert(id.clone());
        self.entries.insert(id, f);
        self._images.push(std::mem::ManuallyDrop::new(lib));
    }

    /// Direct lookup by canonical identity.
    pub fn lookup(&self, id: &CanonicalId) -> Option<ScriptFn> {
        self.entries.get(id).copied()
    }

    /// Drop an entry (for future commit-1.3 follow-on that retires
    /// removed scripts; unused in commit 1.3 itself).
    pub fn remove(&mut self, id: &CanonicalId) {
        self.entries.remove(id);
        self.alias_index.remove(id);
    }

    /// Look up a script by project-relpath first; if that fails, fall
    /// back to the bare-name alias index. The compromise preserves the
    /// old behaviour for projects that attached scripts by leaf name
    /// while forwarding Phase 1's identity contract.
    pub fn resolve(&self, script_path: &Path, project_root: &Path) -> ResolvedScript {
        resolve_script_identity(script_path, project_root, &self.alias_index)
    }

    /// Number of distinct script images held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// All canonical ids currently loaded, sorted.
    pub fn ids(&self) -> Vec<&CanonicalId> {
        let mut v: Vec<&CanonicalId> = self.entries.keys().collect();
        v.sort();
        v
    }
}

/// Resolve a `ScriptComponent::script_path` to a single canonical id, an
/// ambiguity list, or a `NotFound` outcome.
///
/// Rules (Phase 1 commit 1.3):
///
/// 1. If the path is absolute and lives under `project_root`, use its
///    project-relative forward-slash form as a canonical identity.
/// 2. Else if the path is relative (no root), try it directly as a
///    project-relative canonical identity.
/// Resolve a `ScriptComponent::script_path` to a single canonical id, an
/// ambiguity list, or a `NotFound` outcome.
///
/// Public so `lib::dispatch` can call it directly. Tests use it too.
pub fn resolve_script_identity(
    script_path: &Path,
    project_root: &Path,
    alias_index: &BareAliasIndex,
) -> ResolvedScript {
    // Order matters: try the strict canonical-project-relative form first
    // (path lives under the project root), then bare-leaf alias if the
    // path is just a leaf (no separators), then the looser "path as text"
    // form last. The bare-leaf check must come before the loose form or
    // `spin.rs` would always match as `project://spin.rs` before the alias
    // index got a chance.
    if let Ok(rel) = script_path.strip_prefix(project_root) {
        if let Some(id) = canonical_for_path(rel) {
            return ResolvedScript::Unique(id);
        }
    }
    let path_text = script_path.to_string_lossy();
    let has_separator = path_text.contains('/') || path_text.contains('\\');
    if !has_separator {
        // Bare-leaf alias lookup. Use the file name INCLUDING extension
        // (matches how `BareAliasIndex` keys entries — leaf = substring
        // after the last `/`, which keeps the extension). For a leaf that
        // is unknown to the alias index, the answer is NotFound — we do
        // NOT fall through to project-relative parsing, which would
        // happily accept the user's bare-leaf string as a canonical id
        // for a script that does not exist.
        let leaf = script_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        return match alias_index.lookup(&leaf) {
            Ok(id) => ResolvedScript::Unique(id.clone()),
            Err(AliasLookup::Ambiguous(ids)) => ResolvedScript::Ambiguous(ids.to_vec()),
            Err(AliasLookup::NotFound) => ResolvedScript::NotFound,
        };
    }
    let as_text = path_text.replace('\\', "/");
    if let Some(id) = canonical_for_path(Path::new(&as_text)) {
        return ResolvedScript::Unique(id);
    }
    ResolvedScript::NotFound
}

fn canonical_for_path(rel: &Path) -> Option<CanonicalId> {
    let s = rel.to_string_lossy();
    let s = s.trim_start_matches('/').to_string();
    let s = s.replace('\\', "/");
    if s.is_empty() {
        return None;
    }
    CanonicalId::from_rooted(RootKind::Project, &s).ok()
}

/// Build directory hashed off the canonical identity. Two identical
/// canonical identities always pick the same build dir; distinct identities
/// never collide. Used by `build_to_path` to keep generated output keyed
/// to the identity that produced it.
pub fn build_dir_name(id: &CanonicalId) -> String {
    // FxHash isn't a workspace dep; std SipHash is more than fast enough at
    // build time. We hash a stable serialisation of the canonical id and
    // take the first 16 hex chars, which keeps filenames compact.
    use std::hash::Hasher;
    let s = id.to_scheme_path();
    let mut h = std::collections::hash_map::DefaultHasher::default();
    h.write(s.as_bytes());
    let digest = h.finish();
    format!("{:016x}", digest)
}

/// Build path stability check (used by tests). Returns the expected
/// formatted directory name for the given id, so tests can assert that
/// two identical canonical ids produce identical directories.
#[cfg(test)]
pub fn expected_build_dir_name(id: &CanonicalId) -> String {
    build_dir_name(id)
}

/// Convenience: produce a build artifact path under the project's
/// `.renzora/scripts/<dir>/` directory using the canonical identity.
pub fn build_artifact_path(
    project_root: &Path,
    id: &CanonicalId,
    lib_ext: &str,
    suffix: &str,
) -> PathBuf {
    let dir = project_root
        .join(".renzora")
        .join("scripts")
        .join(build_dir_name(id));
    dir.join(format!("{}-{}.{}", id.bare_leaf(), suffix, lib_ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_index(ids: &[&str]) -> BareAliasIndex {
        let mut idx = BareAliasIndex::new();
        for s in ids {
            idx.insert(CanonicalId::from_rooted(RootKind::Project, s).unwrap());
        }
        idx
    }

    /// Build a temporary project root with the given files (as `enemy/spin.rs`
    /// -> `"<root>/enemy/spin.rs"`) populated with the script marker.
    fn project_with_scripts(tmp: &tempfile::TempDir, files: &[&str]) {
        let body = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";
        for rel in files {
            let path = tmp.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
    }

    #[test]
    fn full_canonical_path_under_project_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = fresh_index(&["enemy/spin.rs"]);
        let on_disk = tmp.path().join("enemy/spin.rs");
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, "").unwrap();
        match resolve_script_identity(&on_disk, tmp.path(), &idx) {
            ResolvedScript::Unique(id) => assert_eq!(id.path(), "enemy/spin.rs"),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn project_relative_path_without_prefix_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = fresh_index(&["a/b.rs"]);
        let relative = PathBuf::from("a/b.rs");
        match resolve_script_identity(&relative, tmp.path(), &idx) {
            ResolvedScript::Unique(id) => assert_eq!(id.path(), "a/b.rs"),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn bare_leaf_alias_unique_resolves_to_single_canonical_id() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = fresh_index(&["enemy/spin.rs"]);
        let bare = PathBuf::from("spin.rs");
        match resolve_script_identity(&bare, tmp.path(), &idx) {
            ResolvedScript::Unique(id) => assert_eq!(id.path(), "enemy/spin.rs"),
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn bare_leaf_alias_ambiguous_reports_two_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = fresh_index(&["enemy/spin.rs", "props/spin.rs"]);
        let bare = PathBuf::from("spin.rs");
        match resolve_script_identity(&bare, tmp.path(), &idx) {
            ResolvedScript::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                let paths: Vec<String> = ids.iter().map(|c| c.path().to_string()).collect();
                assert!(paths.contains(&"enemy/spin.rs".to_string()));
                assert!(paths.contains(&"props/spin.rs".to_string()));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_leaf_names_in_separate_dirs_both_resolve_via_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Two real files on disk with the same leaf name in two folders.
        let a = tmp.path().join("enemy/spin.rs");
        let b = tmp.path().join("props/spin.rs");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let mut idx = BareAliasIndex::new();
        idx.insert(CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap());
        idx.insert(CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap());
        // Full canonical paths always work, even when both paths share a leaf.
        match resolve_script_identity(&a, tmp.path(), &idx) {
            ResolvedScript::Unique(id) => assert_eq!(id.path(), "enemy/spin.rs"),
            other => panic!("expected Unique for a, got {other:?}"),
        }
        match resolve_script_identity(&b, tmp.path(), &idx) {
            ResolvedScript::Unique(id) => assert_eq!(id.path(), "props/spin.rs"),
            other => panic!("expected Unique for b, got {other:?}"),
        }
    }

    #[test]
    fn bare_leaf_unknown_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = fresh_index(&["enemy/spin.rs"]);
        let bare = PathBuf::from("nope.rs");
        match resolve_script_identity(&bare, tmp.path(), &idx) {
            ResolvedScript::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn identical_canonical_ids_yield_identical_build_dir_names() {
        let a = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        let b = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        assert_eq!(build_dir_name(&a), build_dir_name(&b));
    }

    #[test]
    fn distinct_canonical_ids_yield_distinct_build_dir_names() {
        let a = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        let b = CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap();
        assert_ne!(build_dir_name(&a), build_dir_name(&b));
    }
}
