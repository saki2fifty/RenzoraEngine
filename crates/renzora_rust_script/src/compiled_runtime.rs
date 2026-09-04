//! Runtime adapter for compiled Tier 1 Rust scripts.
//!
//! Phase 4 holds two related registries:
//!
//! 1. The host's *process-global* [`CompiledScriptBackend`] (in
//!    `renzora_plugin::script::compiled`) maps canonical identity
//!    to an `Arc<ScriptGeneration>` carrying both the per-cdylib
//!    `ScriptEntry` and the `Library` handle that keeps the cdylib
//!    mapped. An in-flight dispatch clones the `Arc` and calls into
//!    the entry through it; replacement is atomic, and the previous
//!    generation becomes releasable as soon as no in-flight call
//!    holds it.
//! 2. The Bevy [`CompiledScriptSlot`] resource wraps the same
//!    `CompiledScriptBackend` for resource-style access from Bevy
//!    systems.

use std::path::Path;
use std::sync::Arc;

use bevy::prelude::Resource;
use renzora_identity::{BareAliasIndex, CanonicalId, RootKind};
use renzora_plugin::script::compiled::{CompiledScriptBackend, ScriptGeneration};

/// A Bevy resource wrapping the process-global
/// [`CompiledScriptBackend`] slot. In normal operation the slot is
/// shared by every subsystem; the Bevy resource exists so the slot
/// can be discovered through normal resource inspection (and so
/// tests can substitute a private slot to keep registrations
/// isolated).
#[derive(Resource, Clone)]
pub struct CompiledScriptSlot(pub Arc<CompiledScriptBackend>);

impl Default for CompiledScriptSlot {
    fn default() -> Self {
        Self(Arc::new(CompiledScriptBackend::new()))
    }
}

impl CompiledScriptSlot {
    /// Register an active generation against a canonical identity.
    pub fn register(&self, id: &str, generation: Arc<ScriptGeneration>) {
        self.0.register(id, generation);
    }

    /// Remove an identity; returns the previous generation, if any.
    /// An in-flight dispatch that already cloned its `Arc` still
    /// finishes safely.
    pub fn unregister(&self, id: &str) -> Option<Arc<ScriptGeneration>> {
        self.0.unregister(id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn ids(&self) -> Vec<String> {
        self.0.ids()
    }

    pub fn lookup(&self, id: &str) -> Option<Arc<ScriptGeneration>> {
        self.0.lookup(id)
    }

    /// Resolve the canonical identity for a script path string the
    /// host handed to the dispatcher in `ScriptCall::path`.
    ///
    /// The dispatcher's caller controls the resolution rule through
    /// a [`PathResolver`] resource. Production callers wire the
    /// resolver with the active project's root; tests wire a
    /// resolver pointing at their tempdir. Without a resolver, the
    /// dispatcher returns `UnknownOp` for any relative path.
    pub fn resolve_canonical(&self, path_str: &str) -> Option<String> {
        resolve_canonical_id_for(path_str, None)
    }

    /// Like [`resolve_canonical`](Self::resolve_canonical) but
    /// consults the supplied resolver first. Used by the dispatcher
    /// when a [`PathResolver`] resource is installed.
    pub fn resolve_canonical_with(
        &self,
        path_str: &str,
        resolver: Option<&PathResolver>,
    ) -> Option<String> {
        resolve_canonical_id_for(path_str, resolver)
    }
}

/// The dispatcher's view of the active project's root.
///
/// Production callers construct one of these in the editor's startup
/// path from `renzora::CurrentProject::path`. The resolver canonicalises
/// absolute paths the dispatcher receives against the project root
/// and rejects paths outside it.
#[derive(Resource, Clone, Debug)]
pub struct PathResolver {
    project_root: Arc<PathBuf>,
    /// Public so tests can introspect the alias index. The
    /// resolver refreshes it after every activate pass; tests use
    /// it to assert that bare lookups resolve correctly.
    pub alias_index: Arc<BareAliasIndex>,
}

impl PathResolver {
    /// Build a resolver pointing at the given project root. The
    /// alias index is empty until scripts are registered.
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root: Arc::new(project_root),
            alias_index: Arc::new(BareAliasIndex::new()),
        }
    }

    /// Update the bare-alias index from the supplied canonical
    /// identities. Called by the lifecycle after each activation
    /// pass so the resolver can answer unique bare-leaf lookups
    /// without falling back to a name-based heuristic.
    pub fn refresh_alias_index(&mut self, ids: impl IntoIterator<Item = CanonicalId>) {
        let mut next = BareAliasIndex::new();
        for id in ids {
            next.insert(id);
        }
        self.alias_index = Arc::new(next);
    }

    /// The active project's root path.
    pub fn project_root(&self) -> &Path {
        self.project_root.as_ref()
    }

    /// Borrow the bare-alias index. Tests use this to assert that
    /// bare lookups resolve correctly without going through the
    /// dispatcher.
    pub fn alias_index(&self) -> &BareAliasIndex {
        &self.alias_index
    }
}

use std::path::PathBuf;

/// Resolve the canonical id for a `ScriptCall::path` string.
///
/// T4-4: the resolver uses the authoritative project root (when one
/// is available) and the Phase 1 canonicalisation API. The order of
/// resolution is fixed; bare leaf names MUST go through the
/// resolver's uniqueness-aware alias index, never through
/// `CanonicalId::from_rooted`. The previous design called
/// `from_rooted("spin.rs")` first which succeeded as
/// `project://spin.rs` and never reached the alias index.
///
/// Rules, in order:
///
/// 1. Already-canonical `project://...` strings parse directly.
/// 2. An absolute path is canonicalised relative to the supplied
///    resolver's project root and rejected if it escapes the root.
/// 3. A relative path containing a directory separator is normalised
///    and traversal-checked, then `project://<rel>` is constructed.
/// 4. A bare leaf name (no `/`) resolves ONLY through the
///    resolver's `BareAliasIndex`. Unique leaf → that id.
///    Ambiguous leaf → `None`. Missing leaf → `None`. Without a
///    resolver, bare lookups fail.
pub fn resolve_canonical_id_for(path_str: &str, resolver: Option<&PathResolver>) -> Option<String> {
    if path_str.is_empty() {
        return None;
    }
    // Rule 1: already-canonical.
    if let Ok(canonical) = CanonicalId::parse(path_str) {
        return Some(canonical.to_string());
    }
    let normalized = path_str.replace('\\', "/");

    // Rule 2: absolute path against the project root.
    if let Some(resolver) = resolver {
        if Path::new(&normalized).is_absolute() {
            let project_root = resolver.project_root();
            let canonical_root = project_root.canonicalize().ok()?;
            let abs = Path::new(&normalized).canonicalize().ok()?;
            let rel = abs.strip_prefix(&canonical_root).ok()?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            return CanonicalId::from_rooted(RootKind::Project, &rel_str)
                .ok()
                .map(|c| c.to_string());
        }
    }

    // Rule 4 (before rule 3): bare leaf names go through the alias
    // index FIRST. `from_rooted(RootKind::Project, "spin.rs")`
    // succeeds as `project://spin.rs` and would otherwise shadow
    // every ambiguity check; the resolver must reject a bare leaf
    // when two scripts share the name.
    if !normalized.contains('/') {
        let resolver = resolver?;
        return match resolver.alias_index.lookup(&normalized) {
            Ok(id) => Some(id.to_string()),
            Err(_) => None,
        };
    }

    // Rule 3: relative path with directory components. Normalise and
    // traversal-check, then build the canonical id.
    match CanonicalId::from_rooted(RootKind::Project, &normalized) {
        Ok(id) => Some(id.to_string()),
        Err(_) => None,
    }
}

/// Convenience wrapper around [`resolve_canonical_id_for`] for
/// callers holding a `&Path`. Normalises backslashes before
/// resolving.
pub fn path_to_canonical_id(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('\\', "/");
    resolve_canonical_id_for(&s, None)
}
