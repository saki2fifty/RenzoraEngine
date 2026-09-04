//! Re-exports of the host crate's transaction types.
//!
//! Phase 3's `renzora_loose_plugins` crate does not redefine the journal
//! shape — the host crate owns [`renzora_plugin::host::JournalEntry`] and
//! [`renzora_plugin::host::RegistrySnapshot`] so every caller agrees on
//! one definition. This module re-exports the public surface the loose
//! plugins integration actually uses, plus a thin alias for the
//! activation outcome.

pub use renzora_plugin::host::loader::{
    activate_with_transaction, ActivationFailure, TransactionalActivationOutcome,
};
pub use renzora_plugin::host::{
    apply_journal_rollback, diff_registrations, snapshot_registrations, JournalEntry,
    RegistrySnapshot, TransactionJournal,
};

/// Outcome of a transactional activation, with the loose-plugin host's
/// naming. Aliased to the host's `TransactionalActivationOutcome` so
/// existing code that imported `ActivationOutcome` from this crate keeps
/// working.
pub type ActivationOutcome = TransactionalActivationOutcome;
