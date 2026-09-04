//! Neutral owner of the editor's single shared `Arc<BuildService>`.
//!
//! Phase 4 has two consumers of `BuildService`:
//!
//! - `renzora_loose_plugins` (Phase 3) submits `ArtifactKind::Tier1Plugin`
//!   requests.
//! - `renzora_rust_script` (Phase 4) submits `ArtifactKind::Tier1Script`
//!   requests.
//!
//! Both consumers MUST share one `BuildService` instance, so the
//! cache root, the worker pool, the supervisor, and the staging
//! directory are exactly the same. Two disjoint services would
//! duplicate cache partitions and miss the cross-artifact supersession
//! the editor needs (a script save while a plugin is recompiling
//! should still resolve through the same `BuildOutcome::Superseded`
//! path).
//!
//! `SharedBuildService` is the Bevy `Resource` the editor installs
//! once at startup. Both consumer crates install a thin local handle
//! (`LooseBuildService`, `RustScriptBuildService`) that wraps the same
//! `Arc<BuildService>`. `Arc::ptr_eq` between the two local handles
//! is what the acceptance suite asserts to prove "one service, two
//! callers".
//!
//! ## S4-2: artifact-specific required symbols
//!
//! `SharedBuildServiceConfig::required_symbols_by_kind` is keyed by
//! [`crate::types::ArtifactKind`]: a Tier-1 plugin's loader requires
//! the loose-plugin init symbol; a Tier-1 script's loader requires
//! the script descriptor symbol. The previous combined-list policy
//! required every configured symbol for every loaded image, which
//! was wrong — a normal loose plugin does not export the script
//! descriptor and a normal script does not export the loose-plugin
//! initializer. The editor populates both keys so the same service
//! compiles both artifact kinds, but the loader verifies only the
//! symbols that the loaded image's kind requires.

use std::collections::HashMap;
use std::sync::Arc;

use crate::service::BuildService;
use crate::types::ArtifactKind;

/// The single shared `Arc<BuildService>` the editor owns. One per
/// process; the constructor hands back an `Arc` so consumers that
/// run outside Bevy (Phase 3/4 test harnesses) can share the same
/// instance with the editor's installed resource.
#[derive(Clone)]
pub struct SharedBuildService {
    inner: Arc<BuildService>,
}

impl SharedBuildService {
    /// Wrap an existing `Arc<BuildService>`. The editor's host crate
    /// is expected to construct the `BuildService` from a
    /// [`BuildServiceConfig`] and hand the `Arc` over so loose
    /// plugins and scripts submit to the same instance.
    pub fn new(inner: Arc<BuildService>) -> Self {
        Self { inner }
    }

    /// Borrow the underlying `Arc`. Useful for tests that want to
    /// compare with another handle via `Arc::ptr_eq`.
    pub fn as_arc(&self) -> Arc<BuildService> {
        self.inner.clone()
    }

    /// Submit a build request. Thin pass-through; the indirection
    /// exists so consumers do not depend on the crate-private
    /// `BuildService::submit` signature.
    pub fn submit(
        &self,
        req: crate::types::BuildRequest,
    ) -> Result<
        crossbeam_channel::Receiver<crate::types::BuildOutcome>,
        crate::service::BuildServiceError,
    > {
        self.inner.submit(req)
    }

    /// Recover the canonical stamped inputs the BuildService captured
    /// at construction. Tests use these to populate
    /// [`crate::types::FingerprintInputs`].
    pub fn stamps(&self) -> crate::compiler::ServiceStamps {
        self.inner.stamps()
    }
}

impl std::ops::Deref for SharedBuildService {
    type Target = BuildService;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::fmt::Debug for SharedBuildService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBuildService")
            .field("strong_count", &Arc::strong_count(&self.inner))
            .finish()
    }
}

/// Configuration for the editor's single shared `BuildService`.
/// Both loose plugins and Rust scripts submit to this instance; the
/// cache root, worker pool, and SDK stamp are unified. Required
/// symbols are keyed by [`ArtifactKind`] so each artifact kind's
/// loader verifies only its own kind's symbol surface.
#[derive(Clone, Debug)]
pub struct SharedBuildServiceConfig {
    pub cache_root: std::path::PathBuf,
    pub sdk_path: std::path::PathBuf,
    pub profile: crate::types::BuildProfile,
    pub compiler_service_schema: u32,
    pub toolchain_stamp: String,
    pub n_workers: Option<usize>,
    pub n_children: Option<usize>,
    pub shutdown_deadline: std::time::Duration,
    /// Required exported symbols keyed by artifact kind. The editor
    /// populates both the Tier-1 plugin and the Tier-1 script keys
    /// so a single `BuildService` compiles both kinds; the loader
    /// verifies only the kind the loaded image claims to be.
    pub required_symbols_by_kind: HashMap<ArtifactKind, Vec<Vec<u8>>>,
}

impl Default for SharedBuildServiceConfig {
    fn default() -> Self {
        let mut required_symbols_by_kind: HashMap<ArtifactKind, Vec<Vec<u8>>> =
            HashMap::new();
        // Loose-plugin cdylibs need their init symbol; scripts do not.
        required_symbols_by_kind.insert(
            ArtifactKind::Tier1Plugin,
            vec![b"renzora_plugin_init\0".to_vec()],
        );
        // Script cdylibs need their descriptor symbol; plugins do not.
        required_symbols_by_kind.insert(
            ArtifactKind::Tier1Script,
            vec![b"renzora_plugin_tier1_script_desc\0".to_vec()],
        );
        Self {
            cache_root: std::path::PathBuf::from(".compiler-cache"),
            sdk_path: std::path::PathBuf::from("sdk"),
            profile: crate::types::BuildProfile::Dist,
            compiler_service_schema: crate::types::COMPILER_SERVICE_SCHEMA,
            toolchain_stamp: String::new(),
            n_workers: Some(1),
            n_children: Some(1),
            shutdown_deadline: std::time::Duration::from_secs(5),
            required_symbols_by_kind,
        }
    }
}

impl SharedBuildServiceConfig {
    /// Build the underlying [`BuildServiceConfig`] from this shared
    /// configuration.
    pub fn to_build_service_config(&self) -> crate::service::BuildServiceConfig {
        crate::service::BuildServiceConfig {
            cache_root: self.cache_root.clone(),
            profile: self.profile,
            sdk_path: self.sdk_path.clone(),
            toolchain_stamp: self.toolchain_stamp.clone(),
            compiler_service_schema: self.compiler_service_schema,
            n_workers: self.n_workers,
            n_children: self.n_children,
            shutdown_deadline: self.shutdown_deadline,
            required_symbols_by_kind: self.required_symbols_by_kind.clone(),
        }
    }

    /// Add (or extend) the required-symbol list for one artifact kind.
    /// Convenience for tests that need to construct a service with a
    /// narrower or wider required-symbol set than the editor default.
    pub fn with_required_symbol_for(
        mut self,
        kind: ArtifactKind,
        symbol: Vec<u8>,
    ) -> Self {
        self.required_symbols_by_kind
            .entry(kind)
            .or_default()
            .push(symbol);
        self
    }

    /// Construct a config with NO required symbols for any kind.
    /// Useful for tests that exercise the loader symbol verification
    /// path in isolation.
    pub fn without_required_symbols() -> Self {
        let mut cfg = Self::default();
        cfg.required_symbols_by_kind.clear();
        cfg
    }
}

/// Construct one shared `BuildService`. The editor calls this once
/// and hands the same `Arc` to `LooseBuildService` and
/// `RustScriptBuildService`. Returns the error if the cache root or
/// SDK stamp cannot be initialised; the editor surfaces that as a
/// startup diagnostic rather than panicking or silently disabling
/// Rust scripts. S4-3 turns recoverable startup failures into
/// diagnostics at the editor entry points.
pub fn build_shared_service(
    config: SharedBuildServiceConfig,
) -> Result<Arc<BuildService>, crate::service::BuildServiceError> {
    BuildService::new(config.to_build_service_config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::BuildServiceConfig;

    fn build_test_service() -> Arc<BuildService> {
        let dir = tempfile::tempdir().unwrap();
        let mut required = HashMap::new();
        required.insert(ArtifactKind::Tier1Plugin, vec![b"renzora_plugin_init\0".to_vec()]);
        let cfg = BuildServiceConfig {
            cache_root: dir.path().to_path_buf(),
            profile: crate::types::BuildProfile::Dist,
            sdk_path: dir.path().join("sdk"),
            toolchain_stamp: "test".into(),
            compiler_service_schema: crate::types::COMPILER_SERVICE_SCHEMA,
            n_workers: Some(1),
            n_children: Some(1),
            shutdown_deadline: std::time::Duration::from_secs(5),
            required_symbols_by_kind: required,
        };
        BuildService::new(cfg).expect("BuildService::new")
    }

    #[test]
    fn shared_handle_exposes_the_same_arc() {
        let svc = build_test_service();
        let a = SharedBuildService::new(svc.clone());
        let b = SharedBuildService::new(a.as_arc());
        assert!(Arc::ptr_eq(&a.as_arc(), &b.as_arc()));
    }

    #[test]
    fn shared_config_default_has_both_artifact_kinds() {
        let cfg = SharedBuildServiceConfig::default();
        assert!(
            cfg.required_symbols_by_kind
                .contains_key(&ArtifactKind::Tier1Plugin),
            "Tier1Plugin required-symbol key must be populated"
        );
        assert!(
            cfg.required_symbols_by_kind
                .contains_key(&ArtifactKind::Tier1Script),
            "Tier1Script required-symbol key must be populated"
        );
        // The two kinds carry DIFFERENT symbol surfaces; the
        // previous combined-list policy was wrong because it
        // demanded both kinds from every loaded image.
        assert_ne!(
            cfg.required_symbols_by_kind[&ArtifactKind::Tier1Plugin],
            cfg.required_symbols_by_kind[&ArtifactKind::Tier1Script],
        );
    }
}
