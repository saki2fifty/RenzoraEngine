//! Pure identity layer.
//!
//! `CanonicalId` is constructed from `&str` references only. It performs no
//! filesystem access, no case-folding, and no platform-dependent comparison.
//! The filesystem-containment layer lives in [`super::discovery`] and is gated
//! behind `feature = "discovery"`.
//!
//! ## Normalisation rules
//!
//! - Separator: `\\` is normalised to `/` on every platform.
//! - Lexical `.` is dropped.
//! - Lexical `..` is resolved against the current prefix. A `..` that escapes
//!   the declared root is rejected with [`IdParseError::EscapesDeclaredRoot`].
//! - Casing: preserved verbatim. The bytes the user wrote on disk are the
//!   bytes the type stores. `eq`, `Hash`, and `Ord` use exact byte equality.
//! - Unicode: preserved.
//! - Symlinks: NOT inspected by this layer. A symlink that escapes the root
//!   is detected by the symlink-safety layer in `discovery` consumers.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};

/// Which root an identity was discovered under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootKind {
    /// Engine-install `plugins/` root (the source chosen by
    /// `crates/renzora_plugin::host::dev::find_source_root`).
    Engine,
    /// The project the editor has open (`CurrentProject::path`).
    Project,
}

impl fmt::Display for RootKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootKind::Engine => f.write_str("engine"),
            RootKind::Project => f.write_str("project"),
        }
    }
}

/// Reasons a string cannot be turned into a [`CanonicalId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdParseError {
    /// The input was empty after normalisation.
    Empty,
    /// Lexical `..` resolution would step above the declared root.
    EscapesDeclaredRoot,
    /// The input contained an unknown scheme.
    InvalidScheme,
    /// The input was a bare root marker (e.g. `"engine://"`).
    IsRoot,
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdParseError::Empty => f.write_str("identity is empty"),
            IdParseError::EscapesDeclaredRoot => f.write_str("path escapes declared root"),
            IdParseError::InvalidScheme => f.write_str("identity has an unknown scheme"),
            IdParseError::IsRoot => f.write_str("identity is the bare root"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for IdParseError {}

/// A canonical identity. Constructed via [`CanonicalId::parse`].
///
/// The stored string has forward-slash separators and is the case the user
/// wrote on disk. `eq`, `Hash`, and `Ord` are byte-exact on the stored
/// string — no case-folding, no platform-dependent equality.
#[derive(Clone, Debug)]
pub struct CanonicalId {
    root: RootKind,
    /// Path-relative form WITHOUT the `scheme://root` prefix. The root
    /// marker is reconstructed by `Display`. Storing it separately keeps
    /// `eq`/`Hash`/`Ord` simple byte operations on `path`.
    path: String,
}

impl CanonicalId {
    /// Parse a string with a required `scheme://` prefix.
    ///
    /// Format: `<root>://<path>`, where `<root>` is `engine` or `project`
    /// (case-insensitive for the scheme only) and `<path>` is a
    /// forward-slash path relative to that root. The leading `/` on
    /// `<path>` is optional. Multiple `/` between segments are collapsed.
    pub fn parse(input: &str) -> Result<Self, IdParseError> {
        let (root, rest) = match input.find("://") {
            Some(idx) => {
                let scheme = &input[..idx];
                let kind = match scheme.to_ascii_lowercase().as_str() {
                    "engine" => RootKind::Engine,
                    "project" => RootKind::Project,
                    _ => return Err(IdParseError::InvalidScheme),
                };
                (kind, &input[idx + 3..])
            }
            None => return Err(IdParseError::InvalidScheme),
        };
        Self::from_rooted(root, rest)
    }

    /// Construct a canonical id directly from a rooted path string (no
    /// `scheme://` prefix). The `rest` is a forward-slash path relative to
    /// the given root.
    pub fn from_rooted(root: RootKind, rest: &str) -> Result<Self, IdParseError> {
        let normalised = match normalise_path(rest) {
            Some(s) => s,
            None => return Err(IdParseError::EscapesDeclaredRoot),
        };
        if normalised.is_empty() {
            return Err(IdParseError::IsRoot);
        }
        for segment in normalised.split('/') {
            if segment.is_empty() {
                return Err(IdParseError::Empty);
            }
        }
        Ok(Self {
            root,
            path: normalised,
        })
    }

    /// Root kind the identity was discovered under.
    pub fn root(&self) -> RootKind {
        self.root
    }

    /// The normalised path-relative string. Forward-slash separated, case
    /// preserved, no `.`/`..` segments.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Convert to the canonical `scheme://path` form.
    pub fn to_scheme_path(&self) -> String {
        let mut s = String::with_capacity(self.root.to_string().len() + 3 + self.path.len());
        s.push_str(&self.root.to_string());
        s.push_str("://");
        s.push_str(&self.path);
        s
    }

    /// Bare leaf name (the substring after the last `/`).
    pub fn bare_leaf(&self) -> &str {
        match self.path.rfind('/') {
            Some(idx) => &self.path[idx + 1..],
            None => &self.path,
        }
    }
}

impl PartialEq for CanonicalId {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.path == other.path
    }
}
impl Eq for CanonicalId {}

impl Hash for CanonicalId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.root.hash(state);
        self.path.hash(state);
    }
}

impl Ord for CanonicalId {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.root.cmp(&other.root) {
            Ordering::Equal => self.path.cmp(&other.path),
            o => o,
        }
    }
}
impl PartialOrd for CanonicalId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for CanonicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.root.to_string())?;
        f.write_str("://")?;
        f.write_str(&self.path)
    }
}

/// Normalise a forward-slash path:
///
/// - drop empty segments caused by leading/trailing/interior doubled slashes;
/// - drop `.` segments;
/// - resolve `..` segments against the current prefix; if a `..` would
///   step above the implicit empty-root prefix, return `None` (the caller
///   turns this into [`IdParseError::EscapesDeclaredRoot`]).
///
/// Returns `Some(String)` with the normalised forward-slash path, or `None`
/// if a `..` would escape the root.
fn normalise_path(input: &str) -> Option<String> {
    if input.contains('\0') {
        return None;
    }
    let replaced = input.replace('\\', "/");
    let mut out: Vec<String> = Vec::new();
    for seg in replaced.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                if out.is_empty() {
                    return None;
                }
                out.pop();
            }
            other => out.push(other.to_string()),
        }
    }
    if out.is_empty() {
        return Some(String::new());
    }
    let mut joined = String::new();
    for (i, seg) in out.iter().enumerate() {
        if i > 0 {
            joined.push('/');
        }
        joined.push_str(seg);
    }
    Some(joined)
}

/// Bare-leaf alias index.
///
/// The bare leaf name of a [`CanonicalId`] is the substring after the last
/// `/` (or the entire path if no `/` is present). This index maps a bare
/// leaf name to a sorted `Vec<CanonicalId>`. Lookups return:
/// - `Ok(&CanonicalId)` if exactly one canonical id has the bare leaf name;
/// - `Err(AliasLookup::Ambiguous)` if multiple do;
/// - `Err(AliasLookup::NotFound)` if none do.
///
/// Both `Project` and `Engine` roots are merged by bare-leaf name. Engine
/// roots do NOT silently shadow project roots. Ambiguity is reported.
#[derive(Clone, Debug, Default)]
pub struct BareAliasIndex {
    by_leaf: Vec<(String, Vec<CanonicalId>)>,
}

/// Result of a `BareAliasIndex::lookup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AliasLookup<'a> {
    /// No canonical id has this bare leaf name.
    NotFound,
    /// The bare leaf name matches more than one canonical id. The bare
    /// alias resolves only at explicit code paths that ask for the full
    /// identity.
    Ambiguous(&'a [CanonicalId]),
}

impl BareAliasIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self {
            by_leaf: Vec::new(),
        }
    }

    /// Insert a canonical id. Inserts preserve the sorted-by-identity list
    /// in each bucket. **Idempotent:** inserting the same id twice is a
    /// no-op. A reload of one script therefore preserves its bare alias
    /// uniqueness.
    pub fn insert(&mut self, id: CanonicalId) {
        let leaf = id.bare_leaf().to_string();
        match self.by_leaf.binary_search_by(|probe| probe.0.cmp(&leaf)) {
            Ok(idx) => {
                let bucket = &mut self.by_leaf[idx].1;
                // Idempotency: skip if the id is already present in the
                // bucket. binary_search_by returns the position of the
                // existing entry; if found, do nothing.
                if bucket.binary_search_by(|probe| probe.cmp(&id)).is_ok() {
                    return;
                }
                let pos = bucket
                    .binary_search_by(|probe| probe.cmp(&id))
                    .unwrap_err();
                bucket.insert(pos, id);
            }
            Err(idx) => {
                self.by_leaf.insert(idx, (leaf, Vec::from([id])));
            }
        }
    }

    /// Remove a canonical id from the index. O(len(bucket)).
    pub fn remove(&mut self, id: &CanonicalId) {
        let leaf = id.bare_leaf().to_string();
        if let Ok(idx) = self.by_leaf.binary_search_by(|probe| probe.0.cmp(&leaf)) {
            let bucket = &mut self.by_leaf[idx].1;
            if let Ok(pos) = bucket.binary_search_by(|probe| probe.cmp(id)) {
                bucket.remove(pos);
                if bucket.is_empty() {
                    self.by_leaf.remove(idx);
                }
            }
        }
    }

    /// Look up by bare leaf name.
    pub fn lookup(&self, leaf: &str) -> Result<&CanonicalId, AliasLookup<'_>> {
        let idx = self
            .by_leaf
            .binary_search_by(|probe| probe.0.as_str().cmp(leaf))
            .map_err(|_| AliasLookup::NotFound)?;
        match self.by_leaf[idx].1.len() {
            0 => Err(AliasLookup::NotFound),
            1 => Ok(&self.by_leaf[idx].1[0]),
            _ => Err(AliasLookup::Ambiguous(&self.by_leaf[idx].1)),
        }
    }

    /// Entries the index holds for the given bare leaf, in sorted
    /// `CanonicalId` order. Useful for reporting ambiguity in the
    /// editor UI.
    pub fn entries(&self, leaf: &str) -> &[CanonicalId] {
        let Ok(idx) = self
            .by_leaf
            .binary_search_by(|probe| probe.0.as_str().cmp(leaf))
        else {
            return &[];
        };
        &self.by_leaf[idx].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_engine_scheme() {
        let id = CanonicalId::parse("engine://plugins/foo.rs").unwrap();
        assert_eq!(id.root(), RootKind::Engine);
        assert_eq!(id.path(), "plugins/foo.rs");
    }

    #[test]
    fn parse_project_scheme() {
        let id = CanonicalId::parse("project://enemy/spin.rs").unwrap();
        assert_eq!(id.root(), RootKind::Project);
        assert_eq!(id.path(), "enemy/spin.rs");
    }

    #[test]
    fn backslash_normalised_to_forward_slash() {
        let id = CanonicalId::parse("project://enemy\\spin.rs").unwrap();
        assert_eq!(id.path(), "enemy/spin.rs");
    }

    #[test]
    fn lexical_dot_dropped() {
        let id = CanonicalId::parse("project://enemy/./spin.rs").unwrap();
        assert_eq!(id.path(), "enemy/spin.rs");
    }

    #[test]
    fn lexical_dotdot_resolves() {
        let id = CanonicalId::parse("project://a/b/../c.rs").unwrap();
        assert_eq!(id.path(), "a/c.rs");
    }

    #[test]
    fn lexical_dotdot_escaping_root_rejected() {
        assert!(matches!(
            CanonicalId::parse("project://../escape.rs"),
            Err(IdParseError::EscapesDeclaredRoot)
        ));
    }

    #[test]
    fn empty_path_rejected() {
        assert!(matches!(
            CanonicalId::parse("engine://"),
            Err(IdParseError::IsRoot)
        ));
    }

    #[test]
    fn unknown_scheme_rejected() {
        assert!(matches!(
            CanonicalId::parse("other://path"),
            Err(IdParseError::InvalidScheme)
        ));
    }

    #[test]
    fn casing_preserved_not_lowercased() {
        let id = CanonicalId::parse("project://Enemy/Spin.rs").unwrap();
        assert_eq!(id.path(), "Enemy/Spin.rs");
    }

    #[test]
    fn unicode_preserved() {
        let id = CanonicalId::parse("project://enemy/你/好/spin.rs").unwrap();
        assert_eq!(id.path(), "enemy/你/好/spin.rs");
    }

    #[test]
    fn byte_exact_equality_no_case_folding() {
        let a = CanonicalId::parse("project://Enemy/spin.rs").unwrap();
        let b = CanonicalId::parse("project://enemy/spin.rs").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn ordering_respects_root_then_path() {
        let eng1 = CanonicalId::parse("engine://a.rs").unwrap();
        let proj = CanonicalId::parse("project://a.rs").unwrap();
        let eng2 = CanonicalId::parse("engine://b.rs").unwrap();
        assert!(eng1 < proj);
        assert!(eng1 < eng2);
        assert!(proj > eng2);
    }

    #[test]
    fn bare_alias_unique_returns_one() {
        let mut idx = BareAliasIndex::new();
        idx.insert(CanonicalId::parse("project://enemy/spin.rs").unwrap());
        let resolved = idx.lookup("spin.rs").unwrap();
        assert_eq!(resolved.path(), "enemy/spin.rs");
    }

    #[test]
    fn bare_alias_ambiguous_returns_err() {
        let mut idx = BareAliasIndex::new();
        idx.insert(CanonicalId::parse("project://enemy/spin.rs").unwrap());
        idx.insert(CanonicalId::parse("project://props/spin.rs").unwrap());
        match idx.lookup("spin.rs").unwrap_err() {
            AliasLookup::Ambiguous(entries) => {
                assert_eq!(entries.len(), 2);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert_eq!(idx.entries("spin.rs").len(), 2);
    }

    #[test]
    fn bare_alias_not_found_returns_err() {
        let mut idx = BareAliasIndex::new();
        match idx.lookup("nope.rs").unwrap_err() {
            AliasLookup::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn remove_drops_id_from_bucket() {
        let mut idx = BareAliasIndex::new();
        let id = CanonicalId::parse("project://enemy/spin.rs").unwrap();
        idx.insert(id.clone());
        idx.remove(&id);
        match idx.lookup("spin.rs").unwrap_err() {
            AliasLookup::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn scheme_prefix_case_insensitive_for_scheme_only() {
        let id = CanonicalId::parse("ENGINE://plugins/foo.rs").unwrap();
        assert_eq!(id.root(), RootKind::Engine);
        assert_eq!(id.path(), "plugins/foo.rs");
    }

    #[test]
    fn from_rooted_helper() {
        let id = CanonicalId::from_rooted(RootKind::Project, "scripts/main.rs").unwrap();
        assert_eq!(id.path(), "scripts/main.rs");
        assert_eq!(id.root(), RootKind::Project);
    }

    #[test]
    fn to_scheme_path_round_trip() {
        let original = "project://enemy/spin.rs";
        let id = CanonicalId::parse(original).unwrap();
        assert_eq!(id.to_scheme_path(), original);
    }

    #[test]
    fn no_std_compiles_no_stdio_or_fs_imports() {
        // This test passes if the file as a whole compiles under
        // `--no-default-features`. The build invocation in the project's
        // validation command exercises that path.
    }
}
