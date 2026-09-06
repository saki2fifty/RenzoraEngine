//! Rust scripts: per-entity native code, compiled from the project,
//! run through the versioned Tier 1 C-ABI in `renzora_plugin::script`.
//!
//! ```ignore
//! // <project>/enemy/spin.rs
//! use renzora_plugin::script::*;
//!
//! fn update(ctx: &Ctx, _reply: &mut ScriptReply) -> Result<(), String> {
//!     let _ = ctx;
//!     Ok(())
//! }
//!
//! renzora_plugin::rust_script!(update);
//! ```
//!
//! Attached exactly like a Lua script: drop it into the entity's **Scripts**
//! component. Routing is by file extension, the same way `.lua`, `.blueprint`
//! and `.bp` already route.
//!
//! ## Phase 4 shape
//!
//! A compiled `.rs` script is a small `cdylib` linked only against
//! `renzora_plugin`. The script exports a [`CompiledScriptDesc`] through
//! the `renzora_plugin::rust_script!` macro; the host loads the cdylib,
//! validates the descriptor, and registers the per-cdylib
//! `unsafe extern "C" fn` `entry` in the host-side
//! [`CompiledScriptBackend`] registry keyed by canonical identity.
//!
//! The host invokes `.rs` scripts through the existing `ScriptEngine`
//! → `PluginScriptBackend` path so preview/play gating, hooks, host
//! calls, prop write-back, REPL, and ScriptComponents all behave
//! identically to Lua. There is one authoritative execution route.
//!
//! The previous Bevy-Rust-ABI form (`renzora::script!` macro,
//! `fn(&mut World, Entity)` entry) is rejected at load with a clear
//! migration diagnostic — unrestricted Bevy access belongs to the
//! restart-required engine-plugin tier (Phase 5). A script that still
//! uses the old macro is recognised by the discovery layer but is
//! never loaded through the unsafe old ABI.

// `assembly.rs` was removed in U4-3. Cross-feature orchestration
// now lives in `renzora_runtime::host_assembly`. The script feature
// crate no longer depends on `renzora_loose_plugins`.
pub mod backend;
pub mod build_service;
pub mod compiled_runtime;
pub mod discovery;
pub mod lifecycle;
pub mod script_resolve;
/// U4-4: production OS-watcher adapter (separate plugin). Editor
/// entry points add this plugin; tests that inject events through
/// [`ScriptSourceEventQueue`] skip it entirely.
pub mod source_watcher;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use libloading::Library;
use renzora::core::console_log::console_error;
use renzora_identity::{BareAliasIndex, CanonicalId};
use renzora_plugin::script::compiled::ScriptGeneration;
use renzora_plugin::script::CompiledScriptDesc;

pub use build_service::{RustScriptBuildService, RustScriptSharedService};
pub use compiled_runtime::CompiledScriptSlot;
pub use lifecycle::{
    PendingRetires, ScriptSourceEvent, ScriptSourceEventQueue, ScriptWatcher, SourceDebouncer,
};
pub use script_resolve::{ResolvedScript, PREBUILT_MANIFEST};
pub use source_watcher::{SourceWatcherAttachment, SourceWatcherFactory};

renzora::add!(RustScriptPlugin, Runtime);

/// The default system set RustScriptPlugin installs around its
/// lifecycle systems. The chain is mandatory: detect the project
/// lifecycle transition, retire or invalidate the previous project's
/// work, then activate the new project's scripts.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RustScriptSet {
    /// Compute the lifecycle action (`lifecycle::watch`). Drains pending
    /// events and applies the lifecycle state machine.
    Lifecycle,
    /// Activate the new project's scripts by reading pending
    /// `BuildOutcome`s and loading the resulting artifacts
    /// (`lifecycle::activate`).
    Activate,
}

/// Holds the engine-side `PluginScriptBackend` installed on first
/// `PreUpdate`.
#[derive(Resource, Default)]
pub struct RustScriptBackendResource {
    pub installed: bool,
}

fn register_backend_once(
    mut done: ResMut<RustScriptBackendResource>,
    engine: Option<ResMut<renzora_scripting::ScriptEngine>>,
) {
    if done.installed {
        return;
    }
    let Some(mut engine) = engine else { return };
    engine.add_backend(crate::backend::compile_rust_script_backend());
    done.installed = true;
}

/// F4-1: keep the dispatcher-facing global slot in lockstep with
/// the Bevy resource. Runs every PreUpdate so a re-installation of
/// the resource (e.g. in tests) is reflected in the dispatcher's
/// next call.
fn sync_global_slot(slot: Res<CompiledScriptSlot>) {
    crate::backend::install_global_slot(slot.clone());
}

#[derive(Default, Resource)]
struct GlobalSlotInstalled(bool);

/// F4-1: install the dispatcher state once on app build. The
/// dispatcher reads its registry from a process-global slot so the
/// `unsafe extern "C"` entry can resolve canonical identity without
/// capturing Bevy resources through a non-`Send` closure.
fn install_global_slot_once(slot: Res<CompiledScriptSlot>, mut done: Local<GlobalSlotInstalled>) {
    if done.0 {
        return;
    }
    done.0 = true;
    crate::backend::install_global_slot(slot.clone());
}

/// A Bevy resource holding the descriptor symbol names a compiled
/// script cdylib exports.
#[derive(Resource, Clone)]
pub struct CompiledScriptSymbols {
    /// Symbol the loader reads for the descriptor pointer.
    pub descriptor: &'static [u8],
    /// Symbol the loader reads for the entry (kept for
    /// compatibility; the descriptor's `entry` field carries the
    /// pointer the host actually registers).
    pub entry: &'static [u8],
}

impl Default for CompiledScriptSymbols {
    fn default() -> Self {
        Self {
            descriptor: b"renzora_plugin_tier1_script_desc\0",
            entry: b"renzora_plugin_tier1_script_call\0",
        }
    }
}

/// One installation of the plugin per process.
///
/// The plugin reads the editor-installed [`RustScriptSharedService`]
/// resource during `build` and promotes it into a
/// [`RustScriptBuildService`] the lifecycle systems submit through.
/// If the editor never installs the shared service (a runtime-only
/// build), the lifecycle systems are no-ops and no script is ever
/// compiled. Tests drive the production lifecycle by installing
/// [`RustScriptBuildService`] directly with a real `BuildService`
/// they constructed; the systems under test are unchanged.
#[derive(Default)]
pub struct RustScriptPlugin;

impl Plugin for RustScriptPlugin {
    fn build(&self, app: &mut App) {
        // Promote the editor-installed shared service (if any) into
        // the resource the lifecycle systems actually submit
        // through. Without this, the watcher is a no-op because the
        // build service resource is absent.
        //
        // Production assembly (`assembly::install_compiler_service`)
        // installs BOTH `RustScriptSharedService` and
        // `RustScriptBuildService` BEFORE `add_engine_plugins` so the
        // plugin sees both during its `build`. This fallback exists
        // for tests that add the plugin after the resource is
        // installed, or for future callers that wire the shared
        // service through a different code path.
        if let Some(shared) = app.world().get_resource::<RustScriptSharedService>() {
            let svc = shared.0.clone();
            if app
                .world()
                .get_resource::<RustScriptBuildService>()
                .is_none()
            {
                app.insert_resource(RustScriptBuildService(svc));
            }
        }

        app.init_resource::<RustScriptBackendResource>()
            .init_resource::<CompiledScriptSymbols>()
            .init_resource::<LifecycleState>()
            .init_resource::<LifecycleDiagnostics>()
            .init_resource::<BuildOutcomeHistory>()
            .init_resource::<LoadedScripts>()
            .init_resource::<CompiledScriptSlot>()
            // U4-4: the production source-event seam. Registered
            // here so the lifecycle and any plugin (or test) can
            // reach it without checking whether the watcher
            // attached its own private channel.
            .init_resource::<ScriptSourceEventQueue>()
            .add_systems(PreUpdate, register_backend_once)
            .add_systems(PreUpdate, sync_global_slot);

        // F4-1: install the dispatcher's process-global slot on the
        // first build so the host dispatcher can find the registry.
        app.add_systems(Startup, install_global_slot_once);

        #[cfg(feature = "static_scripts")]
        {
            app.init_resource::<LoadedScripts>()
                .add_systems(PreUpdate, load_static_scripts);
        }

        // The lifecycle systems only do useful work when a
        // `RustScriptBuildService` is installed. Without one
        // (runtime-only build), skip registration so the watcher
        // channel never spins. The script loader path is gated
        // separately at the call site (`load_compiled_script`
        // uses `libloading`, which requires a shared-engine
        // build); the lifecycle itself is cheap to register.
        if app
            .world()
            .get_resource::<RustScriptBuildService>()
            .is_none()
        {
            debug!(
                "[rust-script] no RustScriptBuildService installed — lifecycle disabled (runtime-only build)"
            );
            return;
        }

        app.init_resource::<ScriptWatcher>()
            .init_resource::<PendingRetires>()
            .add_systems(Update, lifecycle::watch.in_set(RustScriptSet::Lifecycle))
            .add_systems(Update, lifecycle::activate.in_set(RustScriptSet::Activate))
            .add_systems(PreUpdate, load_prebuilt_scripts);

        app.configure_sets(
            Update,
            (RustScriptSet::Lifecycle, RustScriptSet::Activate).chain(),
        )
        .configure_sets(
            Update,
            (
                renzora_scripting::ScriptingSet::PreScript,
                RustScriptSet::Lifecycle,
                RustScriptSet::Activate,
            )
                .chain(),
        );

        if !app.is_plugin_added::<renzora_scripting::ScriptingPlugin>() {
            app.init_resource::<renzora_scripting::ScriptsActive>()
                .add_systems(
                    Update,
                    renzora_scripting::update_scripts_active
                        .in_set(renzora_scripting::ScriptingSet::PreScript),
                );
        }
    }
}

/// Per-process state the lifecycle thread owns: the in-flight
/// `BuildRequest` receivers the editor has submitted, keyed by
/// canonical identity. Populated by [`lifecycle::submit_scripts`],
/// drained by [`lifecycle::activate`].
#[derive(Default, Resource)]
pub struct LifecycleState {
    /// Receivers keyed by canonical id. The receiver resolves to a
    /// `BuildOutcome`; on `Published` / `CacheHit`, the
    /// `immutable_artifact_path` is loaded.
    pub pending: HashMap<CanonicalId, PendingBuild>,
    /// Receivers displaced by a newer submission. They remain here
    /// until their terminal `Superseded` (or raced terminal) result is
    /// recorded in `BuildOutcomeHistory`.
    pub superseded_pending: Vec<(CanonicalId, PendingBuild)>,
    /// Identity the BuildService has handed a successful outcome for
    /// but the loader has not yet promoted to the registry. One
    /// frame's worth of work lives here.
    pub ready: Vec<ReadyBuild>,
}

/// Production diagnostic resource the lifecycle writes into on
/// `BuildOutcome::CompileFailed` or successful `Published` /
/// `CacheHit` outcomes. Acceptance tests assert against this
/// resource (T4-7, T4-8) instead of inspecting the private
/// `LifecycleState::pending` receiver. The production activate
/// system is the only writer.
#[derive(Default, Resource, Debug)]
pub struct LifecycleDiagnostics {
    /// Last identity the activate system recorded a
    /// `BuildOutcome::CompileFailed` for.
    pub last_compile_failed: Option<CanonicalId>,
    /// The compiler-emitted diagnostic message captured for the
    /// last `CompileFailed` outcome.
    pub last_compile_failed_message: Option<String>,
    /// Last identity the activate system successfully loaded as
    /// `Published` or `CacheHit`.
    pub last_built_id: Option<CanonicalId>,
    /// Generation number of the last successfully built script.
    pub last_built_generation: Option<u64>,
}

/// Terminal category recorded when the lifecycle consumes a build
/// receiver. Keeping this small makes it suitable for diagnostics and
/// deterministic acceptance tests without exposing pending receivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalBuildKind {
    Published,
    CacheHit,
    Superseded,
    CompileFailed,
    Cancelled,
    Shutdown,
}

/// One terminal result observed by the production activation system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildOutcomeRecord {
    pub id: CanonicalId,
    pub request_revision: renzora_compiler_cache::types::Revision,
    pub superseded_by: Option<renzora_compiler_cache::types::Revision>,
    pub kind: TerminalBuildKind,
}

/// Bounded history of terminal compiler results.
#[derive(Resource, Default, Debug)]
pub struct BuildOutcomeHistory {
    records: VecDeque<BuildOutcomeRecord>,
}

impl BuildOutcomeHistory {
    const CAPACITY: usize = 128;

    pub fn push(&mut self, record: BuildOutcomeRecord) {
        if self.records.len() == Self::CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    pub fn records(&self) -> impl Iterator<Item = &BuildOutcomeRecord> {
        self.records.iter()
    }
}

type ObserverMaker =
    dyn Fn(&CanonicalId, u64) -> Box<dyn std::any::Any + Send + Sync> + Send + Sync;

/// Optional instrumentation for observing real generation retirement.
/// Production does not install this resource; acceptance tests use it
/// to prove the library-owning generation drops at the correct time.
#[derive(Resource, Clone)]
pub struct ScriptGenerationObserverFactory(pub Arc<ObserverMaker>);

/// One in-flight `BuildRequest` submission.
pub struct PendingBuild {
    pub receiver: crossbeam_channel::Receiver<renzora_compiler_cache::types::BuildOutcome>,
    pub revision: renzora_compiler_cache::types::Revision,
}

/// A successful outcome waiting to be activated.
pub struct ReadyBuild {
    pub id: CanonicalId,
    pub artifact: PathBuf,
    pub generation: renzora_compiler_cache::types::PublishedGeneration,
}

/// Every script image loaded this session, keyed by canonical id.
///
/// The slot stores an `Arc<ScriptGeneration>` so the
/// `PluginScriptBackend` can dispatch through it. Each `Arc` keeps
/// its per-cdylib `Library` mapped for the lifetime of every clone;
/// retiring a canonical id drops the registry's `Arc`, and an
/// in-flight dispatch that already cloned its `Arc` continues to
/// run safely.
#[derive(Resource, Default)]
pub struct LoadedScripts {
    pub(crate) entries: HashMap<CanonicalId, Arc<ScriptGeneration>>,
    alias_index: BareAliasIndex,
}

impl LoadedScripts {
    pub fn is_loaded(&self, id: &CanonicalId) -> bool {
        self.entries.contains_key(id)
    }

    /// Insert a loaded generation. The generation owns its
    /// `Library`; `LoadedScripts` only holds an `Arc` clone.
    pub fn insert(&mut self, id: CanonicalId, generation: Arc<ScriptGeneration>) {
        let already_present = self.entries.contains_key(&id);
        if !already_present {
            self.alias_index.insert(id.clone());
        }
        self.entries.insert(id, generation);
    }

    /// Insert without supplying the owning library — used by the
    /// static-script loader that emits entries at link time.
    pub fn insert_borrowed(&mut self, id: CanonicalId, generation: Arc<ScriptGeneration>) {
        let already_present = self.entries.contains_key(&id);
        if !already_present {
            self.alias_index.insert(id.clone());
        }
        self.entries.insert(id, generation);
    }

    pub fn lookup(&self, id: &CanonicalId) -> Option<Arc<ScriptGeneration>> {
        self.entries.get(id).cloned()
    }

    pub fn remove(&mut self, id: &CanonicalId) -> Option<Arc<ScriptGeneration>> {
        self.alias_index.remove(id);
        self.entries.remove(id)
    }

    pub fn ids(&self) -> Vec<CanonicalId> {
        let mut v: Vec<CanonicalId> = self.entries.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Publish the compiled-in script table into [`LoadedScripts`].
///
/// Runs once. There is nothing to compile, nothing to load and
/// nothing that can fail — the entry points were resolved by the
/// linker, so if the binary started they are valid. That is the
/// whole difference from `load_compiled_script`: the same map,
/// filled from an array instead of from `dlopen`.
///
/// Keyed by canonical id (`project://<rel>`) so duplicate-leaf
/// scripts retain their distinct identities; bare-name aliasing is
/// handled by [`LoadedScripts::insert`].
#[cfg(feature = "static_scripts")]
fn load_static_scripts(
    mut loaded: ResMut<LoadedScripts>,
    slot: Res<CompiledScriptSlot>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    *done = true;
    let table = renzora_static_scripts::scripts();
    if table.is_empty() {
        return;
    }
    for (id, entry) in &table {
        // The per-cdylib `unsafe extern "C"` entry IS the
        // `ScriptEntry`. The static-link path links the script's
        // cdylib into the binary, so the entry's text segment is
        // the binary's text segment and the `Library` placeholder
        // is `()` (no separate mapping to keep alive).
        let generation = Arc::new(ScriptGeneration::new(
            *entry,
            Box::new(()) as Box<dyn std::any::Any + Send + Sync>,
            1,
        ));
        slot.register(&id.to_string(), generation.clone());
        loaded.insert_borrowed(id.clone(), generation);
    }
    info!(
        "[rust-script] {} script(s) compiled into this build",
        table.len()
    );
}

/// Load the script libraries a copy-based export shipped.
fn load_prebuilt_scripts(
    mut loaded: ResMut<LoadedScripts>,
    slot: Res<CompiledScriptSlot>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    *done = true;

    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let dir = dir.join("scripts");
    let Ok(index) = std::fs::read_to_string(dir.join(PREBUILT_MANIFEST)) else {
        return;
    };

    let mut opened: HashMap<String, Arc<ScriptGeneration>> = HashMap::new();
    let mut count = 0usize;
    for line in index.lines() {
        let Some((key, file)) = line.split_once('\t') else {
            continue;
        };
        let (key, file) = (key.trim(), file.trim());
        if key.is_empty() || file.is_empty() {
            continue;
        }
        let canonical = match renzora_identity::CanonicalId::parse(key) {
            Ok(c) => c,
            Err(_) => {
                match renzora_identity::CanonicalId::from_rooted(
                    renzora_identity::RootKind::Project,
                    key,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("[rust-script] malformed prebuilt manifest row {key:?}: {e}");
                        continue;
                    }
                }
            }
        };
        if let Some(generation) = opened.get(file) {
            loaded.insert_borrowed(canonical.clone(), generation.clone());
            continue;
        }
        match load_compiled_script(&dir.join(file)) {
            Ok(generation) => {
                slot.register(&canonical.to_string(), generation.clone());
                loaded.insert(canonical, generation.clone());
                opened.insert(file.to_string(), generation);
                count += 1;
            }
            Err(e) => {
                error!("[rust-script] could not load shipped script {file}: {e}");
                console_error("Script", format!("could not load {file}: {e}"));
            }
        }
    }
    if count > 0 {
        info!("[rust-script] loaded {count} shipped script librar(ies)");
    }
}

/// `dlopen` a built compiled-script cdylib and validate its
/// descriptor.
///
/// The descriptor negotiation uses [`renzora_plugin::script::check_compat`]
/// to verify ABI version, descriptor size, prefix-hash chain, and
/// capability mask before this function returns. A descriptor that
/// fails any check returns an error and the freshly opened
/// `Library` drops normally. On success, the function returns an
/// `Arc<ScriptGeneration>` whose `Library` keeps the cdylib
/// mapped for the lifetime of every clone.
///
/// `generation` is the caller's monotonic counter for the active
/// canonical identity. The loader does not invent a number; the
/// production `lifecycle::activate` path passes the
/// `BuildOutcome::Published.generation` value through. The default
/// of `1` is what tests use when they call
/// `load_compiled_script` without a generation.
///
/// SAFETY: code compiled from the project's own source, put there
/// by the person running the editor. Same trust model as a plugin.
pub fn load_compiled_script(path: &Path) -> Result<Arc<ScriptGeneration>, String> {
    load_compiled_script_with_generation(path, 1)
}

/// Like [`load_compiled_script`] but with an explicit generation
/// number. Production callers pass the `BuildOutcome::Published`
/// generation; the default of 1 is what tests use.
pub fn load_compiled_script_with_generation(
    path: &Path,
    generation: u64,
) -> Result<Arc<ScriptGeneration>, String> {
    load_compiled_script_with_observer(path, generation, None)
}

/// Load with an optional drop observer. The observer is dropped
/// when the LAST `Arc<ScriptGeneration>` for this generation is
/// released — i.e. when every in-flight dispatch has finished and
/// the registry's prior-generation swap has retired. T4-10 uses
/// this to prove the library is actually released, not merely
/// that the `Arc` count changed.
pub fn load_compiled_script_with_observer(
    path: &Path,
    generation: u64,
    observer: Option<Box<dyn std::any::Any + Send + Sync>>,
) -> Result<Arc<ScriptGeneration>, String> {
    let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;

    // T4-3: size-safe descriptor query ABI. The host first asks the
    // cdylib for its descriptor size (a fixed `u32`); only after
    // validating that the size matches the host's own
    // `mem::size_of::<CompiledScriptDesc>()` does the host allocate
    // a buffer and ask the cdylib to copy the descriptor into it.
    // The previous `*const CompiledScriptDesc` dereference copied
    // the FULL current struct from foreign memory before
    // validating its size; an older or malicious descriptor with
    // fewer accessible bytes could drive an out-of-bounds read.
    // The new contract never dereferences a foreign pointer
    // before the size is known.
    let size_fn = match find_size_symbol(&lib) {
        Some(p) => p,
        None => {
            if find_query_symbol(&lib).is_none() && find_legacy_desc_symbol(&lib).is_none() {
                return Err(migration_diagnostic(path));
            }
            return Err(format!(
                "{}: cdylib does not export the size-safe descriptor query ABI; \
                 rebuild the script against the current renzora_plugin",
                path.display(),
            ));
        }
    };
    let query_fn = match find_query_symbol(&lib) {
        Some(p) => p,
        None => return Err(migration_diagnostic(path)),
    };

    let descriptor = unsafe {
        renzora_plugin::script::compiled::query_descriptor(
            size_fn,
            query_fn,
            std::mem::size_of::<CompiledScriptDesc>(),
        )
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;

    let generation_arc = match observer {
        Some(obs) => Arc::new(ScriptGeneration::new_with_observer(
            descriptor.entry,
            Box::new(lib) as Box<dyn std::any::Any + Send + Sync>,
            generation,
            obs,
        )),
        None => Arc::new(ScriptGeneration::new(
            descriptor.entry,
            Box::new(lib) as Box<dyn std::any::Any + Send + Sync>,
            generation,
        )),
    };
    Ok(generation_arc)
}

/// The diagnostic a script that still uses the old
/// `renzora::script!` macro receives when its cdylib is loaded. The
/// Phase 4 wiring never produces one in production — a Phase 3
/// authored file still emits the old symbol, but the Phase 1
/// recogniser has been updated to reject it (see `discovery.rs`).
///
/// `pub` so tests and the migration docs can render the exact text.
pub fn migration_diagnostic(path: &Path) -> String {
    format!(
        "{}: rust script cdylib exports no descriptor symbol. \
         The Phase 3 `renzora::script!(update)` macro emitted a \
         `fn(&mut World, Entity)` Rust-ABI entry that Phase 4 has \
         retired. Compile-time Bevy access moved to the restart-required \
         engine-plugin tier. Update the script to use \
         `renzora_plugin::rust_script!(update)` and \
         `fn update(ctx: &renzora_plugin::script::Ctx, reply: &mut \
         renzora_plugin::script::ScriptReply) -> Result<(), String>`.",
        path.display()
    )
}

/// Find the [`CompiledScriptDesc`] in `lib`. The macro emits the
/// descriptor as a `pub extern "C" fn` shim so the symbol is
/// unambiguously exported under its fixed C identifier regardless of
/// module path; the function returns a pointer to the descriptor,
/// which the host dereferences.
fn find_descriptor_symbol(lib: &Library) -> Option<*const CompiledScriptDesc> {
    type Shim = unsafe extern "C" fn() -> *const CompiledScriptDesc;
    let p = unsafe { lib.get::<Shim>(crate::backend::SCRIPT_DESCRIPTOR_SYMBOL) }.ok()?;
    Some(unsafe { (*p)() })
}

/// Find the descriptor-size symbol exported by the new (T4-3) ABI.
/// The cdylib emits `renzora_plugin_tier1_script_descriptor_size` as
/// `unsafe extern "C" fn() -> u32` returning the descriptor's
/// declared size.
fn find_size_symbol(lib: &Library) -> Option<renzora_plugin::script::compiled::DescriptorSizeFn> {
    let p = unsafe {
        lib.get::<renzora_plugin::script::compiled::DescriptorSizeFn>(
            b"renzora_plugin_tier1_script_descriptor_size\0",
        )
    }
    .ok()?;
    Some(*p)
}

/// Find the descriptor-query symbol exported by the new (T4-3) ABI.
/// The cdylib emits `renzora_plugin_tier1_script_query_desc` as
/// `unsafe extern "C" fn(*mut u8, u32, u32) -> u32` writing the
/// descriptor into a host-owned buffer.
fn find_query_symbol(lib: &Library) -> Option<renzora_plugin::script::compiled::QueryDescFn> {
    let p = unsafe {
        lib.get::<renzora_plugin::script::compiled::QueryDescFn>(
            b"renzora_plugin_tier1_script_query_desc\0",
        )
    }
    .ok()?;
    Some(*p)
}

/// Find the legacy (pre-T4-3) descriptor pointer symbol. Used only
/// to produce a clear migration diagnostic when a cdylib was
/// compiled against the old ABI but no longer exports the new
/// query ABI either.
fn find_legacy_desc_symbol(lib: &Library) -> Option<*const CompiledScriptDesc> {
    find_descriptor_symbol(lib)
}

/// Phase 4 declaration recognition, backed by `rustc_lexer` 0.1.0.
/// Returns `true` when the file source declares a Tier 1 script via
/// `renzora_plugin::rust_script!(...)` somewhere the lexer treats as
/// code (not a comment, string, byte string, raw string, char
/// literal, or identifier continuation). Truncated source returns
/// `false` and never panics.
///
/// A source that still uses the legacy `renzora::script!(...)` form
/// is NOT classified as a script — [`declaration_legacy`] detects
/// those so the editor can surface the migration diagnostic, but
/// [`declaration_recognised`] is the gate the build pipeline uses.
pub fn declaration_recognised(source: &str) -> bool {
    matches!(
        renzora_identity::Recogniser::new().scan(source),
        renzora_identity::Declaration::Recognised,
    )
}

/// True when the file source declares a script via the legacy
/// `renzora::script!(...)` macro form. Phase 4 surfaces this as a
/// migration diagnostic — the source is NEVER compiled through the
/// unsafe old ABI; the editor tells the author to switch to
/// `renzora_plugin::rust_script!`.
pub fn declaration_legacy(source: &str) -> bool {
    matches!(
        renzora_identity::Recogniser::new().scan(source),
        renzora_identity::Declaration::LegacyRecognised,
    )
}

/// `declares_script` kept here as a thin wrapper for backward-
/// compatibility with `crates/renzora_plugin_build` callers that
/// still expect it. The real declaration test lives in the watcher
/// / discovery module.
#[allow(dead_code)]
fn declares_script(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    declaration_recognised(&text)
}

/// Backwards-compatible alias for the production discovery walker.
/// Used by the copy-based exporter.
#[deprecated(note = "use `crate::discovery::collect_rust_scripts`")]
pub fn collect_project_scripts(project: &Path) -> Vec<PathBuf> {
    crate::discovery::collect_rust_scripts(project)
}

// Silence unused-import warnings when `static_scripts` is off.
#[allow(unused_imports)]
use Mutex as _Mutex;
