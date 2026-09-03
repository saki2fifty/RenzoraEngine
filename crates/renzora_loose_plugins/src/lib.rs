//! Phase 3 Tier-1 single-file hot Rust plugins.
//!
//! See `AgentFiles/Documentation/phase3-tier1-hot-plugins-design.md` for the
//! full design. The short version: a `plugins/<name>.rs` file is discovered
//! by the editor-only `LoosePluginHost` Bevy plugin, compiled through the
//! Phase 2 cached compiler service, atomically staged at a stable
//! loader-visible path, and activated through the transactional version of
//! `renzora_plugin::host::loader::load_one`. A failed compile, load, ABI
//! check, symbol lookup, scope mismatch, init failure or layout change leaves
//! the last-good generation running — every registry mutation the candidate
//! made is restored on failure.
//!
//! Module layout:
//!
//! - `contract`: parser for the loose-file authoring contract (`renzora_plugin::add!`).
//! - `inventory`: authoritative Bevy resource tracking status, disable, trust
//!   consent, and the pending-build map keyed by `(CanonicalId, Revision)`.
//! - `staging`: atomic stable staging into a flat, loader-visible directory.
//! - `transaction`: the activation journal and the rollback machinery that
//!   walks the audited host registries and undoes every mutation a candidate
//!   made.
//! - `host_plugin`: the Bevy `Plugin` that owns `BuildService`, the watcher,
//!   the pending-build map, and the drain systems for watcher events, build
//!   completions and reload requests.

pub mod contract;
pub mod inventory;
pub mod staging;
pub mod transaction;
pub mod host_plugin;

pub use contract::{parse_loose_plugin_source, LoosePluginContract, LoosePluginScope, LoosePluginParseError};
pub use inventory::{
    LoosePluginInventory, LoosePluginRow, LoosePluginStatus, LoosePluginStatusKind,
    LoosePluginTrust, LoosePluginConsent, parse_canonical_id,
};
pub use staging::{StableStaging, StableStagingPlacement};
pub use transaction::{
    JournalEntry, RegistrySnapshot, TransactionJournal, ActivationOutcome, ActivationFailure,
    snapshot_registrations, diff_registrations, apply_journal_rollback,
    activate_with_transaction,
};
pub use host_plugin::{
    apply_loose_plugin_toggle, LoosePluginHost, LoosePendingBuilds, LoosePluginReloadRequests,
    PendingBuild,
};
