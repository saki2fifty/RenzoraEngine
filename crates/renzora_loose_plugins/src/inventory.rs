//! Authoritative inventory of every loose plugin the editor has seen.
//!
//! This is the resource every other layer reads from and writes to: the
//! watcher updates it on filesystem events, the build-completion drain
//! advances it after a `BuildOutcome` lands, the activation system reads it
//! to decide what to load, and the Settings UI / `DisabledPlugins` writes
//! to it on user action. It is deliberately one resource per process and
//! it is the only state that matters — there are no private mirrors.

use bevy::prelude::*;
use renzora_identity::CanonicalId;
use std::collections::{BTreeMap, BTreeSet};

/// What the editor thinks the status of one loose plugin is right now.
///
/// Eleven states, matching the design: discovered, compiling, active,
/// superseded, compile failed, load failed, ABI rejected, layout change
/// requires restart, wrong scope, disabled, source removed while mapped.
/// Plus two Phase 3 additions: `AwaitingTrustConsent` (no BuildService
/// submission without explicit consent) and `MalformedContract` (the file
/// was rejected by the parser and will not be compiled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoosePluginStatusKind {
    Discovered,
    AwaitingTrustConsent,
    Compiling,
    Active,
    Superseded,
    CompileFailed,
    LoadFailed,
    AbiRejected,
    LayoutChangeRequiresRestart,
    WrongScope,
    MalformedContract,
    Disabled,
    SourceRemoved,
}

impl LoosePluginStatusKind {
    pub fn as_label(self) -> &'static str {
        match self {
            LoosePluginStatusKind::Discovered => "discovered",
            LoosePluginStatusKind::AwaitingTrustConsent => "awaiting trust consent",
            LoosePluginStatusKind::Compiling => "compiling",
            LoosePluginStatusKind::Active => "active",
            LoosePluginStatusKind::Superseded => "superseded",
            LoosePluginStatusKind::CompileFailed => "compile failed",
            LoosePluginStatusKind::LoadFailed => "load failed",
            LoosePluginStatusKind::AbiRejected => "ABI rejected",
            LoosePluginStatusKind::LayoutChangeRequiresRestart => {
                "layout change (restart required)"
            }
            LoosePluginStatusKind::WrongScope => "wrong scope",
            LoosePluginStatusKind::MalformedContract => "malformed contract",
            LoosePluginStatusKind::Disabled => "disabled",
            LoosePluginStatusKind::SourceRemoved => "source removed",
        }
    }
}

impl std::fmt::Display for LoosePluginStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_label())
    }
}

impl LoosePluginStatusKind {
    /// True if this state is "the plugin is running right now". Only `Active`
    /// counts.
    pub fn is_active(self) -> bool {
        matches!(self, LoosePluginStatusKind::Active)
    }

    /// True if the state is terminal for the current attempt: no further
    /// work to do until the user changes something or the source file
    /// changes.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            LoosePluginStatusKind::Active
                | LoosePluginStatusKind::CompileFailed
                | LoosePluginStatusKind::LoadFailed
                | LoosePluginStatusKind::AbiRejected
                | LoosePluginStatusKind::LayoutChangeRequiresRestart
                | LoosePluginStatusKind::WrongScope
                | LoosePluginStatusKind::MalformedContract
                | LoosePluginStatusKind::Disabled
                | LoosePluginStatusKind::SourceRemoved
        )
    }
}

/// One loose plugin's state, indexed by canonical identity.
///
/// The row carries everything the Settings UI / activation system needs:
/// the scope the author declared, the source path, the last outcome
/// (generation number when `kind == Active`, last error message when
/// `kind` is a failure), and the build diagnostics.
#[derive(Debug, Clone)]
pub struct LoosePluginStatus {
    pub scope: crate::contract::LoosePluginScope,
    pub kind: LoosePluginStatusKind,
    /// Absolute path on disk to the source file. `None` only between a
    /// `SourceRemoved` transition and a re-discovery.
    pub source_path: Option<std::path::PathBuf>,
    /// Active generation. `Some(n)` only when `kind == Active`.
    pub active_generation: Option<u32>,
    /// Last build's diagnostics (empty unless `kind` is a build/load
    /// failure).
    pub diagnostics: Vec<String>,
    /// Stable staged library path — populated whenever the integration has
    /// successfully staged at least one generation for this identity. Used
    /// as the input to `load_one` (after Windows shadow-copy).
    pub stable_staged_path: Option<std::path::PathBuf>,
}

impl Default for LoosePluginStatus {
    fn default() -> Self {
        Self {
            scope: crate::contract::LoosePluginScope::Runtime,
            kind: LoosePluginStatusKind::Discovered,
            source_path: None,
            active_generation: None,
            diagnostics: Vec::new(),
            stable_staged_path: None,
        }
    }
}

/// Per-identity alias.
pub type LoosePluginRow = LoosePluginStatus;

/// The persisted + live trust gate.
///
/// Phase 3 does not sandbox loose plugins: a loose plugin's compiled cdylib
/// has the editor process's permissions. The host therefore refuses to
/// submit a build for any identity the user has not consented to, and the
/// inventory reflects that as `LoosePluginStatusKind::AwaitingTrustConsent`.
///
/// Persistence is delegated to the existing settings store; the resource
/// is the editable mirror that drain systems consult every frame.
#[derive(Resource, Default, Debug, Clone)]
pub struct LoosePluginTrust {
    consented: BTreeSet<CanonicalId>,
}

impl LoosePluginTrust {
    pub fn has_consent(&self, id: &CanonicalId) -> bool {
        self.consented.contains(id)
    }

    /// Look up consent by `Display`-form string (`engine://spin.rs`).
    /// The Settings UI stores canonical ids as opaque strings; this is
    /// the bridge that converts them back into the parsed form.
    pub fn has_consent_by_str(&self, id: &str) -> bool {
        match CanonicalId::parse(id) {
            Ok(parsed) => self.consented.contains(&parsed),
            Err(_) => false,
        }
    }

    pub fn grant(&mut self, id: CanonicalId) -> bool {
        self.consented.insert(id)
    }

    pub fn revoke(&mut self, id: &CanonicalId) -> bool {
        self.consented.remove(id)
    }

    pub fn consented(&self) -> impl Iterator<Item = &CanonicalId> {
        self.consented.iter()
    }
}

/// Parse a `CanonicalId` from its `Display` form (`engine://spin.rs`).
/// The Settings UI presents canonical IDs in this form to the user;
/// the loose host also keys its consent set on the parsed form. This
/// helper bridges the two representations.
pub fn parse_canonical_id(input: &str) -> Option<CanonicalId> {
    CanonicalId::parse(input).ok()
}

/// Per-attempt marker that the inventory can use to decide whether a
/// transition is allowed. (Currently unused outside tests; kept here so the
/// inventory's `transition` rules and the trust gate can both reference the
/// same vocabulary.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoosePluginConsent {
    Granted,
    Revoked,
}

/// The authoritative loose-plugin inventory.
///
/// One Bevy `Resource` per process; every layer reads from it and writes to
/// it through the methods on this type. The watcher, the build drain, the
/// transaction system and the Settings UI all go through `transition` and
/// `set_enabled`, which are the only ways to mutate the underlying state.
#[derive(Resource, Default, Debug)]
pub struct LoosePluginInventory {
    rows: BTreeMap<CanonicalId, LoosePluginRow>,
    /// Ids the user disabled. Mirrors `renzora::DisabledPlugins` for loose
    /// files; the two lists are kept in sync at the host integration
    /// boundary.
    disabled: BTreeSet<CanonicalId>,
    /// Ids the user has consented to compile. Mirrors the persisted
    /// `LoosePluginTrust` resource; both are kept in sync by the same
    /// boundary.
    consented: BTreeSet<CanonicalId>,
}

impl LoosePluginInventory {
    /// Idempotent insert-or-update on a freshly discovered file. Returns
    /// the resulting row.
    pub fn upsert_discovered(
        &mut self,
        id: CanonicalId,
        scope: crate::contract::LoosePluginScope,
        source_path: std::path::PathBuf,
        consented: bool,
        disabled: bool,
    ) -> &mut LoosePluginRow {
        let trust = if consented {
            LoosePluginStatusKind::Discovered
        } else {
            LoosePluginStatusKind::AwaitingTrustConsent
        };
        let entry = self
            .rows
            .entry(id.clone())
            .or_insert_with(|| LoosePluginRow {
                scope,
                kind: trust,
                source_path: Some(source_path.clone()),
                ..Default::default()
            });
        // Refresh the source path and re-derive the trust state on every
        // re-discovery; a user who deleted a file and re-saved it expects
        // the trust gate to fire again unless they have already consented.
        entry.source_path = Some(source_path);
        if !consented {
            entry.kind = LoosePluginStatusKind::AwaitingTrustConsent;
        } else if matches!(entry.kind, LoosePluginStatusKind::AwaitingTrustConsent) {
            // Consent granted externally; promote back to Discovered so a
            // subsequent watcher event will resubmit.
            entry.kind = LoosePluginStatusKind::Discovered;
        }
        if disabled {
            self.disabled.insert(id.clone());
            entry.kind = LoosePluginStatusKind::Disabled;
        }
        entry
    }

    /// Apply a state transition. The `transition` rules are deliberately
    /// permissive — the host's drain system is the only writer and has
    /// already done its own bookkeeping before calling this. A future audit
    /// could tighten the transition table; the current design relies on the
    /// caller.
    pub fn transition(&mut self, id: &CanonicalId, kind: LoosePluginStatusKind) {
        if let Some(row) = self.rows.get_mut(id) {
            row.kind = kind;
        }
    }

    pub fn record_diagnostics(&mut self, id: &CanonicalId, diagnostics: Vec<String>) {
        if let Some(row) = self.rows.get_mut(id) {
            row.diagnostics = diagnostics;
        }
    }

    pub fn set_active_generation(
        &mut self,
        id: &CanonicalId,
        generation: Option<u32>,
        stable_staged_path: Option<std::path::PathBuf>,
    ) {
        if let Some(row) = self.rows.get_mut(id) {
            row.active_generation = generation;
            row.stable_staged_path = stable_staged_path;
        }
    }

    pub fn mark_source_removed(&mut self, id: &CanonicalId) {
        if let Some(row) = self.rows.get_mut(id) {
            // SourceRemoved preserves the active_generation so the last-good
            // row remains queryable for export and Settings display.
            row.kind = LoosePluginStatusKind::SourceRemoved;
            row.source_path = None;
        }
    }

    pub fn set_enabled(&mut self, id: &CanonicalId, enabled: bool) {
        if enabled {
            self.disabled.remove(id);
            if let Some(row) = self.rows.get_mut(id) {
                if matches!(row.kind, LoosePluginStatusKind::Disabled) {
                    row.kind = LoosePluginStatusKind::Discovered;
                }
            }
        } else {
            self.disabled.insert(id.clone());
            if let Some(row) = self.rows.get_mut(id) {
                row.kind = LoosePluginStatusKind::Disabled;
            }
        }
    }

    pub fn is_disabled(&self, id: &CanonicalId) -> bool {
        self.disabled.contains(id)
    }

    pub fn has_consent(&self, id: &CanonicalId) -> bool {
        self.consented.contains(id)
    }

    pub fn grant_consent(&mut self, id: CanonicalId) {
        self.consented.insert(id.clone());
        if let Some(row) = self.rows.get_mut(&id) {
            if matches!(row.kind, LoosePluginStatusKind::AwaitingTrustConsent) {
                row.kind = LoosePluginStatusKind::Discovered;
            }
        }
    }

    pub fn revoke_consent(&mut self, id: &CanonicalId) {
        self.consented.remove(id);
    }

    pub fn row(&self, id: &CanonicalId) -> Option<&LoosePluginRow> {
        self.rows.get(id)
    }

    pub fn row_mut(&mut self, id: &CanonicalId) -> Option<&mut LoosePluginRow> {
        self.rows.get_mut(id)
    }

    pub fn rows(&self) -> impl Iterator<Item = (&CanonicalId, &LoosePluginRow)> {
        self.rows.iter()
    }

    /// All identities whose row indicates `Active`, whose declared scope
    /// is `Runtime`, and which have a stable staged path available.
    /// Returns `(canonical_id, staged_path)` so the export pipeline can
    /// both identify the plugin by its full canonical identity (the
    /// durable key) and locate the artifact to copy.
    pub fn export_candidates(&self) -> Vec<(CanonicalId, std::path::PathBuf)> {
        self.rows
            .iter()
            .filter(|(_, r)| {
                r.kind.is_active() && matches!(r.scope, crate::contract::LoosePluginScope::Runtime)
            })
            .filter_map(|(id, r)| r.stable_staged_path.clone().map(|p| (id.clone(), p)))
            .collect()
    }

    /// All canonical ids currently tracked.
    pub fn ids(&self) -> impl Iterator<Item = &CanonicalId> {
        self.rows.keys()
    }

    /// Iterate (id, row) for every row currently recorded. Used by the
    /// build-drain system to find which `id`/`revision` belongs to which
    /// pending receiver.
    pub fn iter(&self) -> impl Iterator<Item = (&CanonicalId, &LoosePluginRow)> {
        self.rows.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn id(s: &str) -> CanonicalId {
        CanonicalId::parse(s).expect("valid id")
    }

    #[test]
    fn upsert_discovered_requires_consent_for_unconsented() {
        let mut inv = LoosePluginInventory::default();
        let i = id("engine://spin.rs");
        inv.upsert_discovered(
            i.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/x.rs".into(),
            false,
            false,
        );
        assert_eq!(
            inv.row(&i).unwrap().kind,
            LoosePluginStatusKind::AwaitingTrustConsent
        );
    }

    #[test]
    fn upsert_discovered_with_consent_marks_discovered() {
        let mut inv = LoosePluginInventory::default();
        let i = id("engine://spin.rs");
        inv.upsert_discovered(
            i.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/x.rs".into(),
            true,
            false,
        );
        assert_eq!(inv.row(&i).unwrap().kind, LoosePluginStatusKind::Discovered);
    }

    #[test]
    fn set_enabled_round_trip() {
        let mut inv = LoosePluginInventory::default();
        let i = id("engine://spin.rs");
        inv.upsert_discovered(
            i.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/x.rs".into(),
            true,
            false,
        );
        inv.set_enabled(&i, false);
        assert!(inv.is_disabled(&i));
        assert_eq!(inv.row(&i).unwrap().kind, LoosePluginStatusKind::Disabled);
        inv.set_enabled(&i, true);
        assert!(!inv.is_disabled(&i));
        assert_eq!(inv.row(&i).unwrap().kind, LoosePluginStatusKind::Discovered);
    }

    #[test]
    fn source_removed_preserves_active_generation() {
        let mut inv = LoosePluginInventory::default();
        let i = id("engine://spin.rs");
        inv.upsert_discovered(
            i.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/x.rs".into(),
            true,
            false,
        );
        inv.transition(&i, LoosePluginStatusKind::Active);
        inv.set_active_generation(&i, Some(7), None);
        inv.mark_source_removed(&i);
        assert_eq!(
            inv.row(&i).unwrap().kind,
            LoosePluginStatusKind::SourceRemoved
        );
        assert_eq!(inv.row(&i).unwrap().active_generation, Some(7));
    }

    #[test]
    fn grant_consent_promotes_awaiting_to_discovered() {
        let mut inv = LoosePluginInventory::default();
        let i = id("engine://spin.rs");
        inv.upsert_discovered(
            i.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/x.rs".into(),
            false,
            false,
        );
        assert_eq!(
            inv.row(&i).unwrap().kind,
            LoosePluginStatusKind::AwaitingTrustConsent
        );
        inv.grant_consent(i.clone());
        assert_eq!(inv.row(&i).unwrap().kind, LoosePluginStatusKind::Discovered);
    }

    #[test]
    fn export_candidates_only_active_runtime() {
        let mut inv = LoosePluginInventory::default();
        let a = id("engine://a.rs");
        let b = id("engine://b.rs");
        let c = id("engine://c.rs");
        inv.upsert_discovered(
            a.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/a.rs".into(),
            true,
            false,
        );
        inv.upsert_discovered(
            b.clone(),
            crate::contract::LoosePluginScope::Editor,
            "/b.rs".into(),
            true,
            false,
        );
        inv.upsert_discovered(
            c.clone(),
            crate::contract::LoosePluginScope::Runtime,
            "/c.rs".into(),
            true,
            false,
        );
        inv.transition(&a, LoosePluginStatusKind::Active);
        inv.transition(&b, LoosePluginStatusKind::Active);
        inv.transition(&c, LoosePluginStatusKind::CompileFailed);
        // `export_candidates` requires a non-empty stable_staged_path.
        inv.set_active_generation(&a, Some(1), Some(PathBuf::from("/a.staged")));
        let out = inv.export_candidates();
        assert_eq!(out.len(), 1);
        assert!(out.contains(&(a.clone(), PathBuf::from("/a.staged"))));
    }
}
