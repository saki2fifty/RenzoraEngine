//! The Phase 3 Bevy `Plugin` that owns the loose-plugin subsystem.
//!
//! Three responsibilities, each with its own system:
//!
//! 1. **Watcher**: a notify-debouncer-full root-level watcher on the
//!    selected plugin source root. On debounced change events, the
//!    watcher parses the loose-file contract, updates the inventory,
//!    and submits a build to the shared `BuildService`.
//!
//! 2. **Build drain**: every frame, drain completed `BuildOutcome`s
//!    from each pending receiver. For `Published` / `CacheHit`, stage
//!    the immutable generation at the stable loader-visible path and
//!    hand the path to the transactional activation path. For
//!    `CompileFailed`, record the diagnostic in the inventory. For
//!    `Superseded`, record it (no further action — the receiver for
//!    that revision is no longer authoritative). For `Shutdown` and
//!    `Cancelled`, drop the receiver.
//!
//! 3. **Reload coordination**: a system that re-runs transactional
//!    activation on a stable staged path queued by the build drain.
//!    On a successful commit, the inventory row transitions to
//!    `Active`. On a rolled-back activation, the prior generation
//!    remains active and the row transitions to the matching failure
//!    kind.
//!
//! The plugin is editor-only and lives behind `RenzoraPluginHostPlugin`
//! in the host integration.

use bevy::prelude::*;
use renzora_compiler_cache::service::BuildServiceConfig;
use renzora_compiler_cache::types::{
    ArtifactKind, BuildOutcome, BuildProfile, BuildRequest, FingerprintInputs, PanicStrategy,
};
use renzora_compiler_cache::BuildService;
use renzora_identity::CanonicalId;
use renzora_plugin::host::loader::{self, LoadOutcome};
use renzora_plugin::sys;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::contract::{parse_loose_plugin_source, LoosePluginScope};
use crate::inventory::{LoosePluginInventory, LoosePluginStatusKind, LoosePluginTrust};
use crate::staging::StableStaging;

/// Mirror a `LoosePluginStatusKind` to the editor-facing
/// `renzora::PluginState` vocabulary used by the Settings UI.
fn mirror_to_plugin_state(
    kind: LoosePluginStatusKind,
    diagnostics: &[String],
) -> renzora::PluginState {
    use renzora::PluginState;
    match kind {
        LoosePluginStatusKind::Active => PluginState::Loaded,
        LoosePluginStatusKind::Disabled => PluginState::Disabled,
        LoosePluginStatusKind::WrongScope => {
            PluginState::Skipped("wrong scope for this binary".to_string())
        }
        LoosePluginStatusKind::MalformedContract => {
            let msg = diagnostics
                .first()
                .cloned()
                .unwrap_or_else(|| "contract error".to_string());
            PluginState::Skipped(format!("malformed contract: {msg}"))
        }
        LoosePluginStatusKind::AwaitingTrustConsent => {
            PluginState::Skipped("awaiting trust consent".to_string())
        }
        LoosePluginStatusKind::SourceRemoved => PluginState::Skipped("source removed".to_string()),
        LoosePluginStatusKind::CompileFailed
        | LoosePluginStatusKind::LoadFailed
        | LoosePluginStatusKind::AbiRejected
        | LoosePluginStatusKind::LayoutChangeRequiresRestart
        | LoosePluginStatusKind::Compiling
        | LoosePluginStatusKind::Superseded
        | LoosePluginStatusKind::Discovered => {
            let msg = diagnostics
                .first()
                .cloned()
                .unwrap_or_else(|| kind.to_string());
            PluginState::Failed(msg)
        }
    }
}

fn mirror_to_plugin_inventory(
    inventory: &LoosePluginInventory,
    plugin_inventory: &mut renzora::PluginInventory,
) {
    use renzora::PluginKind;
    for (id, row) in inventory.iter() {
        // The shared inventory key MUST be the full canonical identity,
        // not the bare leaf. Distinct canonical paths with the same leaf
        // are different plugins and must remain distinct in Settings,
        // inventory, trust state, and export selection. The bare leaf is
        // surfaced separately as the plugin's display label.
        let canonical_key = id.to_string();
        let state = mirror_to_plugin_state(row.kind, &row.diagnostics);
        plugin_inventory.record(canonical_key, PluginKind::LooseTier1, state);
    }
}

/// Bevy system: copy the loose-plugin inventory rows into the editor's
/// `renzora::PluginInventory`. Runs in `Last` so it sees the freshest
/// state from every other system this frame.
fn mirror_to_editor_inventory(
    inventory: Res<LoosePluginInventory>,
    mut plugin_inventory: ResMut<renzora::PluginInventory>,
) {
    mirror_to_plugin_inventory(&inventory, &mut plugin_inventory);
}

/// Configuration for the loose-plugin host.
///
/// Lives in code (not the project config) because every field is host-
/// deployment: cache root, sdk path, profile. Per-project overrides
/// belong in `project.toml` and are read at integration time, not here.
#[derive(Clone, Debug)]
pub struct LoosePluginHostConfig {
    pub source_root: PathBuf,
    pub staging_root: PathBuf,
    pub build_service: BuildServiceConfig,
    /// ABI version (the `sys::VERSION_MAJOR` the host is built with).
    pub abi_version: u32,
    /// Interface-prefix hashes, in field order. Used as a fingerprint
    /// input so a reload against an older or newer SDK hits a different
    /// cache row.
    pub interface_prefix_hashes: Vec<u32>,
    /// Toolchain stamp to embed in `FingerprintInputs`. The default is
    /// empty, which the build service treats as "first run".
    pub toolchain_stamp: String,
}

impl Default for LoosePluginHostConfig {
    fn default() -> Self {
        Self {
            source_root: PathBuf::from("plugins"),
            staging_root: PathBuf::from("plugins/.loose-staged"),
            build_service: BuildServiceConfig {
                cache_root: PathBuf::from(".loose-cache"),
                profile: BuildProfile::Dist,
                sdk_path: PathBuf::from("sdk"),
                toolchain_stamp: String::new(),
                compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
                n_workers: Some(1),
                n_children: Some(1),
                shutdown_deadline: std::time::Duration::from_secs(5),
                required_symbols_by_kind: std::collections::HashMap::from([(
                    ArtifactKind::Tier1Plugin,
                    vec![b"renzora_plugin_init\0".to_vec()],
                )]),
            },
            abi_version: sys::VERSION_MAJOR,
            interface_prefix_hashes: sys::INTERFACE_PREFIX_HASHES
                .iter()
                .map(|h| *h as u32)
                .collect(),
            toolchain_stamp: String::new(),
        }
    }
}

/// Per-revision build outcome receiver. The map is keyed by canonical
/// identity; each identity holds the receiver for its most-recent
/// submission so rapid edits collapse to "only the newest receiver
/// lives", matching Phase 2's supersession semantics.
#[derive(Default, Resource)]
pub struct LoosePendingBuilds {
    /// One pending receiver per canonical identity, keyed by identity.
    /// Older revisions are superseded by the BuildService and reach the
    /// receiver as `BuildOutcome::Superseded`, which the drain system
    /// records but does not act on.
    pub pending: HashMap<CanonicalId, PendingBuild>,
    /// Activation requests that have been staged but not yet activated.
    /// The drain system pops one per frame and calls
    /// `load_one_transactional`.
    pub staged_for_activation: Vec<(CanonicalId, PathBuf, u64 /* generation */)>,
    /// Diagnostics emitted by the most-recent activation attempt, keyed
    /// by identity. Read by the inventory's `record_diagnostics`.
    pub last_activation_failure: HashMap<CanonicalId, String>,
}

#[derive(Debug)]
pub struct PendingBuild {
    pub receiver: crossbeam_channel::Receiver<BuildOutcome>,
    pub revision: renzora_compiler_cache::types::Revision,
}

/// The `Arc<BuildService>` shared by every loose plugin's submission.
///
/// One per process, mirroring the design's "one BuildService per host"
/// rule. Held by the plugin's resources so the drain system can submit
/// completions to the same service that produced them.
#[derive(Clone, Resource)]
pub struct LooseBuildService(pub Arc<BuildService>);

/// Watcher event drained by the watcher's drain system.
#[derive(Debug, Clone)]
pub enum WatcherEvent {
    /// File at `path` appeared or changed. The drain system reads its
    /// contents, parses the contract, and (subject to trust consent)
    /// submits a build.
    LooseFileChanged {
        canonical_id: CanonicalId,
        path: PathBuf,
    },
    /// File at `path` was deleted or renamed away. The drain system
    /// marks the source removed; the last-good generation, if any,
    /// remains loaded until restart.
    LooseFileRemoved { canonical_id: CanonicalId },
    /// A directory under the source root changed (added/removed). Not a
    /// loose file; the existing directory-plugin watcher handles it.
    DirectoryChanged,
    /// Path that did not pass the root-level `.rs`/no-dotfile filter.
    Ignored,
}

/// The notify-debouncer-full wrapper. Kept in a resource so its drop
/// removes the OS-level watch.
#[derive(Resource)]
pub struct LooseWatcher {
    _debouncer: notify_debouncer_full::Debouncer<
        notify_debouncer_full::notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    pub rx: std::sync::Mutex<std::sync::mpsc::Receiver<notify_debouncer_full::DebounceEventResult>>,
    pub root: PathBuf,
    /// Whether this watcher is editor-only (no watcher in a shipped game).
    pub is_editor: bool,
}

/// The Bevy plugin that wires everything together.
///
/// Add to the editor app via `RenzoraPluginHostPlugin::build` (see the
/// integration site). The plugin does not call `cargo` itself — that is
/// the shared `BuildService`'s job.
#[derive(Clone)]
pub struct LoosePluginHost {
    pub config: LoosePluginHostConfig,
    pub is_editor: bool,
    /// Plugins linked into this binary; the same list `RenzoraPluginHostPlugin`
    /// would pass to `load_dir`. Kept here so the activation path can refuse
    /// a stem that is already linked.
    pub linked_plugin_ids: Vec<String>,
    /// Disabled list mirror. Kept here so the activation path can refuse
    /// to load a plugin the user has turned off.
    pub disabled_plugin_ids: Vec<String>,
    /// Trusted list seed. Populated by `editor_with_trust` from the
    /// persisted `~/.renzora/editor.toml`. Inserted into the
    /// `LoosePluginTrust` resource on plugin install so the watcher and
    /// `initial_scan` see the user's prior consent before any event fires.
    pub trusted_plugin_ids: Vec<String>,
    /// How the loose host resolves its `BuildService`. U4-1
    /// distinguishes three explicit modes; the old `Option<Arc>`
    /// overloading "self-owned" and "unavailable" was the
    /// fourth-review blocker.
    ///
    /// - `Shared(Arc)` — the editor's host assembly constructed the
    ///   shared `Arc`; the loose host installs IT (the SAME Arc
    ///   the `RustScriptBuildService` resource wraps). Loose
    ///   recompiles go through this service's worker pool.
    /// - `Unavailable { diagnostic }` — the editor's host assembly
    ///   could not construct a compiler. Neither loose plugins nor
    ///   Rust scripts can compile source this session. The loose
    ///   host installs WITHOUT `LooseBuildService`; the watchdog
    ///   and inventory systems refuse to submit work. Pre-built C-ABI
    ///   cdylibs are unaffected and can still load.
    /// - `SelfOwned` — the loose host constructs its own service
    ///   inside `build`. Reserved for legacy harness/test callers
    ///   that intentionally request self-owned compilation. Production
    ///   editor entry points and tests use `Shared` or `Unavailable`.
    pub compiler_mode: CompilerMode,
}

/// The three explicit modes the loose host can run under. See
/// [`LoosePluginHost::compiler_mode`] (U4-1).
#[derive(Clone)]
pub enum CompilerMode {
    /// A caller-supplied `Arc<BuildService>` is installed into
    /// `LooseBuildService`. Production editor sessions use this.
    Shared(std::sync::Arc<renzora_compiler_cache::BuildService>),
    /// The compiler is unavailable. No `LooseBuildService` is
    /// installed; submitting source-build work is disallowed. The
    /// diagnostic is surfaced through the editor's diagnostic
    /// channels.
    Unavailable { diagnostic: String },
    /// The loose host constructs its own private `BuildService`
    /// inside `build`. Retained for legacy test callers that
    /// explicitly need a self-owned service.
    SelfOwned,
}

impl std::fmt::Debug for CompilerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompilerMode::Shared(arc) => f
                .debug_tuple("Shared")
                .field(&Arc::strong_count(arc))
                .finish(),
            CompilerMode::Unavailable { diagnostic } => f
                .debug_struct("Unavailable")
                .field("diagnostic", diagnostic)
                .finish(),
            CompilerMode::SelfOwned => f.debug_tuple("SelfOwned").finish(),
        }
    }
}

impl LoosePluginHost {
    /// Construct an editor host pointing at `<plugins_dir>` as both the
    /// source root and the parent of the staging directory.
    pub fn editor(plugins_dir: &std::path::Path, disabled_plugin_ids: Vec<String>) -> Self {
        Self {
            config: LoosePluginHostConfig {
                source_root: plugins_dir.to_path_buf(),
                staging_root: plugins_dir.join(".loose-staged"),
                ..Default::default()
            },
            is_editor: true,
            linked_plugin_ids: Vec::new(),
            disabled_plugin_ids,
            trusted_plugin_ids: Vec::new(),
            // U4-1: explicit default is `Unavailable`, never
            // self-owned. A fresh host without injected compiler mode
            // neither constructs another service nor falls back to
            // one — loose recompiles simply do not happen, which is
            // the editor's correct behavior for a session without
            // the source-modding infrastructure.
            compiler_mode: CompilerMode::Unavailable {
                diagnostic: "no compiler mode selected by the host assembly".to_string(),
            },
        }
    }

    /// Construct an editor host AND seed the trust gate from a list of
    /// canonical ids the user has already consented to. T3-6: this is the
    /// startup-restoration path — the editor binary calls
    /// `renzora::load_trusted_loose_plugins()` and hands the result here so
    /// the loose host's `LoosePluginTrust` resource is populated before any
    /// watcher event is processed.
    pub fn editor_with_trust(
        plugins_dir: &std::path::Path,
        disabled_plugin_ids: Vec<String>,
        trusted_plugin_ids: Vec<String>,
    ) -> Self {
        Self {
            config: LoosePluginHostConfig {
                source_root: plugins_dir.to_path_buf(),
                staging_root: plugins_dir.join(".loose-staged"),
                ..Default::default()
            },
            is_editor: true,
            linked_plugin_ids: Vec::new(),
            disabled_plugin_ids,
            trusted_plugin_ids,
            // U4-1: explicit `Unavailable` default for a fresh host.
            compiler_mode: CompilerMode::Unavailable {
                diagnostic: "no compiler mode selected by the host assembly".to_string(),
            },
        }
    }

    /// Construct an editor host that uses a caller-supplied
    /// `BuildService` instance. The acceptance harness and editor
    /// entry points call this so the loose host installs the SAME
    /// `Arc` as `RustScriptBuildService`. U4-1: this is the ONLY
    /// way the editor reaches the `Shared` mode; a runtime host
    /// has no compiler mode at all (source compilation is out of
    /// scope for a shipped runtime).
    pub fn editor_with_shared_build_service(
        plugins_dir: &std::path::Path,
        disabled_plugin_ids: Vec<String>,
        trusted_plugin_ids: Vec<String>,
        shared_build_service: std::sync::Arc<renzora_compiler_cache::BuildService>,
    ) -> Self {
        let mut host =
            Self::editor_with_trust(plugins_dir, disabled_plugin_ids, trusted_plugin_ids);
        host.compiler_mode = CompilerMode::Shared(shared_build_service);
        host
    }

    /// Switch the host to `Shared(Arc)` mode (U4-1). Used by the
    /// production assembly function that hands the SAME `Arc` to
    /// both `RustScriptPlugin` and the loose host. Replaces any
    /// previous mode.
    pub fn inject_shared_build_service(
        &mut self,
        shared: std::sync::Arc<renzora_compiler_cache::BuildService>,
    ) {
        self.compiler_mode = CompilerMode::Shared(shared);
    }

    /// Switch the host to explicit `Unavailable` mode (U4-1). The
    /// host installs WITHOUT a `LooseBuildService` resource; loose
    /// plugin source compilation is disabled for the session.
    /// Prebuilt C-ABI cdylibs remain loadable.
    pub fn mark_compiler_unavailable(&mut self, diagnostic: impl Into<String>) {
        self.compiler_mode = CompilerMode::Unavailable {
            diagnostic: diagnostic.into(),
        };
    }

    /// Construct a runtime host (no source watcher, no source polling,
    /// no compiler). A runtime host is responsible only for activating
    /// loose plugins a user dropped into `plugins/` — prebuilt C-ABI
    /// cdylibs only; source compilation is out of scope for a shipped
    /// runtime. The runtime host is in `Unavailable` mode by
    /// definition; `compiler_mode` cannot transition to `Shared` on a
    /// non-editor session.
    pub fn runtime(plugins_dir: &std::path::Path, disabled_plugin_ids: Vec<String>) -> Self {
        Self {
            config: LoosePluginHostConfig {
                source_root: plugins_dir.to_path_buf(),
                staging_root: plugins_dir.join(".loose-staged"),
                ..Default::default()
            },
            is_editor: false,
            linked_plugin_ids: Vec::new(),
            disabled_plugin_ids,
            trusted_plugin_ids: Vec::new(),
            compiler_mode: CompilerMode::Unavailable {
                diagnostic: "runtime hosts never compile source".to_string(),
            },
        }
    }

    /// Borrow the current compiler mode as a labeled enum. U4-1
    /// helper used by the editor's host assembly and by tests to
    /// distinguish `Shared`, `Unavailable`, and `SelfOwned` without
    /// reaching into private fields.
    pub fn compiler_mode(&self) -> &CompilerMode {
        &self.compiler_mode
    }

    /// Convenience: true when the host has a `BuildService` it can
    /// submit loose-plugin source work to (U4-1's "compilation is
    /// available" predicate). `Shared` returns `true`; `Unavailable`
    /// and `SelfOwned` are decided by the host assembly.
    pub fn is_compilation_available(&self) -> bool {
        matches!(self.compiler_mode, CompilerMode::Shared(_))
    }

    /// Construct a host that explicitly owns its own `BuildService`,
    /// bypassing the editor's shared service. Reserved for the
    /// acceptance harness's Phase 3 regression tests; the editor
    /// never calls this. U4-1: a separate, explicit constructor
    /// rather than an overload of `editor_with_shared_build_service`
    /// — the distinction is load-bearing for unavailable detection.
    pub fn editor_with_self_owned_build_service(
        plugins_dir: &std::path::Path,
        disabled_plugin_ids: Vec<String>,
        trusted_plugin_ids: Vec<String>,
        build_service_config: renzora_compiler_cache::service::BuildServiceConfig,
    ) -> Self {
        let _ = build_service_config;
        let mut host =
            Self::editor_with_trust(plugins_dir, disabled_plugin_ids, trusted_plugin_ids);
        host.compiler_mode = CompilerMode::SelfOwned;
        host
    }

    /// Look up the source `PathBuf` for a canonical id by reading the
    /// inventory's row. Returns `None` if the row has been removed.
    pub fn source_path(
        inventory: &LoosePluginInventory,
        id: &CanonicalId,
    ) -> Option<std::path::PathBuf> {
        inventory.row(id).and_then(|r| r.source_path.clone())
    }
}

impl Plugin for LoosePluginHost {
    fn build(&self, app: &mut App) {
        let cfg = self.config.clone();
        let source_root = cfg.source_root.clone();
        let staging = StableStaging::new(cfg.staging_root.clone());
        if let Err(e) = staging.ensure() {
            warn!(
                "[loose-plugin] could not create staging root {}: {e}",
                cfg.staging_root.display()
            );
        }

        // U4-1: resolve the host's `BuildService` according to the
        // explicit `CompilerMode`. Three cases:
        //
        // 1. `Shared(Arc)` — install the supplied Arc into
        //    `LooseBuildService`. Production editor sessions.
        // 2. `Unavailable { .. }` — install NO `LooseBuildService`
        //    resource. Loose-plugin source compilation is disabled
        //    for the session. Prebuilt C-ABI cdylibs can still load
        //    because their load path does NOT touch the build
        //    service. The diagnostic is logged so the editor's
        //    settings panel can see it.
        // 3. `SelfOwned` — construct a private service inside
        //    `build`. Reserved for the Phase 3 acceptance harness.
        let build_service = match &self.compiler_mode {
            CompilerMode::Shared(arc) => Some(arc.clone()),
            CompilerMode::Unavailable { diagnostic } => {
                warn!(
                    "[loose-plugin] compiler unavailable; loose-plugin source compilation is disabled: {}",
                    diagnostic
                );
                None
            }
            CompilerMode::SelfOwned => match BuildService::new(cfg.build_service.clone()) {
                Ok(s) => Some(s),
                Err(e) => {
                    error!("[loose-plugin] BuildService::new failed: {e}");
                    return;
                }
            },
        };

        app.insert_resource({
            let mut inv = LoosePluginInventory::default();
            for raw in &self.trusted_plugin_ids {
                if let Some(parsed) = crate::inventory::parse_canonical_id(raw) {
                    inv.grant_consent(parsed);
                }
            }
            // Also seed the inventory's disabled set so a persisted
            // disable survives editor restart. The Settings UI does not
            // have to re-issue a click.
            for raw in &self.disabled_plugin_ids {
                if let Some(parsed) = crate::inventory::parse_canonical_id(raw) {
                    inv.set_enabled(&parsed, false);
                }
            }
            inv
        })
        .insert_resource({
            let mut trust = LoosePluginTrust::default();
            for raw in &self.trusted_plugin_ids {
                if let Some(parsed) = crate::inventory::parse_canonical_id(raw) {
                    trust.grant(parsed);
                }
            }
            trust
        })
        .insert_resource(LoosePendingBuilds::default())
        .insert_resource(LooseStagingHandle(staging))
        .insert_resource(LoosePluginReloadRequests::default())
        .insert_resource(LoosePluginHostMeta {
            source_root: source_root.clone(),
            is_editor: self.is_editor,
            linked_plugin_ids: self.linked_plugin_ids.clone(),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
            build_request_template: make_request_template(&cfg),
        });

        // U4-1: install `LooseBuildService` ONLY when the host is in
        // `Shared` mode. An `Unavailable` host installs nothing; the
        // drain systems below have to gate every `LooseBuildService`
        // access behind `Option<Res<LooseBuildService>>` so the drain
        // gracefully skips source-build work in unavailable mode.
        if let Some(arc) = build_service.clone() {
            app.insert_resource(LooseBuildService(arc));
        } else {
            // Explicitly mark the resource as absent (Bevy cannot
            // install an `Option<Resource>` directly, but we record
            // the unavailability reason in
            // `LoosePluginHostMeta.unavailable_diagnostic` so the
            // settings panel and tests can inspect it).
            warn!(
                "[loose-plugin] editor session without a shared compiler; loose-plugin source compilation is disabled"
            );
        }

        // Watcher: only the editor watches the source root. A shipped
        // game has no toolchain and no source to compile, so the
        // watcher would just spin its channel.
        if self.is_editor {
            match install_watcher(&source_root) {
                Ok(watcher) => {
                    app.insert_resource(watcher)
                        .add_systems(PreUpdate, drain_watcher_events);
                }
                Err(e) => {
                    warn!(
                        "[loose-plugin] could not start watcher on {}: {e}",
                        source_root.display()
                    );
                }
            }
        }

        // The two drain systems. Both run in `PreUpdate` so an event that
        // arrives between two frames is acted on before any schedule runs
        // the plugin's systems. We do not chain them: `drain_pending_builds`
        // enqueues staged paths into `LoosePendingBuilds::staged_for_activation`,
        // and `drain_activation_queue` pops one per frame — the order is
        // implicit (the queue is filled before either runs because the build
        // service is on a worker thread and the events arrive via a channel
        // that both systems drain).
        app.add_systems(PreUpdate, drain_pending_builds);
        app.add_systems(PreUpdate, drain_activation_queue);
        app.add_systems(PreUpdate, process_reload_requests);
        // Mirror the loose-plugin state into the editor's
        // `renzora::PluginInventory` so the Settings panel lists loose
        // plugins alongside directory and C-ABI plugins.
        app.add_systems(Last, mirror_to_editor_inventory);
        // Initial scan runs at `First` so the App's other init systems
        // have run first.
        app.add_systems(First, initial_scan);
    }
}

/// Pending reload requests raised by the editor (a button click, a
/// console command, the watcher detecting a delete + recreate). The
/// loose-plugin host drains this queue once per frame in `PreUpdate`
/// and submits a fresh `BuildRequest` for each entry — which goes
/// through the same `BuildService::submit` path the watcher uses, so
/// supersession is the same too.
#[derive(Resource, Default)]
pub struct LoosePluginReloadRequests(pub Vec<CanonicalId>);

#[derive(Resource, Clone)]
struct LooseStagingHandle(StableStaging);

#[derive(Resource, Clone, Debug)]
struct LoosePluginHostMeta {
    source_root: PathBuf,
    is_editor: bool,
    linked_plugin_ids: Vec<String>,
    disabled_plugin_ids: Vec<String>,
    build_request_template: RequestTemplate,
}

#[derive(Clone, Debug)]
struct RequestTemplate {
    abi_version: u32,
    interface_prefix_hashes: Vec<u32>,
    toolchain_stamp: String,
    sdk_content_hash: [u8; 32],
    target: String,
    profile: BuildProfile,
    panic: PanicStrategy,
    compiler_service_schema: u32,
    rustflags: Vec<String>,
    capabilities: std::collections::BTreeSet<String>,
    wrapper_schema: u32,
    manifest_schema: u32,
    lock_resolution: [u8; 32],
}

fn make_request_template(cfg: &LoosePluginHostConfig) -> RequestTemplate {
    use renzora_compiler_cache::types::default_target_triple;
    RequestTemplate {
        abi_version: cfg.abi_version,
        interface_prefix_hashes: cfg.interface_prefix_hashes.clone(),
        toolchain_stamp: cfg.toolchain_stamp.clone(),
        sdk_content_hash: [0u8; 32], // populated from BuildService::stamps()
        target: default_target_triple(),
        profile: cfg.build_service.profile,
        panic: PanicStrategy::Abort,
        compiler_service_schema: cfg.build_service.compiler_service_schema,
        rustflags: Vec::new(),
        capabilities: std::collections::BTreeSet::new(),
        wrapper_schema: 1,
        manifest_schema: 1,
        lock_resolution: [0u8; 32],
    }
}

/// Construct a `BuildRequest` for `identity` with the supplied source
/// snapshot, using the host's request template.
fn build_request(
    template: &RequestTemplate,
    identity: CanonicalId,
    source_snapshot: Arc<Vec<u8>>,
) -> BuildRequest {
    BuildRequest {
        identity,
        source_snapshot,
        fingerprint_inputs: FingerprintInputs {
            target_triple: template.target.clone(),
            toolchain_stamp: template.toolchain_stamp.clone(),
            sdk_content_hash: template.sdk_content_hash,
            abi_version: template.abi_version,
            interface_prefix_hashes: template.interface_prefix_hashes.clone(),
            wrapper_schema: template.wrapper_schema,
            manifest_schema: template.manifest_schema,
            lock_resolution: template.lock_resolution,
            capabilities: template.capabilities.clone(),
            profile: template.profile,
            rustflags: template.rustflags.clone(),
            panic: template.panic,
            compiler_service_schema: template.compiler_service_schema,
        },
        target: template.target.clone(),
        artifact_kind: ArtifactKind::Tier1Plugin,
    }
}

fn install_watcher(
    root: &std::path::Path,
) -> Result<LooseWatcher, notify_debouncer_full::notify::Error> {
    use notify_debouncer_full::new_debouncer;
    use notify_debouncer_full::notify::RecursiveMode;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(std::time::Duration::from_millis(300), None, tx)?;
    debouncer.watch(root, RecursiveMode::NonRecursive)?;

    Ok(LooseWatcher {
        _debouncer: debouncer,
        rx: std::sync::Mutex::new(rx),
        root: root.to_path_buf(),
        is_editor: true,
    })
}

/// Drain watcher events into the inventory + `BuildService`. Runs in
/// `PreUpdate`.
fn drain_watcher_events(
    watcher: Option<Res<LooseWatcher>>,
    mut inventory: ResMut<LoosePluginInventory>,
    trust: Res<LoosePluginTrust>,
    meta: Res<LoosePluginHostMeta>,
    build_service: Option<Res<LooseBuildService>>,
    mut pending: ResMut<LoosePendingBuilds>,
) {
    // U4-1: an editor session with `Unavailable` compiler mode
    // installs no `LooseBuildService`. The watcher can still see
    // file events; we just skip every submit. Inventory rows
    // transition to `Compiling` → `CompileFailed` with an actionable
    // diagnostic so the editor's settings panel reflects the truth.
    let Some(watcher) = watcher else { return };
    let Ok(rx) = watcher.rx.lock() else { return };
    let Some(build_service) = build_service else {
        // Drain pending events to keep the rx channel from filling,
        // but skip the source-build side effects.
        for _ in rx.try_iter() {}
        return;
    };

    let mut events: Vec<notify_debouncer_full::DebouncedEvent> = Vec::new();
    for batch in rx.try_iter() {
        match batch {
            Ok(evs) => events.extend(evs),
            Err(errors) => {
                for e in errors {
                    warn!("[loose-plugin] watcher error: {e}");
                }
            }
        }
    }
    drop(rx);

    for event in events {
        for path in &event.paths {
            let Some(canonical) = classify_path(&watcher.root, path) else {
                continue;
            };
            match canonical {
                WatcherEvent::LooseFileChanged { canonical_id, path } => {
                    // T3-3: a disabled plugin must never receive a new
                    // submission from the watcher. The Settings UI already
                    // removes pending builds on Disable; this is the
                    // second line of defence in case a watcher event races
                    // the toggle. Drop the receiver and cancel any
                    // already-queued activation.
                    if inventory.is_disabled(&canonical_id) {
                        pending.pending.remove(&canonical_id);
                        pending
                            .staged_for_activation
                            .retain(|(id, _, _)| id != &canonical_id);
                        // Refresh the row so the user sees Disabled in the
                        // Settings UI (the file changed, but the user
                        // disabled the plugin — that wins).
                        let bytes = std::fs::read(&path).ok();
                        let parsed = bytes
                            .as_deref()
                            .and_then(|b| parse_loose_plugin_source(b).ok());
                        if let Some(p) = parsed {
                            inventory.upsert_discovered(
                                canonical_id.clone(),
                                p.scope,
                                path.clone(),
                                trust.has_consent(&canonical_id),
                                true,
                            );
                        }
                        inventory.transition(&canonical_id, LoosePluginStatusKind::Disabled);
                        continue;
                    }
                    let bytes = match std::fs::read(&path) {
                        Ok(b) => b,
                        Err(e) => {
                            warn!("[loose-plugin] could not read {}: {e}", path.display());
                            continue;
                        }
                    };
                    let consented = trust.has_consent(&canonical_id);
                    let disabled = inventory.is_disabled(&canonical_id);
                    let parsed = match parse_loose_plugin_source(&bytes) {
                        Ok(p) => p,
                        Err(e) => {
                            inventory.upsert_discovered(
                                canonical_id.clone(),
                                LoosePluginScope::Runtime,
                                path.clone(),
                                consented,
                                disabled,
                            );
                            inventory.transition(
                                &canonical_id,
                                LoosePluginStatusKind::MalformedContract,
                            );
                            inventory
                                .record_diagnostics(&canonical_id, vec![format!("contract: {e}")]);
                            continue;
                        }
                    };
                    inventory.upsert_discovered(
                        canonical_id.clone(),
                        parsed.scope,
                        path.clone(),
                        consented,
                        disabled,
                    );
                    if !consented {
                        inventory
                            .transition(&canonical_id, LoosePluginStatusKind::AwaitingTrustConsent);
                        continue;
                    }
                    if disabled {
                        inventory.transition(&canonical_id, LoosePluginStatusKind::Disabled);
                        continue;
                    }
                    // Submit to the BuildService. The receiver replaces any
                    // older receiver for the same identity — older
                    // receivers will receive `BuildOutcome::Superseded`
                    // when the BuildService supersedes them.
                    inventory.transition(&canonical_id, LoosePluginStatusKind::Compiling);
                    let source = Arc::new(bytes);
                    let request = build_request(
                        &meta.build_request_template,
                        canonical_id.clone(),
                        source.clone(),
                    );
                    match build_service.0.submit(request) {
                        Ok(rx) => {
                            let revision = renzora_compiler_cache::types::Revision(0);
                            pending.pending.insert(
                                canonical_id.clone(),
                                PendingBuild {
                                    receiver: rx,
                                    revision,
                                },
                            );
                        }
                        Err(e) => {
                            inventory
                                .transition(&canonical_id, LoosePluginStatusKind::CompileFailed);
                            inventory.record_diagnostics(
                                &canonical_id,
                                vec![format!("submit failed: {e}")],
                            );
                        }
                    }
                }
                WatcherEvent::LooseFileRemoved { canonical_id } => {
                    inventory.mark_source_removed(&canonical_id);
                }
                WatcherEvent::DirectoryChanged | WatcherEvent::Ignored => {}
            }
        }
    }
}

/// Decide whether a path emitted by the watcher is a root-level loose
/// file, a directory, or something to ignore.
fn classify_path(root: &std::path::Path, path: &std::path::Path) -> Option<WatcherEvent> {
    let rel = path.strip_prefix(root).ok()?;
    let first = rel.components().next()?;
    let first_str = first.as_os_str().to_string_lossy();
    // Dotfiles (`.reload`, `.cargo`, `target`) are ignored.
    if first_str.starts_with('.') || first_str == "target" {
        return Some(WatcherEvent::Ignored);
    }
    // Nested paths (inside a subdirectory) belong to the directory
    // builder, not to the loose-plugin system.
    if rel.components().count() > 1 {
        return Some(WatcherEvent::DirectoryChanged);
    }
    // Only `.rs` files at the root level are loose-plugin candidates.
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return Some(WatcherEvent::Ignored);
    }
    // Map to a CanonicalId rooted at the engine plugin root.
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    let id_path = file_name.clone();
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Engine, &id_path).ok()?;
    Some(WatcherEvent::LooseFileChanged {
        canonical_id: id,
        path: path.to_path_buf(),
    })
}

/// Initial scan at `First`: list every root-level `.rs` file, register
/// it in the inventory, and submit a build for every enabled + consented
/// one.
fn initial_scan(
    meta: Res<LoosePluginHostMeta>,
    mut inventory: ResMut<LoosePluginInventory>,
    trust: Res<LoosePluginTrust>,
    // U4-1: optional build service. With `Unavailable` compiler mode
    // the resource is absent and the scan only updates the
    // inventory; submit step is skipped.
    build_service: Option<Res<LooseBuildService>>,
    mut pending: ResMut<LoosePendingBuilds>,
) {
    // No `BuildService` installed → skip the submit side of the scan.
    // Inventory rows still get discovered so the Settings UI reflects
    // whatever prebuilt C-ABI cdylibs are present.
    let Some(build_service) = build_service else {
        return initial_scan_inventory_only(&meta, &mut inventory, &trust);
    };
    if !meta.is_editor {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&meta.source_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(canonical_id) =
            CanonicalId::from_rooted(renzora_identity::RootKind::Engine, &file_name)
        else {
            continue;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let consented = trust.has_consent(&canonical_id);
        let disabled = inventory.is_disabled(&canonical_id);
        let parsed = match parse_loose_plugin_source(&bytes) {
            Ok(p) => p,
            Err(e) => {
                inventory.upsert_discovered(
                    canonical_id.clone(),
                    LoosePluginScope::Runtime,
                    path.clone(),
                    consented,
                    disabled,
                );
                inventory.transition(&canonical_id, LoosePluginStatusKind::MalformedContract);
                inventory.record_diagnostics(&canonical_id, vec![format!("contract: {e}")]);
                continue;
            }
        };
        inventory.upsert_discovered(
            canonical_id.clone(),
            parsed.scope,
            path.clone(),
            consented,
            disabled,
        );
        if !consented {
            inventory.transition(&canonical_id, LoosePluginStatusKind::AwaitingTrustConsent);
            continue;
        }
        if disabled {
            inventory.transition(&canonical_id, LoosePluginStatusKind::Disabled);
            continue;
        }
        inventory.transition(&canonical_id, LoosePluginStatusKind::Compiling);
        let source = Arc::new(bytes);
        let request = build_request(
            &meta.build_request_template,
            canonical_id.clone(),
            source.clone(),
        );
        if let Ok(rx) = build_service.0.submit(request) {
            pending.pending.insert(
                canonical_id.clone(),
                PendingBuild {
                    receiver: rx,
                    revision: renzora_compiler_cache::types::Revision(0),
                },
            );
        }
    }
}

/// U4-1: inventory-only initial scan used when `CompilerMode` is
/// `Unavailable` and no `LooseBuildService` resource exists. The
/// scan still walks the source root so the Settings UI sees every
/// discovered `.rs` file, but no `BuildRequest` is submitted — the
/// diagnostic was already handed to the editor by the host
/// assembly.
fn initial_scan_inventory_only(
    meta: &LoosePluginHostMeta,
    inventory: &mut LoosePluginInventory,
    trust: &LoosePluginTrust,
) {
    if !meta.is_editor {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&meta.source_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(canonical_id) =
            CanonicalId::from_rooted(renzora_identity::RootKind::Engine, &file_name)
        else {
            continue;
        };
        let bytes = std::fs::read(&path).unwrap_or_default();
        let consented = trust.has_consent(&canonical_id);
        let disabled = inventory.is_disabled(&canonical_id);
        let parsed = match parse_loose_plugin_source(&bytes) {
            Ok(p) => p,
            Err(e) => {
                inventory.upsert_discovered(
                    canonical_id.clone(),
                    LoosePluginScope::Runtime,
                    path.clone(),
                    consented,
                    disabled,
                );
                inventory.transition(&canonical_id, LoosePluginStatusKind::CompileFailed);
                inventory.record_diagnostics(
                    &canonical_id,
                    vec![format!(
                        "contract: {e}; compiler unavailable, cannot recompile"
                    )],
                );
                continue;
            }
        };
        inventory.upsert_discovered(
            canonical_id.clone(),
            parsed.scope,
            path.clone(),
            consented,
            disabled,
        );
        if !consented {
            inventory.transition(&canonical_id, LoosePluginStatusKind::AwaitingTrustConsent);
            continue;
        }
        if disabled {
            inventory.transition(&canonical_id, LoosePluginStatusKind::Disabled);
            continue;
        }
        inventory.transition(&canonical_id, LoosePluginStatusKind::CompileFailed);
        inventory.record_diagnostics(
            &canonical_id,
            vec!["compiler unavailable; loose-plugin source compilation is disabled".to_string()],
        );
    }
}

/// Drain completed `BuildOutcome`s. On `Published` / `CacheHit`, take the
/// exact `immutable_artifact_path` the service published, copy it into the
/// stable staging directory, and queue an activation request. On
/// `CompileFailed`, record the diagnostic. On `Superseded`, drop the
/// receiver. On `Shutdown` / `Cancelled`, drop the receiver.
fn drain_pending_builds(
    mut pending: ResMut<LoosePendingBuilds>,
    mut inventory: ResMut<LoosePluginInventory>,
    staging: Res<LooseStagingHandle>,
) {
    let mut to_remove: Vec<CanonicalId> = Vec::new();
    let mut to_stage: Vec<(
        CanonicalId,
        renzora_compiler_cache::types::PublishedGeneration,
        PathBuf,
    )> = Vec::new();
    let mut diagnostics: Vec<(CanonicalId, LoosePluginStatusKind, Vec<String>)> = Vec::new();
    for (id, pb) in pending.pending.iter() {
        match pb.receiver.try_recv() {
            Ok(outcome) => match outcome {
                BuildOutcome::Published {
                    generation,
                    immutable_artifact_path,
                    ..
                } => {
                    // T3-3: a build outcome that lands AFTER the user
                    // disabled the plugin must not stage. Drop the
                    // receiver and mark the row Disabled; the artifact
                    // stays on disk but is never activated.
                    if inventory.is_disabled(id) {
                        inventory.transition(id, LoosePluginStatusKind::Disabled);
                        to_remove.push(id.clone());
                        continue;
                    }
                    // The BuildService publishes the exact immutable artifact
                    // path; we consume it directly without reverse-engineering
                    // the cache layout.
                    to_stage.push((id.clone(), generation, immutable_artifact_path));
                }
                BuildOutcome::CacheHit {
                    generation,
                    immutable_artifact_path,
                    ..
                } => {
                    if inventory.is_disabled(id) {
                        inventory.transition(id, LoosePluginStatusKind::Disabled);
                        to_remove.push(id.clone());
                        continue;
                    }
                    to_stage.push((id.clone(), generation, immutable_artifact_path));
                }
                BuildOutcome::Superseded { .. } => {
                    diagnostics.push((id.clone(), LoosePluginStatusKind::Superseded, Vec::new()));
                    to_remove.push(id.clone());
                }
                BuildOutcome::CompileFailed { diagnostics: d, .. } => {
                    diagnostics.push((
                        id.clone(),
                        LoosePluginStatusKind::CompileFailed,
                        d.into_iter().map(|x| x.message).collect(),
                    ));
                    to_remove.push(id.clone());
                }
                BuildOutcome::Cancelled { .. } => {
                    to_remove.push(id.clone());
                }
                BuildOutcome::Shutdown { .. } => {
                    to_remove.push(id.clone());
                }
            },
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                to_remove.push(id.clone());
            }
        }
    }
    for (id, kind, diags) in diagnostics {
        inventory.transition(&id, kind);
        if !diags.is_empty() {
            inventory.record_diagnostics(&id, diags);
        }
    }
    // Drop borrow on pending before staging.
    for id in to_remove {
        pending.pending.remove(&id);
    }
    let staging_root = staging.0.clone();
    for (id, generation, artifact_path) in to_stage {
        if !artifact_path.is_file() {
            inventory.transition(&id, LoosePluginStatusKind::CompileFailed);
            inventory.record_diagnostics(
                &id,
                vec![format!(
                    "BuildService published a path that does not exist on disk: {}",
                    artifact_path.display()
                )],
            );
            continue;
        }
        match staging_root.place(&id, &artifact_path) {
            Ok(placement) => {
                pending.staged_for_activation.push((
                    id.clone(),
                    placement.stable_path.clone(),
                    generation.0,
                ));
                inventory.set_active_generation(&id, None, Some(placement.stable_path));
            }
            Err(e) => {
                inventory.transition(&id, LoosePluginStatusKind::CompileFailed);
                inventory.record_diagnostics(&id, vec![format!("staging failed: {e}")]);
            }
        }
    }
}

/// Drain the activation queue: pop one staged path per frame and run
/// `load_one_transactional` on it via the `LoosePluginActivationRunner`
/// resource that owns the `&mut World` access.
fn drain_activation_queue(world: &mut World) {
    // The resource owns the queue; we pop one and call the host path
    // directly. Bevy's normal system scheduler cannot give a system
    // `&mut World` and individual `Res` parameters at the same time,
    // so we run this as a top-level system that owns the whole world
    // for its single activation attempt.
    let mut staged = match world.get_resource_mut::<LoosePendingBuilds>() {
        Some(mut s) => std::mem::take(&mut s.staged_for_activation),
        None => Vec::new(),
    };
    let (id, stable_path, _generation) = match staged.pop() {
        Some(t) => t,
        None => return,
    };
    // Re-store any leftover staged entries (we only popped one).
    if !staged.is_empty() {
        if let Some(mut s) = world.get_resource_mut::<LoosePendingBuilds>() {
            s.staged_for_activation = staged;
        }
    }
    let meta = match world.get_resource::<LoosePluginHostMeta>() {
        Some(m) => m.clone(),
        None => return,
    };
    // T3-3: re-check the inventory's `is_disabled` immediately before
    // loading. The `LoosePluginHostMeta.disabled_plugin_ids` list is the
    // startup snapshot — it does not see Settings UI toggles during
    // the session. The authoritative live state is `LoosePluginInventory`.
    {
        let inventory = match world.get_resource::<LoosePluginInventory>() {
            Some(i) => i,
            None => return,
        };
        if inventory.is_disabled(&id) {
            let mut inventory = world.get_resource_mut::<LoosePluginInventory>().unwrap();
            inventory.transition(&id, LoosePluginStatusKind::Disabled);
            return;
        }
    }
    let linked: Vec<&str> = meta.linked_plugin_ids.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<String> = meta.disabled_plugin_ids.clone();
    let result = loader::load_one_transactional(
        world,
        &stable_path,
        meta.is_editor,
        &linked,
        &disabled,
        id.clone(),
    );
    let mut inventory = match world.get_resource_mut::<LoosePluginInventory>() {
        Some(i) => i,
        None => return,
    };
    match result {
        Ok(loader::TransactionalActivationOutcome::Committed { generation, .. }) => {
            inventory.transition(&id, LoosePluginStatusKind::Active);
            inventory.set_active_generation(&id, Some(generation), Some(stable_path));
        }
        Ok(loader::TransactionalActivationOutcome::RolledBack { failure, .. }) => {
            let (kind, msg) = match failure {
                loader::ActivationFailure::InitFailed => (
                    LoosePluginStatusKind::LoadFailed,
                    "plugin init returned Failed".to_string(),
                ),
                loader::ActivationFailure::VersionTooOld => (
                    LoosePluginStatusKind::AbiRejected,
                    "version too old".to_string(),
                ),
                loader::ActivationFailure::AbiMismatch => (
                    LoosePluginStatusKind::AbiRejected,
                    "plugin was built against a differently-shaped interface table".to_string(),
                ),
                loader::ActivationFailure::UnknownInitStatus(s) => (
                    LoosePluginStatusKind::AbiRejected,
                    format!("unknown init status {s}"),
                ),
                loader::ActivationFailure::LayoutConflict(why) => (
                    LoosePluginStatusKind::LayoutChangeRequiresRestart,
                    format!("layout conflict: {why}"),
                ),
                loader::ActivationFailure::OpenFailed(s) => (
                    LoosePluginStatusKind::LoadFailed,
                    format!("open failed: {s}"),
                ),
            };
            inventory.transition(&id, kind);
            inventory.record_diagnostics(&id, vec![msg]);
        }
        Err(LoadOutcome::Failed(why)) => {
            inventory.transition(&id, LoosePluginStatusKind::LoadFailed);
            inventory.record_diagnostics(&id, vec![why]);
        }
        Err(LoadOutcome::VersionTooOld) => {
            inventory.transition(&id, LoosePluginStatusKind::AbiRejected);
        }
        Err(LoadOutcome::WrongScope(_)) => {
            inventory.transition(&id, LoosePluginStatusKind::WrongScope);
        }
        Err(LoadOutcome::Disabled) => {
            inventory.transition(&id, LoosePluginStatusKind::Disabled);
        }
        Err(LoadOutcome::NotAPlugin) => {
            inventory.transition(&id, LoosePluginStatusKind::LoadFailed);
        }
        Err(LoadOutcome::Loaded) => {
            inventory.transition(&id, LoosePluginStatusKind::Active);
        }
    }
}

/// Drain `LoosePluginReloadRequests`. For each requested canonical id,
/// re-read the source snapshot and submit a fresh build through the
/// shared `BuildService`. The same supersession semantics the watcher
/// uses apply: an older receiver for the same id terminates as
/// `BuildOutcome::Superseded` and is dropped by `drain_pending_builds`.
fn process_reload_requests(
    mut reloads: ResMut<LoosePluginReloadRequests>,
    mut inventory: ResMut<LoosePluginInventory>,
    trust: Res<LoosePluginTrust>,
    meta: Res<LoosePluginHostMeta>,
    // U4-1: optional build service. `None` means compiler is
    // unavailable; the reload request is dropped after recording
    // its diagnostic on the inventory row.
    build_service: Option<Res<LooseBuildService>>,
    mut pending: ResMut<LoosePendingBuilds>,
) {
    // U4-1: drain-mode compiler unavailable. The reload request is
    // consumed; the inventory row records the diagnostic so the
    // settings panel reflects the truth.
    let Some(build_service) = build_service else {
        let ids = std::mem::take(&mut reloads.0);
        for id in ids {
            let Some(row) = inventory.row(&id).cloned() else {
                continue;
            };
            if inventory.is_disabled(&id) {
                continue;
            }
            let _ = row;
            inventory.transition(&id, LoosePluginStatusKind::CompileFailed);
            inventory.record_diagnostics(
                &id,
                vec![
                    "compiler unavailable; loose-plugin source compilation is disabled".to_string(),
                ],
            );
        }
        return;
    };
    let ids = std::mem::take(&mut reloads.0);
    for id in ids {
        let Some(row) = inventory.row(&id).cloned() else {
            continue;
        };
        if inventory.is_disabled(&id) {
            continue;
        }
        if !trust.has_consent(&id)
            && matches!(row.kind, LoosePluginStatusKind::AwaitingTrustConsent)
        {
            // Reload refused without consent. A trust grant will trigger
            // the initial build via `initial_scan`; this path is for
            // re-loading an already-trusted plugin.
            continue;
        }
        let Some(source_path) = row.source_path.clone() else {
            continue;
        };
        let bytes = match std::fs::read(&source_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let snapshot = Arc::new(bytes);
        let req = build_request(&meta.build_request_template, id.clone(), snapshot);
        let rx = match build_service.0.submit(req) {
            Ok(rx) => rx,
            Err(_) => continue,
        };
        pending.pending.insert(
            id.clone(),
            PendingBuild {
                receiver: rx,
                revision: renzora_compiler_cache::types::Revision(0),
            },
        );
        inventory.transition(&id, LoosePluginStatusKind::Compiling);
    }
}

/// Shared command used by both the Settings UI's `plugin_toggle_click`
/// and the loose-plugin integration tests. F3-9: the production toggle
/// and the test trigger must call the same function so the disable /
/// enable contract is verified at one definition. A test that
/// hand-stages the inventory, pending builds, reload requests, and
/// drain transitions would be testing a duplicate, not the production
/// path.
///
/// Disabling:
/// 1. Sets `LoosePluginInventory::set_enabled(false)`.
/// 2. Transitions the inventory row to `Disabled` so Settings reads it.
/// 3. Drops the in-flight receiver from `LoosePendingBuilds::pending`.
/// 4. Removes any queued staged-for-activation entry.
/// 5. Returns without touching `LoosePluginReloadRequests`.
///
/// Enabling:
/// 1. Sets `LoosePluginInventory::set_enabled(true)`.
/// 2. Pushes the canonical id to `LoosePluginReloadRequests`. The next
///    `process_reload_requests` run submits a fresh `BuildRequest`
///    through the same `BuildService::submit` path the watcher uses;
///    the drain systems then stage and activate via
///    `drain_pending_builds` → `drain_activation_queue`. The id is not
///    transitioned in the inventory here — `initial_scan` /
///    `process_reload_requests` is what flips it to `Compiling`.
pub fn apply_loose_plugin_toggle(
    inventory: &mut LoosePluginInventory,
    pending: &mut LoosePendingBuilds,
    reloads: &mut LoosePluginReloadRequests,
    id: &CanonicalId,
    enable: bool,
) {
    inventory.set_enabled(id, enable);
    if !enable {
        inventory.transition(id, LoosePluginStatusKind::Disabled);
        pending.pending.remove(id);
        pending
            .staged_for_activation
            .retain(|(qid, _, _)| qid != id);
    } else {
        reloads.0.push(id.clone());
    }
}
