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

/// Current immutable editor/runtime generation schema.
pub const ENGINE_PLUGIN_GENERATION_SCHEMA: u32 = 1;

/// Build-time environment variable carrying the replacement binary's identity.
pub const ENGINE_PLUGIN_STAMP_ENV: &str = "RENZORA_ENGINE_GENERATION_STAMP";

/// Decode an identity embedded by a binary entry point.
///
/// Ordinary source/distribution builds have no replacement generation stamp.
/// Call with `option_env!("RENZORA_ENGINE_GENERATION_STAMP")` in the binary,
/// not in a shared library: changing a plugin must not rebuild every engine
/// dependency merely because this stamp changed.
pub fn decode_engine_generation_stamp(
    encoded: Option<&str>,
) -> Result<Option<EnginePluginGenerationStamp>, toml::de::Error> {
    encoded.map(toml::from_str).transpose()
}

/// Identity of the running binary, supplied before plugin assembly.
#[derive(Clone, Debug, Default, Resource)]
pub struct EnginePluginRunningGeneration(pub Option<EnginePluginGenerationStamp>);

/// Native process owners require resource teardown before the editor exits.
#[derive(Resource, Default)]
pub struct EnginePluginShutdownGuard;

/// Session consent to compile native code from one explicitly chosen project.
#[derive(Clone, Debug, Default, Resource)]
pub struct EnginePluginTrust {
    /// Exact project root approved by the user, never read from project metadata.
    pub project: Option<std::path::PathBuf>,
}

/// A requested project switch that must not reuse the current native plugin set.
#[derive(Clone, Debug, Resource)]
pub struct EnginePluginPendingProject(pub std::path::PathBuf);

/// Select the build/restart project without replacing the live editor's project.
pub fn engine_plugin_project<'a>(
    current: Option<&'a super::project_config::CurrentProject>,
    pending: Option<&'a EnginePluginPendingProject>,
) -> Option<&'a std::path::Path> {
    pending
        .map(|pending| pending.0.as_path())
        .or_else(|| current.map(|current| current.path.as_path()))
}

/// User decision from the editor's native-code warning.
#[derive(Clone, Debug, Message)]
pub struct EnginePluginTrustRequest {
    /// Project shown in the warning; stale decisions cannot approve another project.
    pub project: std::path::PathBuf,
    /// Grant or revoke native compilation for the currently open project.
    pub trusted: bool,
}

/// Unsaved-work counts reported by the subsystems that own the edited data.
#[derive(Clone, Debug, Default, Resource)]
pub struct EditorUnsavedWork(pub BTreeMap<&'static str, usize>);

/// Last-schedule restart gate, after subsystem unsaved-work reports.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, bevy::prelude::SystemSet)]
pub struct EnginePluginRestartGate;

impl EditorUnsavedWork {
    /// Update one owner's count without changing unrelated owners.
    pub fn report(&mut self, owner: &'static str, count: usize) {
        if count == 0 {
            self.0.remove(owner);
        } else {
            self.0.insert(owner, count);
        }
    }

    /// Whether all reporting subsystems are safe to leave without losing edits.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

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
    /// Project displayed when the restart was offered.
    pub project: std::path::PathBuf,
    /// Candidate generation selected by the user.
    pub generation: u64,
    /// Exact candidate identity; generation numbers alone are cache-local.
    pub stamp: EnginePluginGenerationStamp,
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
    /// No project-native code is compiled before explicit consent.
    AwaitingTrust {
        /// Revision waiting for the user's decision.
        revision: u64,
    },
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
    fn saving_one_editor_does_not_clear_other_unsaved_work() {
        let mut work = EditorUnsavedWork::default();
        for owner in [
            "documents",
            "code",
            "shader",
            "material",
            "blueprint",
            "particles",
            "theme",
        ] {
            work.report(owner, 1);
        }
        work.report("documents", 0);
        assert!(!work.is_empty());
        assert_eq!(work.0.len(), 6);
        work.report("code", 3);
        assert_eq!(work.0["code"], 3);
        for owner in [
            "code",
            "shader",
            "material",
            "blueprint",
            "particles",
            "theme",
        ] {
            work.report(owner, 0);
        }
        assert!(work.is_empty());
    }

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
