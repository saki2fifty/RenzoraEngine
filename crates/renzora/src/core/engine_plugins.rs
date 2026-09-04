//! Shared contracts for restart-required engine plugins.
//!
//! Build tooling and editor UI both consume these types, so they live in the
//! contract crate rather than either implementation. The builder itself stays
//! outside this crate to keep process and filesystem policy out of the ECS API.

use std::collections::BTreeMap;

use bevy::prelude::{Message, Resource};
use serde::{Deserialize, Serialize};

/// Current on-disk engine-plugin manifest schema.
pub const ENGINE_PLUGIN_MANIFEST_SCHEMA: u32 = 1;

/// Explicit classification required in every Tier 2 manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnginePluginType {
    /// A trusted plugin compiled into a replacement editor or runtime.
    Engine,
}

/// One editor or runtime half declared by an engine plugin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnginePluginHalf {
    /// Plugin-relative directory containing the half's `Cargo.toml`.
    #[serde(rename = "crate")]
    pub crate_path: String,
}

/// Strict `plugin.toml` representation for a Tier 2 engine plugin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnginePluginManifest {
    /// Manifest format version. Only schema 1 is currently accepted.
    pub schema: u32,
    /// Explicit tier marker. A directory without `type = "engine"` is not Tier 2.
    #[serde(rename = "type")]
    pub plugin_type: EnginePluginType,
    /// Stable reverse-domain identity, such as `com.example.weather`.
    pub id: String,
    /// Runtime half, compiled into runtime artifacts and exports.
    #[serde(default)]
    pub runtime: Option<EnginePluginHalf>,
    /// Editor half, compiled only into editor artifacts.
    #[serde(default)]
    pub editor: Option<EnginePluginHalf>,
}

/// Why an engine-plugin generation build was requested.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnginePluginBuildReason {
    /// A declared source or manifest changed.
    SourceChanged,
    /// The project or plugin root was opened and needs reconciliation.
    Reconcile,
    /// The user explicitly requested another attempt.
    UserRequested,
    /// An export requires an up-to-date runtime generation.
    Export,
}

/// Requests a background Tier 2 build.
#[derive(Clone, Debug, Message)]
pub struct EnginePluginBuildRequest {
    /// Canonical plugin identity, or `None` to reconcile all declarations.
    pub plugin_id: Option<String>,
    /// Reason shown in diagnostics and retained with the build record.
    pub reason: EnginePluginBuildReason,
}

/// Requests a controlled restart into a fully published candidate generation.
#[derive(Clone, Debug, Message)]
pub struct EnginePluginRestartRequest {
    /// Candidate generation selected by the user.
    pub generation: u64,
}

/// Stable identity of a generated editor/runtime pair.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnginePluginGenerationStamp {
    /// Schema of this stamp.
    pub schema: u32,
    /// Engine release/build identity.
    pub engine_build: String,
    /// Content hash of the installed build kit.
    pub build_kit_hash: String,
    /// Compilation target triple.
    pub target: String,
    /// Cargo profile used for the generation.
    pub profile: String,
    /// Hash of the exact Rust toolchain stamp.
    pub toolchain_hash: String,
    /// Hash of the canonical lockfile bytes.
    pub lockfile_hash: String,
    /// Hash of generated integration files.
    pub integration_hash: String,
    /// Enabled engine features in canonical order.
    pub features: Vec<String>,
    /// Plugin id to normalized manifest/source hash.
    pub plugins: BTreeMap<String, String>,
}

/// Severity suitable for both the Problems panel and console logging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePluginDiagnosticLevel {
    /// Informational build progress.
    Info,
    /// Recoverable condition requiring attention.
    Warning,
    /// Build or staging failure.
    Error,
}

/// One bounded, user-facing Tier 2 diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnginePluginDiagnostic {
    /// Plugin responsible for the message, when known.
    pub plugin_id: Option<String>,
    /// Diagnostic severity.
    pub level: EnginePluginDiagnosticLevel,
    /// Plain-English summary.
    pub message: String,
    /// Authored source location, when the compiler supplied one.
    pub source: Option<String>,
}

/// Current state of the Tier 2 build pipeline.
#[derive(Clone, Debug, Default, Resource)]
pub enum EnginePluginBuildState {
    /// No build is queued or running.
    #[default]
    Idle,
    /// Source state changed and is waiting for a worker slot.
    Queued {
        /// Monotonic request revision.
        revision: u64,
    },
    /// An immutable source snapshot is compiling off the ECS schedule.
    Building {
        /// Monotonic request revision.
        revision: u64,
        /// Human-readable current step.
        step: String,
    },
    /// The attempted generation failed; the known-good generation is unchanged.
    Failed {
        /// Failed request revision.
        revision: u64,
        /// Short failure summary.
        message: String,
    },
    /// A complete candidate is available for explicit restart.
    RestartReady {
        /// Published immutable generation.
        generation: u64,
        /// Stamp validated during publication.
        stamp: EnginePluginGenerationStamp,
    },
}

/// Bounded diagnostic history owned by the ECS world.
#[derive(Clone, Debug, Resource)]
pub struct EnginePluginDiagnostics {
    /// Oldest-to-newest records.
    pub entries: Vec<EnginePluginDiagnostic>,
    /// Maximum retained records.
    pub capacity: usize,
}

impl Default for EnginePluginDiagnostics {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            capacity: 200,
        }
    }
}

impl EnginePluginDiagnostics {
    /// Append a record while keeping memory use bounded.
    pub fn push(&mut self, diagnostic: EnginePluginDiagnostic) {
        if self.capacity == 0 {
            return;
        }
        let overflow = self
            .entries
            .len()
            .saturating_add(1)
            .saturating_sub(self.capacity);
        if overflow > 0 {
            self.entries.drain(..overflow);
        }
        self.entries.push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_remain_bounded() {
        let mut diagnostics = EnginePluginDiagnostics {
            entries: Vec::new(),
            capacity: 2,
        };
        for message in ["one", "two", "three"] {
            diagnostics.push(EnginePluginDiagnostic {
                plugin_id: None,
                level: EnginePluginDiagnosticLevel::Info,
                message: message.to_string(),
                source: None,
            });
        }
        assert_eq!(diagnostics.entries.len(), 2);
        assert_eq!(diagnostics.entries[0].message, "two");
        assert_eq!(diagnostics.entries[1].message, "three");
    }
}
