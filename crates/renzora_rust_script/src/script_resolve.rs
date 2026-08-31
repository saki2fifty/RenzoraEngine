//! Path-to-canonical-id resolution helpers used by the script dispatch.
//!
//! Phase 1 commit 1.3 added this module so the dispatch and the
//! exporter could share one identity-resolution function. The Bevy
//! resource `LoadedScripts` lives in `lib.rs` (the source of truth
//! for the Bevy resource); this module is intentionally storage-free.

use std::path::{Path, PathBuf};

use renzora_identity::{AliasLookup, BareAliasIndex, CanonicalId, RootKind};

/// What dispatch turned the user's `script_path` into.
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

/// Resolve a `ScriptComponent::script_path` to a single canonical id, an
/// ambiguity list, or a `NotFound` outcome.
///
/// Resolution order:
/// 1. If the path lives under `project_root`, use its project-relative
///    forward-slash form as a canonical identity.
/// 2. Else if the path has no separators (i.e. a bare leaf), look up
///    the alias index. A unique match resolves; multiple matches
///    produce `Ambiguous`; no match produces `NotFound`.
/// 3. Else (path with separators but not under the root), try it
///    directly as a project-relative canonical identity.
///
/// Bare leaves are NEVER silently promoted to canonical ids. A bare
/// `spin.rs` does not become `project://spin.rs` if no entry in the
/// alias index matches.
pub fn resolve_script_identity(
    script_path: &Path,
    project_root: &Path,
    alias_index: &BareAliasIndex,
) -> ResolvedScript {
    if let Ok(rel) = script_path.strip_prefix(project_root) {
        if let Some(id) = canonical_for_path(rel) {
            return ResolvedScript::Unique(id);
        }
    }
    let path_text = script_path.to_string_lossy();
    let has_separator = path_text.contains('/') || path_text.contains('\\');
    if !has_separator {
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
/// canonical identities always pick the same build dir; distinct
/// identities never collide. Used by `build_to_path_with_id` in
/// `lib.rs` to keep generated output keyed to the identity that
/// produced it. See correction 14 for the contract on this hash.
pub fn build_dir_name(id: &CanonicalId) -> String {
    use std::hash::Hasher;
    let s = id.to_scheme_path();
    let mut h = std::collections::hash_map::DefaultHasher::default();
    h.write(s.as_bytes());
    let digest = h.finish();
    format!("{:016x}", digest)
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
        let a = tmp.path().join("enemy/spin.rs");
        let b = tmp.path().join("props/spin.rs");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let mut idx = BareAliasIndex::new();
        idx.insert(CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap());
        idx.insert(CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap());
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
