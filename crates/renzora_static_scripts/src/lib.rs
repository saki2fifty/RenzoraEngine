//! The project's Rust scripts, compiled into this binary.
//!
//! **This file is generated.** The version checked into the repo returns an
//! empty list; the lean exporter overwrites it (and the manifest beside it)
//! inside `target/export-src/`, the throwaway workspace copy an export compiles.
//! Editing it here changes nothing about an export.
//!
//! Phase 4 swaps the entry type for the versioned Tier 1 C-ABI: each
//! generated script module emits, through the
//! `renzora_plugin::rust_script!` macro, a per-cdylib
//! `unsafe extern "C" fn renzora_plugin_tier1_script_call` whose
//! pointer is what the descriptor's `entry` field carries. The
//! static-link variant collects those function pointers here and the
//! loader registers each one against its canonical id.

use renzora_identity::CanonicalId;
use renzora_plugin::script::ScriptEntry;

/// Every script compiled into this binary, as
/// `(canonical id, ScriptEntry)`. Keyed by canonical id rather than
/// file name. Two scripts that share a leaf name at different paths
/// have distinct canonical ids and both are registered.
///
/// Empty in the dev tree.
pub fn scripts() -> Vec<(CanonicalId, ScriptEntry)> {
    Vec::new()
}
