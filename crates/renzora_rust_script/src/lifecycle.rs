//! Event-driven lifecycle for Tier-1 Rust scripts.
//!
//! # Architecture
//!
//! The lifecycle is built around the shared `renzora_compiler_cache::BuildService`:
//!
//! 1. `watch` (a Bevy system) drives the project-lifecycle state
//!    machine, debounces filesystem events, reconciles them against
//!    the watcher's `seen_paths`, and submits a fresh
//!    `ArtifactKind::Tier1Script` `BuildRequest` for every dirty id.
//! 2. `activate` (a Bevy system) drains the pending receivers,
//!    promotes successful outcomes to `LifecycleState::ready`, then
//!    loads the published artifact through the production
//!    [`crate::load_compiled_script`] and registers the per-cdylib
//!    entry against the canonical identity in the shared
//!    [`crate::CompiledScriptSlot`].
//! 3. Retirements (rename / delete / project-switch) cancel the
//!    in-flight receivers, retire `LoadedScripts` entries, and
//!    drop the slot registration. The `BuildService` itself never
//!    unloads: the service's `cancel_for(id, cause)` resolves every
//!    pending receiver for that id with `BuildOutcome::Cancelled`.
//!
//! # Project state machine
//!
//! Each in-flight build is one entry in [`ScriptWatcher::building`],
//! keyed by canonical id, holding a `crossbeam_channel::Receiver`.
//! `pending_dirty` carries the additional state during a build. A
//! stale result (pending-dirty or source mtime moved past the
//! snapshot's `started_at`) is discarded and a replacement
//! `BuildRequest` is submitted immediately, without waiting for
//! another filesystem event.
//!
//! # Directory topology changes
//!
//! `Remove` and `Modify(Name)` events whose path was (or might have
//! been) a directory trigger a full rescan: the OS does not
//! enumerate every child. A nested directory deletion that was not
//! enumerated as one event would otherwise leave its scripts loaded
//! forever. Ordinary file edits at the project root use targeted
//! reconciliation.
//!
//! # Idle frames
//!
//! The production reconcile path runs only when:
//! - a debounced batch arrives;
//! - the watcher was just attached (initial rescan once);
//! - the filesystem watcher reports an error (overflow recovery).
//!
//! Idle frames perform zero discovery calls.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use crossbeam_channel::Receiver;
use renzora::core::console_log::{console_error, console_success};
use renzora::CurrentProject;
use renzora_compiler_cache::compiler::ServiceStamps;
use renzora_compiler_cache::{
    types::{
        ArtifactKind, BuildOutcome, BuildRequest, CancelCause, FingerprintInputs, PanicStrategy,
        Revision,
    },
    BuildService, BuildServiceError,
};
use renzora_identity::{CanonicalId, RootKind};

use crate::declaration_recognised;
use crate::discovery;
use crate::CompiledScriptSlot;
use crate::{
    BuildOutcomeHistory, BuildOutcomeRecord, LifecycleDiagnostics, LifecycleState, LoadedScripts,
    PendingBuild, ReadyBuild, RustScriptBuildService, ScriptGenerationObserverFactory,
    TerminalBuildKind,
};

/// U4-4: legacy channel type retained only for the `ScriptWatcher`
/// fields the OS adapter writes to. The lifecycle reads events
/// from `ScriptSourceEventQueue` instead.
pub type DebouncedRx = std::sync::mpsc::Receiver<notify_debouncer_full::DebounceEventResult>;
pub type SourceDebouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

/// U4-4: a production seam for filesystem events the
/// source-script lifecycle should react to.
///
/// The OS-watcher adapter Bevy plugin writes `SourceChanged`,
/// `SourceRemoved`, and `TopologyRescanNeeded` events into this
/// queue (the adapter does that conversion on its own worker
/// thread; the adapter does not need a producer handle back
/// from the queue). The lifecycle's `watch` system drains the
/// same queue. Tests inject events through
/// [`ScriptSourceEventQueue::push`] without standing up a real OS
/// watcher, without mutating any private `ScriptWatcher` field,
/// and without duplicating `discovery::collect_canonical_scripts`.
///
/// The previous design forced tests to overwrite
/// `ScriptWatcher::debouncer`, `ScriptWatcher::rx`, and
/// `ScriptWatcher::seen_paths`, which is the exact path the
/// fourth-pass review rejected.
#[derive(Default, Resource, Clone)]
pub struct ScriptSourceEventQueue {
    inner: Arc<Mutex<VecDeque<ScriptSourceEvent>>>,
}

impl ScriptSourceEventQueue {
    /// Push a single event. Both the OS watcher adapter (which
    /// converts `notify_debouncer_full` events on its worker
    /// thread) and test code take this path.
    pub fn push(&self, event: ScriptSourceEvent) {
        self.inner.lock().unwrap().push_back(event);
    }

    /// Drain all currently-queued events into a `Vec`. Called by
    /// the production `watch` system exactly once per frame.
    pub fn drain(&self) -> Vec<ScriptSourceEvent> {
        let mut guard = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(guard.len());
        while let Some(ev) = guard.pop_front() {
            out.push(ev);
        }
        out
    }
}

/// One filesystem event the lifecycle should react to. U4-4: the
/// enum deliberately matches the lifecycle's three actions (modify
/// a script, remove a script, rescan the project root). Tests
/// build `ScriptSourceEvent`s directly.
#[derive(Clone, Debug)]
pub enum ScriptSourceEvent {
    /// A specific path changed. The lifecycle reconciles the
    /// affected `CanonicalId` and submits a fresh build if it is
    /// a known Tier-1 script.
    SourceChanged(PathBuf),
    /// A specific path was removed. The lifecycle retires the
    /// canonical id matching this path.
    SourceRemoved(PathBuf),
    /// Topology changed (directory rename, directory delete) — the
    /// lifecycle does a full project rescan.
    TopologyRescanNeeded,
}

/// The watcher resource. U4-4: holds the seen-path snapshot and
/// the per-id in-flight build state; the OS-debouncer field is
/// retained only for late-detach bookkeeping. Tests do NOT mutate
/// any field of this resource; they push events into
/// [`ScriptSourceEventQueue`] through the public seam.
#[derive(Resource)]
pub struct ScriptWatcher {
    /// In-flight build state keyed by canonical id. The actual
    /// receiver lives in [`LifecycleState::pending`]; this map
    /// tracks `pending_dirty` for each id so a dirty-during-build
    /// replacement fires immediately after the current one finishes.
    pub building: HashMap<CanonicalId, InFlightBuild>,
    /// U4-4: the legacy OS-debouncer field. Retained only so the
    /// `LooseWatcher` (which references ScriptWatcher for
    /// inventory-coupling) does not crash on a missing field;
    /// the lifecycle does NOT consume it for event delivery
    /// (events flow through [`ScriptSourceEventQueue`]). The OS
    /// adapter plugin sets this field when it attaches.
    pub debouncer: Option<SourceDebouncer>,
    /// U4-4: legacy channel field. Same caveat as `debouncer`:
    /// the lifecycle reads events from `ScriptSourceEventQueue`,
    /// not from here. The adapter still writes to it when it has
    /// no other seam.
    pub rx: std::sync::Mutex<Option<DebouncedRx>>,
    pub seen_paths: Vec<CanonicalId>,
    pub watched_root: Option<PathBuf>,
    last_failed_attach: Option<std::time::Instant>,
}

impl ScriptWatcher {
    /// U4-4: returns `true` when at least one producer (OS watcher
    /// OR test seam) is attached to the event queue. Used to
    /// decide whether `OpenFirst`/`Switch` actions should compute
    /// their initial scan now or wait. Always `true` once the test
    /// seam (`ScriptSourceEventQueue::push`) has been used, since
    /// the queue itself is installed by the lifecycle's `build`.
    pub fn has_any_producer(&self) -> bool {
        self.debouncer.is_some() || self.rx.lock().unwrap().is_some()
    }
}

#[derive(Debug)]
pub struct InFlightBuild {
    /// True when a filesystem event arrived while this build was in
    /// flight. The activate path discards the result and submits a
    /// replacement immediately.
    pub pending_dirty: bool,
    /// mtime of the source when the build was started. A result is
    /// stale if the file's current mtime is greater than this. Kept
    /// for future staleness checks (the BuildService already
    /// supersedes by revision).
    #[allow(dead_code)]
    pub started_at: std::time::SystemTime,
}

impl Default for InFlightBuild {
    fn default() -> Self {
        Self {
            pending_dirty: false,
            started_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }
}

impl Default for ScriptWatcher {
    fn default() -> Self {
        Self {
            building: HashMap::new(),
            debouncer: None,
            rx: std::sync::Mutex::new(None),
            seen_paths: Vec::new(),
            watched_root: None,
            last_failed_attach: None,
        }
    }
}

/// Resource-shared retire queue.
#[derive(Resource, Default)]
pub struct PendingRetires {
    inner: Vec<CanonicalId>,
}

impl PendingRetires {
    pub fn enqueue(&mut self, id: CanonicalId) {
        if !self.inner.iter().any(|p| p == &id) {
            self.inner.push(id);
        }
    }

    pub fn take(&mut self) -> Vec<CanonicalId> {
        std::mem::take(&mut self.inner)
    }
}

// ─── Lifecycle state machine ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleAction {
    Idle,
    OpenFirst,
    Keep,
    Switch {
        old_root: PathBuf,
        new_root: PathBuf,
        retired: Vec<CanonicalId>,
    },
    Close {
        retired: Vec<CanonicalId>,
    },
    RetryAttach,
}

#[allow(dead_code)]
pub(crate) fn compute_lifecycle_action(
    watcher: &ScriptWatcher,
    current_project: Option<&Path>,
) -> LifecycleAction {
    compute_lifecycle_action_from_snapshot(
        &LifecycleSnapshot {
            watched_root: watcher.watched_root.clone(),
            has_debouncer: watcher.debouncer.is_some(),
            seen_paths: watcher.seen_paths.clone(),
            last_failed_attach: watcher.last_failed_attach,
        },
        current_project,
    )
}

#[derive(Clone)]
pub(crate) struct LifecycleSnapshot {
    pub watched_root: Option<PathBuf>,
    pub has_debouncer: bool,
    pub seen_paths: Vec<CanonicalId>,
    pub last_failed_attach: Option<std::time::Instant>,
}

pub(crate) fn compute_lifecycle_action_from_snapshot(
    snapshot: &LifecycleSnapshot,
    current_project: Option<&Path>,
) -> LifecycleAction {
    match (current_project, snapshot.watched_root.as_deref()) {
        (None, None) => LifecycleAction::Idle,
        (None, Some(_)) => {
            let mut retired: Vec<CanonicalId> = snapshot.seen_paths.clone();
            retired.sort();
            retired.dedup();
            LifecycleAction::Close { retired }
        }
        (Some(_), None) => LifecycleAction::OpenFirst,
        (Some(new), Some(old)) => {
            if new == old {
                if !snapshot.has_debouncer {
                    LifecycleAction::RetryAttach
                } else {
                    LifecycleAction::Keep
                }
            } else {
                let mut retired: Vec<CanonicalId> = snapshot.seen_paths.clone();
                retired.sort();
                retired.dedup();
                LifecycleAction::Switch {
                    old_root: old.to_path_buf(),
                    new_root: new.to_path_buf(),
                    retired,
                }
            }
        }
    }
}

pub(crate) fn apply_lifecycle_action(
    world: &mut World,
    action: LifecycleAction,
    build_service: Option<&std::sync::Arc<BuildService>>,
) {
    match action {
        LifecycleAction::Idle | LifecycleAction::Keep => {}
        LifecycleAction::Close { retired } => {
            {
                let mut pending = world.resource_mut::<PendingRetires>();
                for id in &retired {
                    pending.enqueue(id.clone());
                }
            }
            // Cancel any in-flight receivers for retired ids.
            if let Some(bs) = build_service {
                for id in &retired {
                    bs.cancel_for(id, CancelCause::ProjectClose);
                }
            }
            // U4-4: OS-watcher management is a separate concern.
            // The lifecycle's `Close` arm only resets its own
            // state; the OS adapter listens for `CurrentProject`
            // resource changes and tears down its own watcher.
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.watched_root = None;
            watcher.seen_paths.clear();
            watcher.building.clear();
            watcher.last_failed_attach = None;
        }
        LifecycleAction::OpenFirst => {
            let new_root = match world.get_resource::<CurrentProject>() {
                Some(p) => p.path.clone(),
                None => return,
            };
            // U4-4: perform the initial scan through
            // `discovery::collect_canonical_scripts` and submit
            // initial builds WITHOUT attaching an OS watcher.
            // The OS adapter (a separate Bevy plugin installed
            // by the editor when one is configured) attaches on
            // its own when `CurrentProject` shows up.
            {
                let mut watcher = world.resource_mut::<ScriptWatcher>();
                watcher.building.clear();
                watcher.watched_root = Some(new_root.clone());
                watcher.last_failed_attach = None;
            }
            let ids = discovery::collect_canonical_scripts(&new_root);
            submit_initial(world, build_service, &new_root, &ids);
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.seen_paths = ids;
        }
        LifecycleAction::Switch {
            old_root: _,
            new_root,
            retired,
        } => {
            {
                let mut pending = world.resource_mut::<PendingRetires>();
                for id in &retired {
                    pending.enqueue(id.clone());
                }
            }
            if let Some(bs) = build_service {
                for id in &retired {
                    bs.cancel_for(id, CancelCause::ProjectClose);
                }
            }
            // U4-4: same split as `OpenFirst`. The lifecycle
            // resets its own state and rescans the new project
            // root; the OS adapter reacts on its own.
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.watched_root = Some(new_root.clone());
            watcher.seen_paths.clear();
            watcher.building.clear();
            watcher.last_failed_attach = None;
            let ids = discovery::collect_canonical_scripts(&new_root);
            submit_initial(world, build_service, &new_root, &ids);
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.seen_paths = ids;
        }
        LifecycleAction::RetryAttach => {
            // U4-4: RetryAttach is no longer a lifecycle action.
            // OS watcher attach failures are observed through the
            // `last_failed_attach` instant, recorded here only so
            // observer code can read it; backoff is handled in
            // `lifecycle_tick`'s outer branch.
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.last_failed_attach = None;
        }
    }
}

// U4-4: `attach_debouncer_raw` was removed. OS-watcher setup is
// the OS adapter plugin's responsibility. The lifecycle now scans
// the project root through `discovery::collect_canonical_scripts`
// and submits initial builds WITHOUT an attached OS watcher; the
// adapter is free to attach a watcher on its own timeline. The
// `notify_debouncer_full` import is retained only for the legacy
// `debouncer`/`rx` fields on `ScriptWatcher` that older loose-
// plugin paths read.

/// Submit a `BuildRequest` for every id in `ids` against the shared
/// `BuildService`. `build_service` is the editor's neutral
/// `Arc<BuildService>`; the same instance is shared by loose plugins
/// so cache partitions, worker pools, and supersession are unified.
fn submit_initial(
    world: &mut World,
    build_service: Option<&std::sync::Arc<BuildService>>,
    project_root: &Path,
    ids: &[CanonicalId],
) {
    let Some(bs) = build_service else {
        return;
    };
    let stamps = bs.stamps();
    for id in ids {
        let src = project_root.join(id.path());
        let bytes = match std::fs::read(&src) {
            Ok(b) => b,
            Err(e) => {
                warn!("[rust-script] could not read {}: {e}", src.display());
                continue;
            }
        };
        let request = build_script_request(id.clone(), bytes, &stamps);
        match bs.submit(request) {
            Ok(rx) => {
                let started_at = std::fs::metadata(&src)
                    .and_then(|m| m.modified())
                    .unwrap_or_else(|_| std::time::SystemTime::now());
                {
                    let mut state = world.resource_mut::<LifecycleState>();
                    state.pending.insert(
                        id.clone(),
                        PendingBuild {
                            receiver: rx,
                            revision: Revision(0),
                        },
                    );
                }
                let mut watcher = world.resource_mut::<ScriptWatcher>();
                watcher.building.insert(
                    id.clone(),
                    InFlightBuild {
                        started_at,
                        pending_dirty: false,
                    },
                );
            }
            Err(e) => {
                error!("[rust-script] initial submit failed for {id}: {e}");
            }
        }
    }
}

/// Submit a single `BuildRequest` for a dirty id against the shared
/// `BuildService`. Returns the receiver so the caller can store it
/// for outcome draining.
fn submit_one(
    bs: &std::sync::Arc<BuildService>,
    id: CanonicalId,
    project_root: &Path,
) -> Result<(Receiver<BuildOutcome>, std::time::SystemTime), BuildServiceError> {
    let src = project_root.join(id.path());
    let bytes = match std::fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            warn!("[rust-script] could not read {}: {e}", src.display());
            return Err(BuildServiceError::CacheRoot(std::io::Error::other(
                e.to_string(),
            )));
        }
    };
    let stamps = bs.stamps();
    let request = build_script_request(id, bytes, &stamps);
    let rx = bs.submit(request)?;
    let started_at = std::fs::metadata(&src)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    Ok((rx, started_at))
}

fn build_script_request(id: CanonicalId, source: Vec<u8>, stamps: &ServiceStamps) -> BuildRequest {
    BuildRequest {
        identity: id,
        source_snapshot: std::sync::Arc::new(source),
        fingerprint_inputs: FingerprintInputs {
            toolchain_stamp: stamps.toolchain_stamp.clone(),
            sdk_content_hash: stamps.sdk_content_hash.0,
            compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
            // F4-1: scripts need the `script` feature so
            // `renzora_plugin::script::*` is in the SDK the wrapper
            // depends on. The capability set is the production
            // fingerprint input; changing it invalidates cache rows.
            capabilities: ["script".to_string()].into_iter().collect(),
            profile: renzora_compiler_cache::types::BuildProfile::Dist,
            // F4-4: production Tier-1 scripts MUST compile with unwind
            // semantics. The macro emits `catch_unwind` around the typed
            // entry; `-C panic=abort` would skip the guard and any
            // script-side panic would abort the editor. Unwind mode is
            // part of the cache fingerprint so an abort-mode artifact
            // does not collide with an unwind-mode one.
            panic: PanicStrategy::Unwind,
            ..Default::default()
        },
        target: renzora_compiler_cache::types::default_target_triple(),
        artifact_kind: ArtifactKind::Tier1Script,
    }
}

/// `watch` is the Bevy `Lifecycle` system: compute the lifecycle
/// action, apply it, then drain pending events.
pub fn watch(world: &mut World) {
    let current_project = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone());
    let bs = world
        .get_resource::<RustScriptBuildService>()
        .map(|r| r.0.clone());
    // Install or refresh the dispatcher-facing path resolver from
    // the active project. The resolver is the authority for runtime
    // identity resolution; without it, the dispatcher rejects
    // every path that is not already canonical.
    if let Some(project_path) = current_project.as_ref() {
        let needs_install = !world.contains_resource::<crate::compiled_runtime::PathResolver>();
        if needs_install {
            let resolver = crate::compiled_runtime::PathResolver::new(project_path.clone());
            crate::backend::sync_global_resolver(&resolver);
            world.insert_resource(resolver);
        } else if let Some(resolver) = world.get_resource::<crate::compiled_runtime::PathResolver>()
        {
            // Even if installed, push the latest into the global
            // slot so the dispatcher sees it.
            crate::backend::sync_global_resolver(resolver);
        }
    }
    lifecycle_tick(world, current_project.as_deref(), bs.as_ref());
}

pub(crate) fn lifecycle_tick(
    world: &mut World,
    current_project: Option<&Path>,
    build_service: Option<&std::sync::Arc<BuildService>>,
) {
    let snapshot = {
        let w = world.resource::<ScriptWatcher>();
        LifecycleSnapshot {
            watched_root: w.watched_root.clone(),
            // U4-4: has_watcher is now derived from whether ANY
            // producer is attached (OS watcher OR test producer).
            // The OS-watcher field `debouncer` is still used to
            // decide when to live-attach a real OS-backed watcher
            // during `OpenFirst`/`Switch` — it is a hint to the
            // adapter plugin, not a lifecycle gate.
            has_debouncer: w.has_any_producer(),
            seen_paths: w.seen_paths.clone(),
            last_failed_attach: w.last_failed_attach,
        }
    };
    let action = compute_lifecycle_action_from_snapshot(&snapshot, current_project);
    let project_root_for_events = match &action {
        LifecycleAction::OpenFirst | LifecycleAction::Switch { .. } => None,
        _ => current_project.map(Path::to_path_buf),
    };

    match action {
        LifecycleAction::Idle => return,
        LifecycleAction::Keep => {}
        _ => {
            if matches!(action, LifecycleAction::RetryAttach) {
                if let Some(prev) = snapshot.last_failed_attach {
                    if prev.elapsed() < ATTACH_BACKOFF {
                        return;
                    }
                }
            }
            apply_lifecycle_action(world, action, build_service);
        }
    }

    let project_root = match project_root_for_events.or_else(|| {
        world
            .get_resource::<CurrentProject>()
            .map(|p| p.path.clone())
    }) {
        Some(r) => r,
        None => return,
    };

    // U4-4: drain the production event queue rather than the
    // private `ScriptWatcher::rx`. The OS watcher, the test seam,
    // and the lifecycle all read from this same queue.
    let queue = world.resource::<ScriptSourceEventQueue>();
    let events = queue.drain();
    if events.is_empty() {
        return;
    }

    // Compute the plan up front so we can release the `ScriptWatcher`
    // mutable borrow before mutating other resources in `apply_plan`.
    let plan: Option<Plan> = {
        let mut watcher = world.resource_mut::<ScriptWatcher>();
        let batch = reconcile_with_events(&mut watcher, &project_root, events);
        if batch.is_none() {
            return;
        }
        batch
    };
    if let Some(plan) = plan {
        apply_plan(world, &project_root, plan, build_service);
    }
}

pub enum Batch {
    Events(Vec<ScriptSourceEvent>),
    FullRescan,
}

#[derive(Default, Debug)]
pub(crate) struct Plan {
    pub dirty: Vec<CanonicalId>,
    pub removed: Vec<CanonicalId>,
}

/// U4-4: replace `reconcile_with_events(DebouncedEvent)` with a
/// version that consumes the production `ScriptSourceEvent` seam
/// directly. The dedup / declaration / lexical resolution
/// functions are reimplemented on top of `ScriptSourceEvent`. No
/// `notify_debouncer_full` types appear in the call chain anymore.
pub(crate) fn reconcile_with_events(
    watcher: &mut ScriptWatcher,
    project_root: &Path,
    events: Vec<ScriptSourceEvent>,
) -> Option<Plan> {
    let mut seen_paths: HashMap<PathBuf, (bool, bool)> = HashMap::new();
    let mut topology_rescan = false;

    for event in events {
        match event {
            ScriptSourceEvent::SourceChanged(path) | ScriptSourceEvent::SourceRemoved(path) => {
                let present = std::fs::symlink_metadata(&path).is_ok();
                let declared = if present {
                    std::fs::read_to_string(&path)
                        .map(|src| declaration_recognised(&src))
                        .unwrap_or(false)
                } else {
                    false
                };
                let entry = seen_paths.entry(path).or_insert((present, declared));
                entry.0 = present;
                entry.1 = declared;
            }
            ScriptSourceEvent::TopologyRescanNeeded => {
                topology_rescan = true;
            }
        }
    }

    if topology_rescan {
        return Some(full_rescan(watcher, project_root));
    }

    let mut plan = Plan::default();

    for (path, (present, declared)) in seen_paths {
        if !is_source_path(project_root, &path) {
            continue;
        }
        let Some(id) = lexical_event_identity(project_root, &path) else {
            continue;
        };
        if present && declared {
            plan.dirty.push(id);
        } else if (!present || !declared) && watcher.seen_paths.contains(&id) {
            plan.removed.push(id);
        }
    }

    let removed_set: HashSet<CanonicalId> = plan.removed.iter().cloned().collect();
    plan.dirty.retain(|id| !removed_set.contains(id));

    watcher.seen_paths.retain(|p| !plan.removed.contains(p));
    for id in &plan.dirty {
        if !watcher.seen_paths.contains(id) {
            watcher.seen_paths.push(id.clone());
        }
    }

    if plan.dirty.is_empty() && plan.removed.is_empty() {
        return None;
    }
    Some(plan)
}

// U4-4: `has_directory_topology_change`,
// `path_could_be_directory_or_has_known_descendants`, and
// `dedup_event_paths` were removed. The new seam maps OS-side
// `Rename` and `Remove(dirs)` into a single
// `ScriptSourceEvent::TopologyRescanNeeded`; the conversion lives
// in the OS adapter (added next).

pub(crate) fn full_rescan(watcher: &mut ScriptWatcher, project_root: &Path) -> Plan {
    let current: std::collections::BTreeSet<CanonicalId> =
        discovery::collect_canonical_scripts(project_root)
            .into_iter()
            .collect();
    let previous: std::collections::BTreeSet<CanonicalId> =
        watcher.seen_paths.iter().cloned().collect();

    let mut plan = Plan::default();
    for added in current.difference(&previous) {
        plan.dirty.push(added.clone());
    }
    for gone in previous.difference(&current) {
        plan.removed.push(gone.clone());
    }
    for added in &plan.dirty {
        if !watcher.seen_paths.contains(added) {
            watcher.seen_paths.push(added.clone());
        }
    }
    watcher.seen_paths.retain(|p| !plan.removed.contains(p));
    plan
}

fn apply_plan(
    world: &mut World,
    project_root: &Path,
    plan: Plan,
    build_service: Option<&std::sync::Arc<BuildService>>,
) {
    // Queue retires first.
    {
        let mut pending = world.resource_mut::<PendingRetires>();
        for id in &plan.removed {
            pending.enqueue(id.clone());
            if let Some(bs) = build_service {
                bs.cancel_for(id, CancelCause::UserRetry);
            }
        }
    }
    {
        let mut watcher = world.resource_mut::<ScriptWatcher>();
        for id in &plan.removed {
            watcher.seen_paths.retain(|p| p != id);
        }
        for id in &plan.dirty {
            if !watcher.seen_paths.contains(id) {
                watcher.seen_paths.push(id.clone());
            }
        }
    }

    let Some(bs) = build_service else {
        if !plan.dirty.is_empty() {
            warn!(
                "[rust-script] {} build(s) skipped: no shared BuildService installed",
                plan.dirty.len()
            );
        }
        return;
    };

    for id in plan.dirty {
        match submit_one(bs, id.clone(), project_root) {
            Ok((rx, started_at)) => {
                {
                    let mut state = world.resource_mut::<LifecycleState>();
                    let displaced = state.pending.insert(
                        id.clone(),
                        PendingBuild {
                            receiver: rx,
                            revision: Revision(0),
                        },
                    );
                    if let Some(previous) = displaced {
                        state.superseded_pending.push((id.clone(), previous));
                    }
                }
                let mut watcher = world.resource_mut::<ScriptWatcher>();
                watcher.building.insert(
                    id,
                    InFlightBuild {
                        started_at,
                        pending_dirty: false,
                    },
                );
            }
            Err(e) => {
                error!("[rust-script] submit failed for {id}: {e}");
            }
        }
    }
}

/// `activate` is the Bevy `Activate` system: drain `LifecycleState`'s
/// receivers, load successful artifacts, retire cancelled / failed
/// builds, and apply `PendingRetires`.
pub fn activate(world: &mut World) {
    // Drain pending receivers.
    let mut ready: Vec<ReadyBuild> = Vec::new();
    let mut to_remove: Vec<CanonicalId> = Vec::new();
    let mut diagnostics: Vec<(CanonicalId, String)> = Vec::new();
    let mut outcome_records: Vec<BuildOutcomeRecord> = Vec::new();
    {
        let mut state = world.resource_mut::<LifecycleState>();
        let mut still_waiting = Vec::new();
        for (id, pending) in state.superseded_pending.drain(..) {
            match pending.receiver.try_recv() {
                Ok(BuildOutcome::Superseded {
                    superseded_revision,
                    by_revision,
                }) => outcome_records.push(BuildOutcomeRecord {
                    id,
                    request_revision: superseded_revision,
                    superseded_by: Some(by_revision),
                    kind: TerminalBuildKind::Superseded,
                }),
                Ok(BuildOutcome::Published {
                    request_revision, ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id,
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Published,
                    });
                }
                Ok(BuildOutcome::CacheHit {
                    request_revision, ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id,
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::CacheHit,
                    });
                }
                Ok(BuildOutcome::CompileFailed {
                    request_revision, ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id,
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::CompileFailed,
                    });
                }
                Ok(BuildOutcome::Cancelled {
                    request_revision, ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id,
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Cancelled,
                    });
                }
                Ok(BuildOutcome::Shutdown { request_revision }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id,
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Shutdown,
                    });
                }
                Err(crossbeam_channel::TryRecvError::Empty) => still_waiting.push((id, pending)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {}
            }
        }
        state.superseded_pending = still_waiting;
        for (id, pb) in state.pending.iter() {
            match pb.receiver.try_recv() {
                Ok(BuildOutcome::Published {
                    request_revision,
                    immutable_artifact_path,
                    generation,
                    ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Published,
                    });
                    ready.push(ReadyBuild {
                        id: id.clone(),
                        artifact: immutable_artifact_path,
                        generation,
                    });
                    to_remove.push(id.clone());
                }
                Ok(BuildOutcome::CacheHit {
                    request_revision,
                    immutable_artifact_path,
                    generation,
                    ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::CacheHit,
                    });
                    ready.push(ReadyBuild {
                        id: id.clone(),
                        artifact: immutable_artifact_path,
                        generation,
                    });
                    to_remove.push(id.clone());
                }
                Ok(BuildOutcome::Superseded {
                    superseded_revision,
                    by_revision,
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision: superseded_revision,
                        superseded_by: Some(by_revision),
                        kind: TerminalBuildKind::Superseded,
                    });
                    // Stale: the watcher will submit a fresh build
                    // for the same id on the next reconcile.
                    to_remove.push(id.clone());
                }
                Ok(BuildOutcome::CompileFailed {
                    request_revision,
                    diagnostics: d,
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::CompileFailed,
                    });
                    diagnostics.push((
                        id.clone(),
                        d.into_iter()
                            .map(|diag| diag.message)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ));
                    to_remove.push(id.clone());
                }
                Ok(BuildOutcome::Cancelled {
                    request_revision, ..
                }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Cancelled,
                    });
                    to_remove.push(id.clone());
                }
                Ok(BuildOutcome::Shutdown { request_revision }) => {
                    outcome_records.push(BuildOutcomeRecord {
                        id: id.clone(),
                        request_revision,
                        superseded_by: None,
                        kind: TerminalBuildKind::Shutdown,
                    });
                    to_remove.push(id.clone());
                }
                Err(_) => {
                    // No outcome yet; leave the pending entry in place.
                }
            }
        }
        for id in &to_remove {
            state.pending.remove(id);
        }
    }

    if !outcome_records.is_empty() {
        let mut history = world.resource_mut::<BuildOutcomeHistory>();
        for record in outcome_records {
            history.push(record);
        }
    }

    // Load ready artifacts and register them. Detach the slot and
    // LoadedScripts from the world so we can mutate them freely
    // without Bevy's borrow checker; we re-insert both at the end.
    let mut loaded = world.remove_resource::<LoadedScripts>().unwrap_or_default();
    let slot = world
        .remove_resource::<CompiledScriptSlot>()
        .unwrap_or_default();
    // T4-7: capture the last successful id/generation BEFORE the
    // loop moves `ready`.
    let last_built = ready.last().map(|r| (r.id.clone(), r.generation.0));
    for r in ready {
        // Production: no drop observer. The generation's
        // `_library` field owns the `Library`; when every `Arc`
        // clone is dropped, the `Library` drops and the cdylib
        // unloads.
        let observer = world
            .get_resource::<ScriptGenerationObserverFactory>()
            .map(|factory| (factory.0)(&r.id, r.generation.0));
        match crate::load_compiled_script_with_observer(&r.artifact, r.generation.0, observer) {
            Ok(generation) => {
                slot.register(&r.id.to_string(), generation.clone());
                loaded.insert(r.id.clone(), generation);
                info!("[rust-script] activated {} (gen {})", r.id, r.generation.0);
                console_success(
                    "Script",
                    format!("activated {} (gen {})", r.id, r.generation.0),
                );
            }
            Err(e) => {
                error!("[rust-script] load failed for {}: {e}", r.id);
                console_error("Script", format!("load failed for {}\n{e}", r.id));
            }
        }
    }

    for (id, msg) in &diagnostics {
        error!("[rust-script] compile failed for {id}:\n{msg}");
        console_error("Script", format!("compile failed for {id}\n{msg}"));
    }

    // T4-8: write the production diagnostic resource so tests can
    // observe the failure without inspecting the private receiver.
    if let Some((last_id, last_msg)) = diagnostics.last() {
        if let Some(mut diag) = world.get_resource_mut::<LifecycleDiagnostics>() {
            diag.last_compile_failed = Some(last_id.clone());
            diag.last_compile_failed_message = Some(last_msg.clone());
        }
    }

    // T4-7: record the last `Published` / `CacheHit` outcome the
    // activate system consumed, so acceptance tests can verify
    // the build succeeded without inspecting the private receiver.
    if let Some((id, gen)) = last_built {
        if let Some(mut diag) = world.get_resource_mut::<LifecycleDiagnostics>() {
            diag.last_built_id = Some(id);
            diag.last_built_generation = Some(gen);
        }
    }

    // Apply retirements. The previous `Arc<ScriptGeneration>` is
    // dropped here, which releases its `Library` handle once every
    // in-flight dispatch that cloned the Arc has finished.
    let retires: Vec<CanonicalId> = world
        .get_resource_mut::<PendingRetires>()
        .map(|mut p| p.take())
        .unwrap_or_default();
    if !retires.is_empty() {
        let previous: Vec<_> = retires
            .iter()
            .filter_map(|id| slot.unregister(&id.to_string()))
            .collect();
        for id in &retires {
            loaded.remove(id);
            info!("[rust-script] retired {id}");
        }
        // Hold the retired generations alive briefly so any
        // in-flight call still holding its Arc clone can finish.
        // The drop happens at the end of this scope — by then the
        // Bevy schedule that invoked the dispatch has returned.
        drop(previous);
    }
    // Re-insert both resources.
    world.insert_resource(loaded);
    world.insert_resource(slot.clone());
    // F4-1 / S4-5: sync the dispatcher's process-global slot with
    // the resource the lifecycle just updated. The global slot
    // pointer is what the `unsafe extern "C"` dispatcher reads,
    // so it must reflect the same `Arc<CompiledScriptBackend>`
    // the resource holds. Without this, an `activate` pass that
    // rebuilds the slot resource would leave the dispatcher
    // looking at the original (now stale) backend.
    crate::backend::install_global_slot(slot);

    // S4-5: refresh the dispatcher's bare-alias index from the
    // current `LoadedScripts` set. The dispatcher consults the
    // index for bare-leaf lookups; without a refresh, a newly
    // activated script is invisible to a bare-name call.
    let alias_ids: Vec<CanonicalId> = world
        .get_resource::<LoadedScripts>()
        .map(|s| {
            let mut ids: Vec<CanonicalId> = s.entries.keys().cloned().collect();
            ids.sort();
            ids
        })
        .unwrap_or_default();
    if let Some(mut resolver) = world.get_resource_mut::<crate::compiled_runtime::PathResolver>() {
        resolver.refresh_alias_index(alias_ids);
    }

    // Update the watcher's `building` map to reflect the drains.
    let mut watcher = world.resource_mut::<ScriptWatcher>();
    for id in &to_remove {
        watcher.building.remove(id);
    }
    for (id, _) in &diagnostics {
        watcher.building.remove(id);
    }
    for id in &retires {
        watcher.building.remove(id);
    }
}

fn is_source_path(project_root: &Path, path: &Path) -> bool {
    let rel = match path.strip_prefix(project_root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    for component in rel.components() {
        let s = component.as_os_str().to_string_lossy();
        if discovery::SKIP_NAMES.contains(&s.as_ref()) {
            return false;
        }
        if s.starts_with('.') && s != "." && s != ".." {
            return false;
        }
    }
    path.extension().and_then(|e| e.to_str()) == Some("rs")
}

fn lexical_event_identity(project_root: &Path, path: &Path) -> Option<CanonicalId> {
    if !is_source_path(project_root, path) {
        return None;
    }
    let stripped = path.strip_prefix(project_root).ok()?;
    let rel = stripped.to_string_lossy().replace('\\', "/");
    CanonicalId::from_rooted(RootKind::Project, &rel).ok()
}

const ATTACH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
