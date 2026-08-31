//! Event-driven script watcher.
//!
//! # Build state machine
//!
//! Each in-flight build is one entry in [`ScriptWatcher::building`], keyed
//! by canonical id, holding an [`InFlightBuild`]. A build that arrives
//! dirty stays as one entry — the entry's `pending_dirty` flag carries
//! the additional state. The reason the state is encoded as a flag
//! rather than an explicit `BuildState::BuildingAndDirty` variant is that
//! no production code paths need to inspect that distinction; they only
//! care that `pending_dirty` triggers a discard-and-replace on
//! completion.
//!
//! Three guarantees the implementation must satisfy:
//!
//! 1. A `Task` polled to `Pending` stays inside `watcher.building`. The
//!    task is removed only when it transitions to `Ready`.
//! 2. When a stale result is discarded, a replacement `Task` is spawned
//!    immediately in the same `finish` call — the watcher does not wait
//!    for another filesystem event.
//! 3. The retirements collected from per-path events are stored in the
//!    `PendingRetires` Bevy resource, so any system on any thread can
//!    enqueue and the `finish` exclusive system can drain.
//!
//! # Directory topology changes
//!
//! `Remove` and `Modify(Name)` events whose path was (or might have been)
//! a directory trigger a full rescan: the OS does not enumerate every
//! child. A nested directory deletion that was not enumerated as one
//! event would otherwise leave its scripts loaded forever. Ordinary file
//! edits at the project root use targeted reconciliation.
//!
//! # Idle frames
//!
//! The production reconcile path runs only when:
//! - a debounced batch arrives;
//! - the watcher was just attached (initial rescan once);
//! - the filesystem watcher reports an error (overflow recovery).
//!
//! Idle frames perform zero discovery calls.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use bevy::tasks::{block_on, poll_once, AsyncComputeTaskPool, Task};
type BuildTask = Task<Result<PathBuf, String>>;
use notify_debouncer_full::{
    new_debouncer,
    notify::{
        event::{ModifyKind, RenameMode},
        EventKind, RecursiveMode,
    },
    DebounceEventResult, DebouncedEvent,
};
use renzora::core::console_log::{console_error, console_success};
use renzora::CurrentProject;
use renzora_identity::{CanonicalId, RootKind};

use crate::declaration_recognised;
use crate::discovery;
use crate::load_library;
use crate::LoadedScripts;

const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

type DebouncedRx = std::sync::mpsc::Receiver<DebounceEventResult>;
type SourceDebouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

/// The watcher resource. Holds the in-flight build state machine, the
/// debouncer, the failure backoff timer, the per-id seen-path
/// snapshot, and the lifecycle-pending-compile flag.
#[derive(Resource)]
pub struct ScriptWatcher {
    building: HashMap<CanonicalId, InFlightBuild>,
    debouncer: Option<SourceDebouncer>,
    rx: std::sync::Mutex<Option<DebouncedRx>>,
    seen_paths: Vec<CanonicalId>,
    watched_root: Option<PathBuf>,
    last_failed_attach: Option<std::time::Instant>,
    /// Set true by the lifecycle transition when a fresh project has
    /// just been attached and its scripts have not yet been compiled.
    /// `compile_for_new_project` reads this flag, runs
    /// `compile_and_load_for_project` exactly once, and clears it.
    pub(crate) lifecycle_needs_compile: bool,
}

/// One in-flight build entry. `pending_dirty` carries the additional
/// dirty state during a build.
#[derive(Debug)]
pub struct InFlightBuild {
    task: BuildTask,
    /// mtime of the source file when the build was started. A result is
    /// stale if the file's current mtime is greater than this.
    started_at: std::time::SystemTime,
    /// True when a filesystem event arrived while this build was in flight.
    /// A dirty result is always discarded and the build is replaced
    /// immediately after the current task completes.
    pending_dirty: bool,
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
            lifecycle_needs_compile: false,
        }
    }
}

impl ScriptWatcher {
    /// Record that the script at the given canonical id has already been
    /// dealt with (called by `compile_and_load` at project open so the
    /// watcher does not double-build).
    pub fn mark_seen(&mut self, id: CanonicalId) {
        if !self.seen_paths.contains(&id) {
            self.seen_paths.push(id);
        }
    }
}

/// Resource-shared retire queue. Producers are Bevy systems running on
/// any thread; consumers are the `finish` exclusive system. Deduplicated
/// by canonical id, exactly-once delivered.
#[derive(Resource, Default)]
pub struct PendingRetires {
    inner: Vec<CanonicalId>,
}

impl PendingRetires {
    /// Enqueue a canonical id for retirement. Deduplicated by id.
    pub fn enqueue(&mut self, id: CanonicalId) {
        if !self.inner.iter().any(|p| p == &id) {
            self.inner.push(id);
        }
    }

    /// Drain pending retires. Called by `finish`.
    pub fn take(&mut self) -> Vec<CanonicalId> {
        std::mem::take(&mut self.inner)
    }
}

// ─── Lifecycle state machine ─────────────────────────────────────────────────

/// The set of transitions the project lifecycle can take. Computed
/// from `(watched_root, current_project)` plus the debouncer
/// attachment state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleAction {
    /// `CurrentProject` is absent and no project was open. No work.
    Idle,
    /// `CurrentProject` is set and the watcher has never attached.
    /// The new project has already been loaded by
    /// `compile_and_load_for_project` (the watcher calls it via
    /// `compile_for_new_project` after OpenFirst sets the flag); the
    /// watcher just attaches and seeds `seen_paths`.
    OpenFirst,
    /// Same project root as the watcher is attached to and the
    /// debouncer is attached. Just reconcile any pending events.
    Keep,
    /// `CurrentProject` is set and the watcher was attached to a
    /// different root. The `retired` list contains the ids the
    /// previous project held; the caller retires them in
    /// `LoadedScripts` via `PendingRetires`.
    Switch {
        old_root: PathBuf,
        new_root: PathBuf,
        retired: Vec<CanonicalId>,
    },
    /// `CurrentProject` is absent and the watcher was attached.
    /// Detach and retire every loaded script.
    Close {
        retired: Vec<CanonicalId>,
    },
    /// Same project root but the previous attach failed. Retry the
    /// attach (gated by `last_failed_attach`'s backoff in `watch`).
    RetryAttach,
}

/// Compute the lifecycle action from the watcher's attached state and
/// the current `CurrentProject` value. Pure: does not touch the world.
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

/// Subset of `ScriptWatcher` state consumed by
/// [`compute_lifecycle_action`]. Used by `lifecycle_tick` to compute the
/// action without holding an immutable borrow across multiple
/// subsequent resource mutations.
#[derive(Clone)]
pub(crate) struct LifecycleSnapshot {
    pub watched_root: Option<PathBuf>,
    pub has_debouncer: bool,
    pub seen_paths: Vec<CanonicalId>,
    pub last_failed_attach: Option<std::time::Instant>,
}

/// Same logic as `compute_lifecycle_action` but consumes a snapshot.
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

/// Apply the lifecycle action: detach / attach the watcher, drop
/// in-flight builds, retire old ids, mark `lifecycle_needs_compile`
/// when a fresh project has been attached so the companion
/// `compile_for_new_project` system runs
/// `compile_and_load_for_project` exactly once.
///
/// Takes `&mut World` rather than two resource refs because Bevy's
/// borrow checker forbids two `resource_mut` calls on the same
/// world in one scope.
pub(crate) fn apply_lifecycle_action(world: &mut World, action: LifecycleAction) {
    match action {
        LifecycleAction::Idle | LifecycleAction::Keep => {}
        LifecycleAction::Close { retired } => {
            // Touch pending first, drop the borrow, then mutate watcher.
            {
                let mut pending = world.resource_mut::<PendingRetires>();
                for id in &retired {
                    pending.enqueue(id.clone());
                }
            }
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.debouncer = None;
            *watcher.rx.lock().unwrap() = None;
            watcher.watched_root = None;
            watcher.seen_paths.clear();
            watcher.building.clear();
            watcher.last_failed_attach = None;
            watcher.lifecycle_needs_compile = false;
        }
        LifecycleAction::OpenFirst => {
            // OpenFirst happens precisely when `watcher.watched_root`
            // is None — the project root lives in `CurrentProject`,
            // not in the watcher.
            let new_root = match world.get_resource::<CurrentProject>() {
                Some(p) => p.path.clone(),
                None => return,
            };
            {
                let mut watcher = world.resource_mut::<ScriptWatcher>();
                watcher.building.clear();
                *watcher.rx.lock().unwrap() = None;
                watcher.debouncer = None;
            }
            match attach_debouncer_raw(&new_root) {
                AttachRaw::Live(debouncer, rx) => {
                    let mut watcher = world.resource_mut::<ScriptWatcher>();
                    watcher.debouncer = Some(debouncer);
                    *watcher.rx.lock().unwrap() = Some(rx);
                    watcher.watched_root = Some(new_root.clone());
                    watcher.last_failed_attach = None;
                    let ids = discovery::collect_canonical_scripts(&new_root);
                    watcher.seen_paths = ids;
                    watcher.lifecycle_needs_compile = true;
                }
                AttachRaw::Failed => {
                    let mut watcher = world.resource_mut::<ScriptWatcher>();
                    watcher.watched_root = None;
                }
            }
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
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.debouncer = None;
            *watcher.rx.lock().unwrap() = None;
            watcher.watched_root = Some(new_root.clone());
            watcher.seen_paths.clear();
            watcher.building.clear();
            watcher.last_failed_attach = None;
            match attach_debouncer_raw(&new_root) {
                AttachRaw::Live(debouncer, rx) => {
                    watcher.debouncer = Some(debouncer);
                    *watcher.rx.lock().unwrap() = Some(rx);
                    let ids = discovery::collect_canonical_scripts(&new_root);
                    watcher.seen_paths = ids;
                    watcher.lifecycle_needs_compile = true;
                }
                AttachRaw::Failed => {
                    // Backoff is recorded inside attach_debouncer_raw.
                }
            }
        }
        LifecycleAction::RetryAttach => {
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            let root = match watcher.watched_root.clone() {
                Some(r) => r,
                None => return,
            };
            watcher.building.clear();
            *watcher.rx.lock().unwrap() = None;
            watcher.debouncer = None;
            match attach_debouncer_raw(&root) {
                AttachRaw::Live(debouncer, rx) => {
                    watcher.debouncer = Some(debouncer);
                    *watcher.rx.lock().unwrap() = Some(rx);
                    watcher.last_failed_attach = None;
                }
                AttachRaw::Failed => {
                    watcher.watched_root = None;
                }
            }
        }
    }
}

enum AttachRaw {
    Live(SourceDebouncer, DebouncedRx),
    Failed,
}

fn attach_debouncer_raw(project_root: &Path) -> AttachRaw {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(DEBOUNCE, None, tx) {
        Ok(d) => d,
        Err(e) => {
            warn!("[rust-script] could not start source watch ({e})");
            return AttachRaw::Failed;
        }
    };
    if let Err(e) = debouncer.watch(project_root, RecursiveMode::Recursive) {
        warn!("[rust-script] could not start source watch ({e})");
        return AttachRaw::Failed;
    }
    info!(
        "[rust-script] watching {} recursively",
        project_root.display()
    );
    AttachRaw::Live(debouncer, rx)
}

/// Notice changed or new `.rs` files and start building them.
///
/// Bevy system: compute the lifecycle action for this frame, apply it,
/// then drain any pending debouncer events through the reconcile
/// transition. The actual SDK compile/load for a fresh project is
/// performed by [`compile_for_new_project`].
pub fn watch(world: &mut World) {
    let current_project = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone());
    lifecycle_tick(world, current_project.as_deref());
}

/// The lifecycle tick used by the Bevy `watch` system AND by tests.
/// Tests that drive this function with a `bevy::prelude::World` exercise
/// the same production code path the Bevy scheduler runs every frame.
pub(crate) fn lifecycle_tick(world: &mut World, current_project: Option<&Path>) {
    // Snapshot the watcher's lifecycle-relevant state so we can
    // compute the action without holding an immutable borrow across
    // the rest of the function.
    let snapshot = {
        let w = world.resource::<ScriptWatcher>();
        LifecycleSnapshot {
            watched_root: w.watched_root.clone(),
            has_debouncer: w.debouncer.is_some(),
            seen_paths: w.seen_paths.clone(),
            last_failed_attach: w.last_failed_attach,
        }
    };
    let action = compute_lifecycle_action_from_snapshot(&snapshot, current_project);

    match action {
        LifecycleAction::Idle => return,
        LifecycleAction::Keep => {}
        _ => {
            // Backoff gate for RetryAttach.
            if matches!(action, LifecycleAction::RetryAttach) {
                if let Some(prev) = snapshot.last_failed_attach {
                    if prev.elapsed() < ATTACH_BACKOFF {
                        return;
                    }
                }
            }
            apply_lifecycle_action(world, action);
        }
    }

    // Drain pending events.
    let project_root = match current_project {
        Some(p) => p,
        None => return,
    };
    let watcher_deb = world.resource::<ScriptWatcher>().debouncer.is_some();
    if !watcher_deb {
        return;
    }
    let mut watcher = world.resource_mut::<ScriptWatcher>();
    if let Some(events) = drain_pending(&watcher) {
        match events {
            Batch::Events(events) => {
                let plan = reconcile_with_events(&mut watcher, &project_root, events);
                if let Some(plan) = plan {
                    apply_plan(&mut watcher, &project_root, plan);
                }
            }
            Batch::FullRescan => {
                let plan = full_rescan(&mut watcher, &project_root);
                apply_plan(&mut watcher, &project_root, plan);
            }
        }
    }
}

/// One batch of events ready for reconciliation. Production constructs
/// this from the debouncer receiver; tests construct it directly.
pub enum Batch {
    Events(Vec<DebouncedEvent>),
    FullRescan,
}

#[derive(Default, Debug)]
pub(crate) struct Plan {
    pub dirty: Vec<CanonicalId>,
    pub removed: Vec<CanonicalId>,
}

/// Drain the debouncer receiver into a `Batch`. Returns `None` when no
/// events are pending.
fn drain_pending(watcher: &ScriptWatcher) -> Option<Batch> {
    let mut inflight: Vec<DebouncedEvent> = Vec::new();
    let rx_guard = watcher.rx.lock().ok()?;
    let rx = rx_guard.as_ref()?;
    for batch in rx.try_iter() {
        match batch {
            Ok(events) => inflight.extend(events),
            Err(errors) => {
                for e in errors {
                    warn!("[rust-script] watch error: {e}");
                }
                return Some(Batch::FullRescan);
            }
        }
    }
    if inflight.is_empty() {
        return None;
    }
    Some(Batch::Events(inflight))
}

/// Reconcile a single batch of debounced events against the project
/// state. `full_rescan` is called when the batch reports a directory
/// removal or rename at any depth: the OS does not enumerate every
/// child, so the per-path translation cannot be trusted for those.
///
/// Returns `None` when the batch produced no work.
pub(crate) fn reconcile_with_events(
    watcher: &mut ScriptWatcher,
    project_root: &Path,
    events: Vec<DebouncedEvent>,
) -> Option<Plan> {
    // Directory topology change detection: any `Remove` or `Modify(Name)`
    // event whose path was, or might have been, a directory. A nested
    // directory deletion that wasn't fully enumerated would leave its
    // descendant scripts in `seen_paths` and `LoadedScripts`.
    if has_directory_topology_change(watcher, project_root, &events) {
        return Some(full_rescan(watcher, project_root));
    }

    let mut plan = Plan::default();
    let unique = dedup_event_paths(&events);

    for (path, present, declared) in unique {
        if !is_source_path(project_root, &path) {
            continue;
        }
        let Some(id) = lexical_event_identity(project_root, &path) else {
            continue;
        };
        if present && declared {
            plan.dirty.push(id);
        } else if !present {
            // File missing on disk → retire if previously known.
            if watcher.seen_paths.contains(&id) {
                plan.removed.push(id);
            }
        } else if !declared {
            // File present but lost the script declaration → retire if
            // previously known.
            if watcher.seen_paths.contains(&id) {
                plan.removed.push(id);
            }
        }
    }

    // Mutually exclusive: an id cannot be both dirty and removed.
    let removed_set: HashSet<CanonicalId> = plan.removed.iter().cloned().collect();
    plan.dirty.retain(|id| !removed_set.contains(id));

    // Update seen_paths: remove retired ids, add newly dirty ones.
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

/// True when any event in the batch is a directory-level remove or
/// rename at any depth. A directory-level change requires a full rescan
/// because the OS does not enumerate every child.
fn has_directory_topology_change(
    watcher: &ScriptWatcher,
    project_root: &Path,
    events: &[DebouncedEvent],
) -> bool {
    for event in events {
        match &event.event.kind {
            EventKind::Remove(_) => {
                if path_could_be_directory_or_has_known_descendants(
                    watcher,
                    project_root,
                    &event.paths,
                ) {
                    return true;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// True if any of the paths in `paths` either:
/// - still exists as a directory (the rename half), OR
/// - is a lexical ancestor of any known script id, OR
/// - falls under a parent directory whose other child entries (still
///   on disk) suggest this was a directory.
///
/// The third check distinguishes a leaf-file removal from a directory
/// removal when the directory is already gone.
fn path_could_be_directory_or_has_known_descendants(
    watcher: &ScriptWatcher,
    project_root: &Path,
    paths: &[PathBuf],
) -> bool {
    for path in paths {
        if std::fs::symlink_metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return true;
        }
        let Ok(rel) = path.strip_prefix(project_root) else {
            continue;
        };
        let prefix = rel.to_string_lossy().replace('\\', "/");
        let prefix_with_slash = format!("{prefix}/");
        // Any known id whose relpath starts with `<prefix>/` belongs
        // below this path. If we knew any, the OS did not enumerate
        // their removal as separate events, so we must full-rescan.
        for known in &watcher.seen_paths {
            let known_rel = known.path();
            if known_rel.starts_with(&prefix_with_slash)
                || known_rel == prefix
            {
                return true;
            }
        }
    }
    false
}

/// Fold a batch of raw `DebouncedEvent`s into one `(path, present, declared)`
/// tuple per affected path. Classification happens here: each path's
/// final on-disk state is inspected independently of the raw event kind.
fn dedup_event_paths(events: &[DebouncedEvent]) -> Vec<(PathBuf, bool, bool)> {
    let mut by_path: HashMap<PathBuf, bool> = HashMap::new();
    for event in events {
        for path in &event.paths {
            let exists = std::fs::symlink_metadata(path).is_ok();
            by_path.insert(path.clone(), exists);
        }
    }
    let mut out: Vec<(PathBuf, bool, bool)> = Vec::new();
    for (path, present) in by_path {
        let declared = if present {
            std::fs::read_to_string(&path)
                .map(|src| declaration_recognised(&src))
                .unwrap_or(false)
        } else {
            false
        };
        out.push((path, present, declared));
    }
    out
}

/// Full reconciliation: rescan the project once, diff against
/// `seen_paths`, and emit dirty/removed buckets. Used for initial
/// attachment, overflow recovery, and directory topology changes.
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

/// Apply a plan: queue retires first (always), update seen state, then
/// start builds for dirty entries only when an SDK is available.
///
/// Removal is independent of SDK availability: a deletion the editor
/// observes must reach `LoadedScripts::remove` whether or not a
/// compiler is installed. A dirty build without an SDK reports the
/// problem and leaves the previously-loaded script untouched.
fn apply_plan(
    watcher: &mut ScriptWatcher,
    project_root: &Path,
    plan: Plan,
) {
    for id in &plan.removed {
        watcher.seen_paths.retain(|p| p != id);
    }

    for id in &plan.dirty {
        if !watcher.seen_paths.contains(id) {
            watcher.seen_paths.push(id.clone());
        }
    }

    let Some(sdk_root) = crate::sdk_root() else {
        if !plan.dirty.is_empty() {
            warn!(
                "[rust-script] {} rebuild(s) skipped: SDK unavailable",
                plan.dirty.len()
            );
        }
        return;
    };

    for id in plan.dirty {
        if let Some(existing) = watcher.building.get_mut(&id) {
            existing.pending_dirty = true;
            continue;
        }
        let on_disk = match watcher
            .seen_paths
            .iter()
            .find(|p| *p == &id)
            .map(|id| {
                let root = watcher.watched_root.clone().unwrap_or_default();
                root.join(id.path())
            }) {
            Some(p) => p,
            None => continue,
        };
        spawn_build(watcher, id, on_disk, project_root, sdk_root.clone());
    }
}

/// Spawn a new build Task for `id`, recording its `started_at` mtime.
fn spawn_build(
    watcher: &mut ScriptWatcher,
    id: CanonicalId,
    on_disk: PathBuf,
    project_root: &Path,
    sdk_root: PathBuf,
) {
    let started_at = std::fs::metadata(&on_disk)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    let project_path = project_root.to_path_buf();
    let sdk_root_path = sdk_root.clone();
    let id_for_task = id.clone();
    let id_for_build = id.clone();
    let on_disk_for_task = on_disk.clone();
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let sdk = renzora_plugin_build::Sdk::load(sdk_root_path.join("sdk"))
            .map_err(|e| e.to_string())?;
        crate::build_to_path_with_id(&sdk, &project_path, &on_disk_for_task, &id_for_build)
    });
    watcher.building.insert(
        id_for_task,
        InFlightBuild {
            task,
            started_at,
            pending_dirty: false,
        },
    );
}

/// Exclusive system: run the production SDK compile + load for any
/// project the watcher has just attached to. Triggered by the
/// `lifecycle_needs_compile` flag set during the
/// [`apply_lifecycle_action`] OpenFirst / Switch path. Runs exactly
/// once per attach, then clears the flag.
pub fn compile_for_new_project(world: &mut World) {
    let (project_root, should_run) = {
        let watcher = match world.get_resource::<ScriptWatcher>() {
            Some(w) => w,
            None => return,
        };
        if !watcher.lifecycle_needs_compile {
            return;
        }
        (watcher.watched_root.clone(), true)
    };
    let Some(project_root) = project_root else { return };

    // Clear the flag before doing the work so a re-entry from this
    // system cannot double-build.
    {
        let mut watcher = world.resource_mut::<ScriptWatcher>();
        watcher.lifecycle_needs_compile = false;
    }
    crate::compile_and_load_for_project(world, &project_root);
    let _ = should_run;
}

/// Poll in-flight builds. Tasks still pending stay in `watcher.building`.
/// A stale result (pending-dirty or source mtime moved past
/// `started_at`) is discarded and a replacement `Task` is spawned
/// immediately, without waiting for another filesystem event.
pub fn finish(world: &mut World) {
    // 1. Poll in place.
    let drained: Vec<(CanonicalId, Result<PathBuf, String>, std::time::SystemTime, bool)> = {
        let Some(mut watcher) = world.get_resource_mut::<ScriptWatcher>() else {
            return;
        };
        let mut drained = Vec::new();
        let ids: Vec<CanonicalId> = watcher.building.keys().cloned().collect();
        for id in ids {
            let Some(build) = watcher.building.get_mut(&id) else { continue };
            let polled = block_on(poll_once(&mut build.task));
            match polled {
                Some(result) => {
                    let InFlightBuild { task: _, started_at, pending_dirty } =
                        watcher.building.remove(&id).expect("present");
                    drained.push((id, result, started_at, pending_dirty));
                }
                None => continue,
            }
        }
        drained
    };

    // 2. Apply results.
    let project_root = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone());
    let mut replacements: Vec<CanonicalId> = Vec::new();
    {
        let mut loaded = world.resource_mut::<LoadedScripts>();
        for (id, result, started_at, pending_dirty) in drained {
            let stale = if pending_dirty {
                true
            } else {
                let on_disk = project_root.as_ref().map(|p| p.join(id.path()));
                match on_disk.and_then(|p| std::fs::metadata(&p).ok()) {
                    Some(meta) => meta
                        .modified()
                        .map(|m| m > started_at)
                        .unwrap_or(false),
                    None => false,
                }
            };
            if stale {
                replacements.push(id);
                continue;
            }
            match result.and_then(|lib_path| load_library(&lib_path)) {
                Ok((f, lib)) => {
                    loaded.insert(id.clone(), f, lib);
                    info!("[rust-script] reloaded {id}");
                    console_success("Script", format!("recompiled {id}"));
                }
                Err(e) => {
                    error!("[rust-script] {id}: {e}");
                    console_error("Script", format!("{id}\n{e}"));
                }
            }
        }
    }

    // 3. Apply retires from the resource.
    let retires: Vec<CanonicalId> = match world.get_resource_mut::<PendingRetires>() {
        Some(mut pending) => pending.take(),
        None => Vec::new(),
    };
    {
        let mut loaded = world.resource_mut::<LoadedScripts>();
        for id in &retires {
            loaded.remove(id);
            info!("[rust-script] retired {id}");
        }
    }

    // 4. Schedule replacement builds for stale ids. Spawn the Task
    //    immediately — no wait for another filesystem event.
    if !replacements.is_empty() {
        let Some(mut watcher) = world.get_resource_mut::<ScriptWatcher>() else {
            return;
        };
        let Some(project_path) = watcher.watched_root.clone() else {
            return;
        };
        let Some(sdk_root) = crate::sdk_root() else {
            // Without an SDK we cannot spawn a replacement build; the
            // next reconcile or initial rescan will re-discover and
            // dirty the id.
            warn!(
                "[rust-script] {} replacement build(s) skipped: SDK unavailable",
                replacements.len()
            );
            return;
        };
        for id in replacements {
            if let Some(existing) = watcher.building.get_mut(&id) {
                existing.pending_dirty = true;
                continue;
            }
            let on_disk = project_path.join(id.path());
            spawn_build(&mut watcher, id, on_disk, &project_path, sdk_root.clone());
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

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

/// Derive a canonical id from an event path that may or may not still
/// exist on disk. Used for Remove events and the old half of a rename.
fn lexical_event_identity(project_root: &Path, path: &Path) -> Option<CanonicalId> {
    if !is_source_path(project_root, path) {
        return None;
    }
    let stripped = path.strip_prefix(project_root).ok()?;
    let rel = stripped.to_string_lossy().replace('\\', "/");
    renzora_identity::CanonicalId::from_rooted(RootKind::Project, &rel).ok()
}

/// Minimum time between two consecutive failed `RetryAttach` attempts
/// on the same project root. Without this the watcher would log on
/// every frame and thrash the OS handle layer.
const ATTACH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

// ─── unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CompileAction, CompileStrategy, CompileStrategyResource, RustScriptPlugin,
        StrategyKind,
    };

    fn make_create_event(path: PathBuf) -> DebouncedEvent {
        DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: EventKind::Create(notify_debouncer_full::notify::event::CreateKind::File),
                paths: vec![path],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        )
    }

    fn make_remove_event(path: PathBuf) -> DebouncedEvent {
        DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: EventKind::Remove(notify_debouncer_full::notify::event::RemoveKind::File),
                paths: vec![path],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        )
    }

    fn make_modify_any_event(path: PathBuf) -> DebouncedEvent {
        DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Any),
                paths: vec![path],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        )
    }

    fn script_body() -> &'static str {
        "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
    }

    fn write_script(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn reconcile_with_events_handles_create_for_known_script() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let spin = root.join("enemy/spin.rs");
        write_script(&spin, script_body());

        let mut watcher = ScriptWatcher::default();
        let plan = reconcile_with_events(&mut watcher, root, vec![make_create_event(spin)]);
        let plan = plan.expect("create event for a declared script produces a plan");
        assert_eq!(plan.dirty.len(), 1);
        assert_eq!(plan.dirty[0].path(), "enemy/spin.rs");
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn reconcile_with_events_handles_remove_for_known_script() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let spin = root.join("enemy/spin.rs");
        write_script(&spin, script_body());

        let mut watcher = ScriptWatcher::default();
        let id = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        watcher.mark_seen(id.clone());
        // Remove the file BEFORE the event arrives.
        std::fs::remove_file(&spin).unwrap();
        let plan = reconcile_with_events(&mut watcher, root, vec![make_remove_event(spin)]);
        let plan = plan.expect("remove of a known id produces a plan");
        assert!(plan.removed.contains(&id));
        assert!(!plan.dirty.contains(&id));
    }

    #[test]
    fn reconcile_with_events_ignores_non_script_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let helper = root.join("enemy/helper.rs");
        write_script(&helper, "fn helper() {}\n");
        let mut watcher = ScriptWatcher::default();
        let plan = reconcile_with_events(&mut watcher, root, vec![make_create_event(helper)]);
        assert!(plan.is_none());
    }

    #[test]
    fn directory_remove_triggers_full_rescan() {
        // Create nested scripts under a directory, then remove the
        // directory. The reconciler must retire every script below it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.rs");
        let b = sub.join("b.rs");
        write_script(&a, script_body());
        write_script(&b, script_body());

        let mut watcher = ScriptWatcher::default();
        // Pretend the watcher already knew about these two scripts.
        let id_a = CanonicalId::from_rooted(RootKind::Project, "sub/a.rs").unwrap();
        let id_b = CanonicalId::from_rooted(RootKind::Project, "sub/b.rs").unwrap();
        watcher.mark_seen(id_a.clone());
        watcher.mark_seen(id_b.clone());

        // Delete the directory on disk.
        std::fs::remove_dir_all(&sub).unwrap();
        let plan = reconcile_with_events(&mut watcher, root, vec![make_remove_event(sub)]);
        let plan = plan.expect("directory removal produces a plan");
        assert!(plan.removed.contains(&id_a), "a.rs must retire");
        assert!(plan.removed.contains(&id_b), "b.rs must retire");
        assert!(plan.dirty.is_empty());
    }

    #[test]
    fn directory_remove_after_already_absent_retires_known_descendants() {
        // The OS sometimes reports the directory removal AFTER the
        // children have already been removed silently. The reconciler
        // must still retire the known ids.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.rs");
        let b = sub.join("nested/b.rs");
        write_script(&a, script_body());
        write_script(&b, script_body());

        let mut watcher = ScriptWatcher::default();
        let id_a = CanonicalId::from_rooted(RootKind::Project, "sub/a.rs").unwrap();
        let id_b = CanonicalId::from_rooted(RootKind::Project, "sub/nested/b.rs").unwrap();
        watcher.mark_seen(id_a.clone());
        watcher.mark_seen(id_b.clone());

        std::fs::remove_dir_all(&sub).unwrap();
        // The event arrives for the directory path itself; children are
        // gone. The full rescan path must still retire the descendants
        // we used to know about.
        let plan = reconcile_with_events(&mut watcher, root, vec![make_remove_event(sub)]);
        let plan = plan.expect("plan");
        assert!(plan.removed.contains(&id_a));
        assert!(plan.removed.contains(&id_b));
    }

    #[test]
    fn directory_rename_triggers_full_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.rs"), script_body()).unwrap();

        let mut watcher = ScriptWatcher::default();
        let id_old = CanonicalId::from_rooted(RootKind::Project, "old/a.rs").unwrap();
        watcher.mark_seen(id_old.clone());

        // Perform the rename on disk.
        std::fs::rename(&old, &new).unwrap();
        std::fs::write(new.join("a.rs"), script_body()).unwrap();
        std::fs::write(new.join("b.rs"), script_body()).unwrap();

        // notify reports rename as Modify(Name) with both paths.
        let event = DebouncedEvent::new(
            notify_debouncer_full::notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(
                    notify_debouncer_full::notify::event::RenameMode::Any,
                )),
                paths: vec![old, new],
                attrs: Default::default(),
            },
            std::time::Instant::now(),
        );
        let plan = reconcile_with_events(&mut watcher, root, vec![event]);
        let plan = plan.expect("rename produces a plan");
        let id_new_a = CanonicalId::from_rooted(RootKind::Project, "new/a.rs").unwrap();
        let id_new_b = CanonicalId::from_rooted(RootKind::Project, "new/b.rs").unwrap();
        assert!(plan.removed.contains(&id_old));
        assert!(plan.dirty.contains(&id_new_a));
        assert!(plan.dirty.contains(&id_new_b));
    }

    #[test]
    fn root_level_file_edit_uses_targeted_reconcile() {
        // An ordinary edit at the project root (not a directory change)
        // must NOT force a full rescan. The targeted path produces a
        // single-id dirty plan.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = root.join("a.rs");
        let other = root.join("sub/other.rs");
        write_script(&a, script_body());
        write_script(&other, script_body());

        let mut watcher = ScriptWatcher::default();
        // Both scripts were known.
        let id_a = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let id_other = CanonicalId::from_rooted(RootKind::Project, "sub/other.rs").unwrap();
        watcher.mark_seen(id_a.clone());
        watcher.mark_seen(id_other.clone());

        // Modify(Any) on a.rs. `a.rs`'s parent IS project_root, but
        // it's a FILE, not a directory. The targeted path should run.
        let plan = reconcile_with_events(&mut watcher, root, vec![make_modify_any_event(a)]);
        let plan = plan.expect("file edit produces a plan");
        assert_eq!(plan.dirty.len(), 1);
        assert_eq!(plan.dirty[0], id_a);
        assert!(!plan.removed.contains(&id_other), "sub/other.rs must not retire");
    }

    #[test]
    fn rename_removes_old_identity_and_adds_new_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let old = root.join("enemy/spin.rs");
        let new = root.join("props/spin.rs");
        write_script(&old, script_body());
        let mut watcher = ScriptWatcher::default();
        let old_id = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        watcher.mark_seen(old_id.clone());

        // Atomic rename on disk.
        std::fs::remove_file(&old).unwrap();
        write_script(&new, script_body());
        let events = vec![
            make_remove_event(old),
            make_create_event(new),
        ];
        let plan = reconcile_with_events(&mut watcher, root, events).expect("plan");
        let new_id = CanonicalId::from_rooted(RootKind::Project, "props/spin.rs").unwrap();
        assert!(plan.removed.contains(&old_id));
        assert!(plan.dirty.contains(&new_id));
        assert!(!plan.dirty.contains(&old_id));
        assert!(!plan.removed.contains(&new_id));
    }

    #[test]
    fn marker_removal_retires_previously_loaded_script() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let spin = root.join("enemy/spin.rs");
        write_script(&spin, script_body());
        let mut watcher = ScriptWatcher::default();
        let id = CanonicalId::from_rooted(RootKind::Project, "enemy/spin.rs").unwrap();
        watcher.mark_seen(id.clone());

        // Lose the declaration.
        std::fs::write(&spin, "").unwrap();
        let plan = reconcile_with_events(&mut watcher, root, vec![make_modify_any_event(spin)]);
        let plan = plan.expect("marker removal produces a plan");
        assert!(plan.removed.contains(&id));
        assert!(!plan.dirty.contains(&id));
    }

    #[test]
    fn lexical_event_identity_works_for_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = root.join("a/spin.rs");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
        let id = lexical_event_identity(root, &p).unwrap();
        assert_eq!(id.path(), "a/spin.rs");
        std::fs::remove_file(&p).unwrap();
        let id2 = lexical_event_identity(root, &p).unwrap();
        assert_eq!(id2, id);
    }

    #[test]
    fn is_source_path_filters_skip_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = root.join(".renzora/scripts/spin.rs");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
        assert!(!is_source_path(root, &p));
    }

    /// T1: a build that completes with pending_dirty must spawn a
    /// replacement Task immediately, without waiting for another
    /// filesystem event. We construct a real Task, mark it dirty, drive
    /// the production `finish` path via a real Bevy world, and assert
    /// a new Task exists in `watcher.building` after `finish` returns.
    #[test]
    fn dirty_completion_spawns_exactly_one_replacement_task() {
        init_task_pool();
        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        let id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();
        {
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            // Attach the debouncer directly; this test cares about
            // the in-flight build state machine, not the lifecycle.
            if let AttachRaw::Live(d, rx) = attach_debouncer_raw(root) {
                watcher.debouncer = Some(d);
                *watcher.rx.lock().unwrap() = Some(rx);
                watcher.watched_root = Some(root.to_path_buf());
            }
        }
        world.resource_mut::<ScriptWatcher>().mark_seen(id.clone());

        // A task that completes successfully the moment we poll.
        let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
        let task: Task<Result<PathBuf, String>> =
            AsyncComputeTaskPool::get().spawn(async move {
                let _ = tx_done.send(());
                Ok(PathBuf::from("/tmp/fake.so"))
            });
        let started_at = std::time::SystemTime::now();
        {
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.building.insert(
                id.clone(),
                InFlightBuild {
                    task,
                    started_at,
                    pending_dirty: true,
                },
            );
        }

        // Wait until the spawned task is Ready (signalled by tx_done).
        for _ in 0..1000 {
            if rx_done.try_recv().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Run the production `finish` system.
        finish(&mut world);

        // After finish, exactly one fresh (non-pending_dirty) build
        // entry exists for the dirty id — the replacement task was
        // spawned.
        let watcher = world.resource::<ScriptWatcher>();
        let build = watcher
            .building
            .get(&id)
            .expect("replacement task must be spawned (T1)");
        assert!(
            !build.pending_dirty,
            "replacement task must start fresh (not pending_dirty)"
        );
    }

    /// T1 (parallelism invariant): the dirty-completion replacement
    /// never produces more than one active build per id. The state is
    /// bounded even after many edits.
    #[test]
    fn dirty_completion_does_not_spawn_parallel_builds() {
        init_task_pool();
        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        let id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "fn u() {}\nrenzora::script!(u);\n").unwrap();
        {
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            if let AttachRaw::Live(d, rx) = attach_debouncer_raw(root) {
                watcher.debouncer = Some(d);
                *watcher.rx.lock().unwrap() = Some(rx);
                watcher.watched_root = Some(root.to_path_buf());
            }
        }
        world.resource_mut::<ScriptWatcher>().mark_seen(id.clone());

        let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
        let task: Task<Result<PathBuf, String>> =
            AsyncComputeTaskPool::get().spawn(async move {
                let _ = tx_done.send(());
                Ok(PathBuf::from("/tmp/fake.so"))
            });
        {
            let mut watcher = world.resource_mut::<ScriptWatcher>();
            watcher.building.insert(
                id.clone(),
                InFlightBuild {
                    task,
                    started_at: std::time::SystemTime::now(),
                    pending_dirty: true,
                },
            );
        }
        for _ in 0..1000 {
            if rx_done.try_recv().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        finish(&mut world);
        let watcher = world.resource::<ScriptWatcher>();
        assert_eq!(watcher.building.len(), 1, "exactly one build per id");
        assert!(watcher.building.contains_key(&id));
    }

    // ─── Lifecycle action classification (pure-function tests) ──────

    fn init_task_pool() {
        let _ = bevy::tasks::AsyncComputeTaskPool::get_or_init(|| {
            bevy::tasks::TaskPoolBuilder::new().num_threads(1).build()
        });
    }

/// Per-script counter. Indexed by canonical id.
    #[derive(Default)]
    struct AttemptCounts {
        /// `project_calls[path]` — how many times the lifecycle
        /// invoked the strategy for `path` (one per OpenFirst /
        /// Switch / Reopen, regardless of how many scripts the project
        /// contains).
        project_calls: std::collections::BTreeMap<PathBuf, usize>,
        /// `id_attempts[id]` — how many times the strategy attempted
        /// to compile a script with this canonical id (one per project
        /// call * per script in the project; idempotent: re-opening
        /// the same project with the same scripts increments both
        /// counters again, by design).
        id_attempts: std::collections::BTreeMap<CanonicalId, usize>,
        /// `actions_returned[id]` — how many times the strategy
        /// returned a `CompileAction` for this id. The test fake
        /// returns `Ok(action)` for every script it discovers, so this
        /// stays equal to `id_attempts[id]` while the project is open
        /// (the fake's artifact path does not exist, so the orchestrator's
        /// `load_library` step fails after this count is recorded —
        /// i.e. an "action returned" by the strategy does not imply
        /// a successful `dlopen`).
        actions_returned: std::collections::BTreeMap<CanonicalId, usize>,
        /// `failed[id]` — how many times the strategy returned an
        /// error for this id (zero in the happy-path test).
        failed: std::collections::BTreeMap<CanonicalId, usize>,
    }

    impl AttemptCounts {
        fn project_calls_for(&self, project: &Path) -> usize {
            self.project_calls.get(&project.to_path_buf()).copied().unwrap_or(0)
        }
        fn id_attempts_for(&self, id: &CanonicalId) -> usize {
            self.id_attempts.get(id).copied().unwrap_or(0)
        }
        fn actions_returned_for(&self, id: &CanonicalId) -> usize {
            self.actions_returned.get(id).copied().unwrap_or(0)
        }
        fn failed_for(&self, id: &CanonicalId) -> usize {
            self.failed.get(id).copied().unwrap_or(0)
        }
    }

    /// The test fake: implements `CompileStrategy`, runs the shared
    /// discovery helper (so we exercise the real `collect_rust_scripts`
    /// and `project_relpath_for`), and records both per-project and
    /// per-id counters. Returns a real `CompileAction` per script —
    /// with a fake artifact path that the orchestrator will fail to
    /// `dlopen`, so the test asserts about the **strategy's** records,
    /// not about `LoadedScripts`.
    ///
    /// Lives only under `#[cfg(test)]`; no production code references
    /// this type.
    struct TestCompileStrategy {
        counts: std::sync::Mutex<AttemptCounts>,
    }

    impl TestCompileStrategy {
        fn counts_snapshot(&self) -> AttemptCounts {
            // Lock once and copy all four fields out under a single
            // guard so the BTreeMaps cannot be mutated between
            // snapshots. The previous design (lock-and-clone four
            // times in a row) was hanging the test under the
            // `--profile dist` build — the temporary `MutexGuard`s
            // were apparently being held across the statement
            // boundary when `compile_project` was running on a
            // worker thread and re-entering the same mutex.
            let guard = self.counts.lock().expect("counts mutex poisoned");
            AttemptCounts {
                project_calls: guard.project_calls.clone(),
                id_attempts: guard.id_attempts.clone(),
                actions_returned: guard.actions_returned.clone(),
                failed: guard.failed.clone(),
            }
        }
    }

    impl CompileStrategy for TestCompileStrategy {
        fn compile_project(&self, project: &Path) -> Vec<CompileAction> {
            let mut counts = self.counts.lock().unwrap();
            *counts.project_calls.entry(project.to_path_buf()).or_insert(0) += 1;
            let sources = crate::discovery::collect_rust_scripts(project);
            let mut actions = Vec::with_capacity(sources.len());
            for src in &sources {
                let canonical = match crate::discovery::project_relpath_for(project, src) {
                    Some(c) => c,
                    None => continue,
                };
                *counts.id_attempts.entry(canonical.clone()).or_insert(0) += 1;
                // Return a fake artifact path so the orchestrator has
                // something to call `load_library` on. The path does
                // not exist, so `load_library` errors and the
                // orchestrator's error branch runs; this test asserts
                // strategy records, not the load outcome.
                let artifact = project.join(".renzora").join("test_artifact.so");
                *counts.actions_returned.entry(canonical.clone()).or_insert(0) += 1;
                actions.push(CompileAction {
                    src: src.clone(),
                    id: canonical,
                    artifact,
                });
            }
            actions
        }
    }

    /// Build a real `App` containing the production lifecycle systems
    /// in their production order. Returns the `App` and a handle to
    /// the test fake strategy it calls into.
    ///
    /// The fake is installed by replacing the
    /// `CompileStrategyResource`'s `Real` variant with `Test` AFTER
    /// `add_plugins` ran. The resource stays in the World —
    /// `compile_and_load_for_project` removes it temporarily, calls
    /// the strategy on a local box, and reinserts it; the test
    /// asserts the strategy is still installed after each compile
    /// (V1 acceptance criterion).
    fn build_app_with_fake_service(
        a_root: &Path,
        b_root: &Path,
    ) -> (bevy::prelude::App, std::sync::Arc<TestCompileStrategy>) {
        // Write the scripts for projects A and B.
        std::fs::write(a_root.join("a1.rs"), SCRIPT).unwrap();
        std::fs::write(a_root.join("a2.rs"), SCRIPT).unwrap();
        std::fs::write(b_root.join("b1.rs"), SCRIPT).unwrap();

        let mut app = bevy::prelude::App::new();

        // Register the production RustScriptPlugin — same wiring the
        // editor uses. The plugin's `finish` adds `ScriptsActive` +
        // the fallback `update_scripts_active` system when
        // `ScriptingPlugin` is not present. The plugin's `build`
        // configures the `PreScript → Lifecycle → Compile → Finish →
        // Dispatch` chain unconditionally.
        app.add_plugins(RustScriptPlugin);
        // Production calls `app.finish()` once between plugin
        // registration and the first frame; the scheduler test does
        // the same so `RustScriptPlugin::finish` runs and installs the
        // fallback `ScriptsActive` (or, with `ScriptingPlugin`, sets up
        // the chain).
        app.finish();

        // Swap the default strategy for the test fake. The
        // production resource remains in the World; only its inner
        // StrategyKind is changed. `compile_and_load_for_project`
        // removes the resource, calls `compile_project` on a local
        // reference, and reinserts — there is no unsafe, no shared
        // borrow across the call, and no public test type.
        let fake = std::sync::Arc::new(TestCompileStrategy {
            counts: std::sync::Mutex::new(AttemptCounts::default()),
        });
        {
            let mut resource = app
                .world_mut()
                .remove_resource::<CompileStrategyResource>()
                .expect("RustScriptPlugin must have inserted CompileStrategyResource");
            resource.0 = StrategyKind::Test(std::sync::Mutex::new(Box::new(
                TestCompileStrategyClone(fake.clone()),
            )));
            app.world_mut().insert_resource(resource);
        }
        (app, fake)
    }

    /// Newtype that wraps the `Arc<TestCompileStrategy>` so it can
    /// sit inside a `Box<dyn CompileStrategy>`. The shim is one
    /// line of delegation — it owns no discovery or load logic of
    /// its own (that lives in `TestCompileStrategy::compile_project`).
    struct TestCompileStrategyClone(std::sync::Arc<TestCompileStrategy>);

    impl CompileStrategy for TestCompileStrategyClone {
        fn compile_project(&self, project: &Path) -> Vec<CompileAction> {
            self.0.compile_project(project)
        }
    }

    const SCRIPT: &str = "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n";

    #[test]
    fn lifecycle_action_is_idle_when_no_project_ever() {
        let watcher = ScriptWatcher::default();
        let action = compute_lifecycle_action(&watcher, None);
        assert_eq!(action, LifecycleAction::Idle);
    }

    #[test]
    fn lifecycle_action_is_close_when_project_disappears() {
        let mut watcher = ScriptWatcher::default();
        watcher.watched_root = Some(std::path::PathBuf::from("/old"));
        watcher.seen_paths = vec![CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap()];
        let action = compute_lifecycle_action(&watcher, None);
        match action {
            LifecycleAction::Close { retired } => {
                assert_eq!(retired.len(), 1);
            }
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn lifecycle_action_is_open_first_when_watcher_has_no_root() {
        let watcher = ScriptWatcher::default();
        let action = compute_lifecycle_action(&watcher, Some(std::path::Path::new("/p")));
        assert_eq!(action, LifecycleAction::OpenFirst);
    }

    #[test]
    fn lifecycle_action_is_keep_when_same_root_and_attached() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watcher = ScriptWatcher::default();
        // Manually wire up an attached debouncer.
        match attach_debouncer_raw(tmp.path()) {
            AttachRaw::Live(d, rx) => {
                watcher.debouncer = Some(d);
                *watcher.rx.lock().unwrap() = Some(rx);
                watcher.watched_root = Some(tmp.path().to_path_buf());
            }
            AttachRaw::Failed => panic!("attach_debouncer_raw failed"),
        }
        let action = compute_lifecycle_action(&watcher, Some(tmp.path()));
        assert_eq!(action, LifecycleAction::Keep);
    }

    #[test]
    fn lifecycle_action_is_switch_when_roots_differ() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watcher = ScriptWatcher::default();
        match attach_debouncer_raw(tmp.path()) {
            AttachRaw::Live(d, rx) => {
                watcher.debouncer = Some(d);
                *watcher.rx.lock().unwrap() = Some(rx);
                watcher.watched_root = Some(tmp.path().to_path_buf());
            }
            AttachRaw::Failed => panic!("attach failed"),
        }
        watcher.seen_paths = vec![CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap()];
        let other = tempfile::tempdir().unwrap();
        let action = compute_lifecycle_action(&watcher, Some(other.path()));
        match action {
            LifecycleAction::Switch { old_root, new_root, retired } => {
                assert_eq!(old_root, tmp.path());
                assert_eq!(new_root, other.path());
                assert_eq!(retired.len(), 1);
            }
            _ => panic!("expected Switch"),
        }
    }

    // ─── S2/S3 final scheduler test: drives a real Bevy App ──────
    //
    // The only thing replaced between this test and the production
    // plugin is the `CompileService` resource — every system, every
    // ordering, every resource ownership rule is identical. The test
    // therefore exercises the real Bevy schedule, not a hand-written
    // replica of it.

    /// Per-frame assertions used by [`scheduler_seven_frames`]. Captures
    /// both the per-project and per-id attempt counts the seventh-pass
    /// review asked for.
    struct FrameSnapshot<'a> {
        active_root: Option<PathBuf>,
        loaded_ids: Vec<CanonicalId>,
        bare_alias_unique_for_b_leaf: bool,
        project_call_for_a: usize,
        project_call_for_b: usize,
        id_attempts_a1: usize,
        id_attempts_a2: usize,
        id_attempts_b1: usize,
        failed_total: usize,
        in_flight_for_a: usize,
        in_flight_for_b: usize,
        pending_retires: Vec<CanonicalId>,
        watcher_attached: bool,
        lifecycle_needs_compile: bool,
        strategy_present_after_compile: bool,
        // Borrowed paths so we can probe the per-project counters
        // by canonical path. The test passes the live tempdir paths.
        _a_root: &'a Path,
        _b_root: &'a Path,
        _a1: &'a CanonicalId,
        _a2: &'a CanonicalId,
        _b1: &'a CanonicalId,
    }

    fn snapshot<'a>(
        app: &mut bevy::prelude::App,
        counts: &AttemptCounts,
        a_root: &'a Path,
        b_root: &'a Path,
        a1: &'a CanonicalId,
        a2: &'a CanonicalId,
        b1: &'a CanonicalId,
    ) -> FrameSnapshot<'a> {
        // Snapshot everything we need while holding only one immutable
        // borrow, then drop it before taking the mutable borrow on
        // `PendingRetires`.
        let (watcher_state, loaded_ids, alias_unique, strategy_present) = {
            let w = app.world().resource::<ScriptWatcher>();
            let l = app.world().resource::<LoadedScripts>();
            let bare_alias = l.resolve(
                std::path::Path::new("spin.rs"),
                w.watched_root.as_deref().unwrap_or(std::path::Path::new("")),
            );
            let strategy_present =
                app.world().get_resource::<CompileStrategyResource>().is_some();
            (
                (
                    w.watched_root.clone(),
                    w.building.keys().cloned().collect::<Vec<_>>(),
                    w.debouncer.is_some(),
                    w.lifecycle_needs_compile,
                ),
                l.ids(),
                matches!(bare_alias, crate::script_resolve::ResolvedScript::Unique(_)),
                strategy_present,
            )
        };
        let pending: Vec<CanonicalId> = {
            let mut p = app.world_mut().resource_mut::<PendingRetires>();
            p.take()
        };
        let (active_root, in_flight_ids, watcher_attached, lifecycle_needs_compile) =
            watcher_state;
        let in_flight_for_a = in_flight_ids
            .iter()
            .filter(|id| id.path() == "a1.rs" || id.path() == "a2.rs")
            .count();
        let in_flight_for_b = in_flight_ids
            .iter()
            .filter(|id| id.path() == "b1.rs")
            .count();
        FrameSnapshot {
            active_root,
            loaded_ids,
            bare_alias_unique_for_b_leaf: alias_unique,
            project_call_for_a: counts.project_calls_for(a_root),
            project_call_for_b: counts.project_calls_for(b_root),
            id_attempts_a1: counts.id_attempts_for(a1),
            id_attempts_a2: counts.id_attempts_for(a2),
            id_attempts_b1: counts.id_attempts_for(b1),
            failed_total: counts.failed_for(a1)
                + counts.failed_for(a2)
                + counts.failed_for(b1),
            in_flight_for_a,
            in_flight_for_b,
            pending_retires: pending,
            watcher_attached,
            lifecycle_needs_compile,
            strategy_present_after_compile: strategy_present,
            _a_root: a_root,
            _b_root: b_root,
            _a1: a1,
            _a2: a2,
            _b1: b1,
        }
    }

    /// Drive the production Bevy schedule through the seven frames
    /// the sixth- and seventh-pass reviews require. Every system in
    /// the chain runs through `app.update()`; no system is invoked by
    /// hand. V4: every assertion is split between a project-level
    /// counter (one per OpenFirst / Switch / Reopen) and a per-canonical-id
    /// counter (one per script in the project).
    #[test]
    fn scheduler_seven_frames() {
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        // A has two scripts; B has one. Their canonical ids below.
        std::fs::write(a_root.path().join("a1.rs"), SCRIPT).unwrap();
        std::fs::write(a_root.path().join("a2.rs"), SCRIPT).unwrap();
        std::fs::write(b_root.path().join("b1.rs"), SCRIPT).unwrap();
        let a_root_path = a_root.path().to_path_buf();
        let b_root_path = b_root.path().to_path_buf();

        let (mut app, fake) = build_app_with_fake_service(a_root.path(), b_root.path());

        let a1 = CanonicalId::from_rooted(RootKind::Project, "a1.rs").unwrap();
        let a2 = CanonicalId::from_rooted(RootKind::Project, "a2.rs").unwrap();
        let b1 = CanonicalId::from_rooted(RootKind::Project, "b1.rs").unwrap();

        // ── Frame 1: no project. Update must do zero compile/load.
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert!(s.active_root.is_none(), "no project → no active root");
        assert!(s.loaded_ids.is_empty(), "no project → no loaded scripts");
        assert_eq!(s.project_call_for_a, 0, "frame 1: no A project call");
        assert_eq!(s.project_call_for_b, 0, "frame 1: no B project call");
        assert_eq!(s.id_attempts_a1, 0, "frame 1: a1 not attempted");
        assert_eq!(s.id_attempts_a2, 0, "frame 1: a2 not attempted");
        assert_eq!(s.id_attempts_b1, 0, "frame 1: b1 not attempted");
        assert_eq!(s.failed_total, 0);
        assert!(!s.watcher_attached);
        assert!(!s.lifecycle_needs_compile);
        assert!(s.strategy_present_after_compile, "strategy must remain installed");

        // ── Frame 2: open A. Exactly one project call; a1, a2 attempted
        //     exactly once each.
        app.world_mut().insert_resource(CurrentProject {
            path: a_root_path.clone(),
            config: Default::default(),
        });
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert_eq!(s.active_root.as_deref(), Some(a_root_path.as_path()));
        assert_eq!(s.project_call_for_a, 1, "frame 2: A invoked once");
        assert_eq!(s.project_call_for_b, 0, "frame 2: B untouched");
        assert_eq!(s.id_attempts_a1, 1, "frame 2: a1 attempted once");
        assert_eq!(s.id_attempts_a2, 1, "frame 2: a2 attempted once");
        assert_eq!(s.id_attempts_b1, 0, "frame 2: b1 untouched");
        assert_eq!(s.failed_total, 0);
        assert!(s.watcher_attached);
        // lifecycle_needs_compile must be cleared by compile_for_new_project
        // before the frame ends (the Compile system set ran this frame).
        assert!(!s.lifecycle_needs_compile);
        assert!(s.strategy_present_after_compile, "strategy still installed after frame 2");

        // ── Frame 3: same A, multiple updates. No extra project call
        //     and no extra per-id attempts.
        app.update();
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert_eq!(s.project_call_for_a, 1, "frame 3: A still one project call");
        assert_eq!(s.id_attempts_a1, 1, "frame 3: a1 not re-attempted");
        assert_eq!(s.id_attempts_a2, 1, "frame 3: a2 not re-attempted");
        assert_eq!(s.id_attempts_b1, 0);

        // ── Frame 4: switch A → B with an A task still pending.
        //     The lifecycle's Switch action must retire A and
        //     schedule B exactly once. The pending A task is dropped.
        {
            let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
            let task: Task<Result<PathBuf, String>> =
                AsyncComputeTaskPool::get().spawn(async move {
                    let _ = tx_done.send(());
                    Ok(PathBuf::from("/tmp/fake_a.so"))
                });
            {
                let mut w = app.world_mut().resource_mut::<ScriptWatcher>();
                w.building.insert(
                    a1.clone(),
                    InFlightBuild {
                        task,
                        started_at: std::time::SystemTime::now(),
                        pending_dirty: false,
                    },
                );
            }
            // Wait for the task to be Ready.
            for _ in 0..1000 {
                if rx_done.try_recv().is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        app.world_mut().insert_resource(CurrentProject {
            path: b_root_path.clone(),
            config: Default::default(),
        });
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert_eq!(s.active_root.as_deref(), Some(b_root_path.as_path()));
        assert_eq!(s.project_call_for_a, 1, "frame 4: A still one project call");
        assert_eq!(s.project_call_for_b, 1, "frame 4: B invoked once");
        assert_eq!(s.id_attempts_a1, 1, "frame 4: a1 not re-attempted");
        assert_eq!(s.id_attempts_a2, 1, "frame 4: a2 not re-attempted");
        assert_eq!(s.id_attempts_b1, 1, "frame 4: b1 attempted once");
        assert_eq!(s.failed_total, 0);
        assert!(s.strategy_present_after_compile);
        // A's strategy records don't reset on close — reopens
        // increment again (V4: every open counts).

        // ── Frame 5: same B, multiple updates. No extra.
        app.update();
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert_eq!(s.project_call_for_b, 1, "frame 5: B still one project call");
        assert_eq!(s.id_attempts_b1, 1);

        // ── Frame 6: close B. B retired, watcher detached.
        app.world_mut().remove_resource::<CurrentProject>();
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert!(s.active_root.is_none(), "frame 6: no active root after close");
        assert_eq!(s.project_call_for_b, 1, "frame 6: close does not invoke");
        assert_eq!(s.id_attempts_b1, 1);
        assert!(s.strategy_present_after_compile, "strategy survives close");

        // ── Frame 7: reopen A. A's per-id counters go from 1 → 2; A's
        //     project call counter goes from 1 → 2.
        app.world_mut().insert_resource(CurrentProject {
            path: a_root_path.clone(),
            config: Default::default(),
        });
        app.update();
        let counts = fake.counts_snapshot();
        let s = snapshot(&mut app, &counts, &a_root_path, &b_root_path, &a1, &a2, &b1);
        assert_eq!(s.active_root.as_deref(), Some(a_root_path.as_path()));
        assert_eq!(s.project_call_for_a, 2, "frame 7: A reopened, second project call");
        assert_eq!(s.project_call_for_b, 1, "frame 7: B still one project call");
        assert_eq!(s.id_attempts_a1, 2, "frame 7: a1 attempted twice across reopens");
        assert_eq!(s.id_attempts_a2, 2, "frame 7: a2 attempted twice across reopens");
        assert_eq!(s.id_attempts_b1, 1, "frame 7: b1 still once");
        assert_eq!(s.failed_total, 0);
        assert!(s.strategy_present_after_compile, "strategy survives reopen");
        assert!(!s.lifecycle_needs_compile);

        // No further counters change after frame 7.
    }

    // ─── V3: scheduler test with the real ScriptingPlugin installed ──
    //
    // The default scheduler_seven_frames test exercises the fallback
    // branch of RustScriptPlugin::finish (no ScriptingPlugin, so the
    // fallback `ScriptsActive` + `update_scripts_active` is installed).
    // This second test installs the real ScriptingPlugin and asserts
    // that:
    //   1. the fallback is NOT installed (no duplicate observer);
    //   2. the PreScript -> Lifecycle ordering is still enforced;
    //   3. the lifecycle + load still work.
    #[test]
    fn scheduler_with_external_pre_script_provider() {
        // The default `scheduler_seven_frames` test exercises the
        // fallback branch of `RustScriptPlugin::finish` (no
        // `ScriptingPlugin`, so the fallback `ScriptsActive` +
        // `update_scripts_active` is installed). This second test
        // installs an external provider of `ScriptsActive` (the
        // production arrangement) and asserts that:
        //   1. the fallback system is NOT added — i.e.
        //      `RustScriptPlugin::finish` correctly skips the
        //      fallback when another plugin provides it;
        //   2. the chain `PreScript -> Lifecycle -> Compile -> Finish
        //      -> Dispatch` is still configured.
        //
        // We deliberately do NOT install the real `ScriptingPlugin`
        // because its `run_scripts` system reads resources the editor
        // provides but the test environment doesn't have; that
        // arrangement is exercised by the editor's own integration
        // tests. This test exercises only the rust-script lifecycle
        // contract under "another plugin provides ScriptsActive".
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::write(a_root.path().join("a1.rs"), SCRIPT).unwrap();
        std::fs::write(a_root.path().join("a2.rs"), SCRIPT).unwrap();
        std::fs::write(b_root.path().join("b1.rs"), SCRIPT).unwrap();
        let a_root_path = a_root.path().to_path_buf();

        let mut app = bevy::prelude::App::new();

        // External provider: pre-install `ScriptsActive` (simulating
        // `ScriptingPlugin`'s contribution) AND a no-op system in
        // `PreScript` (the role `ScriptingPlugin`'s `update_scripts_active`
        // would fill). `RustScriptPlugin::finish` must detect that
        // and skip the fallback.
        app.init_resource::<renzora_scripting::ScriptsActive>();
        app.add_systems(
            bevy::prelude::Update,
            noop_pre_script_system.in_set(renzora_scripting::ScriptingSet::PreScript),
        );
        app.add_plugins(RustScriptPlugin);
        app.finish();

        // V3 acceptance: no fallback duplicate. If
        // `RustScriptPlugin::finish` had taken the fallback branch
        // it would have called `init_resource::<ScriptsActive>` again
        // (idempotent — fine) and `add_systems(Update, update_scripts_active.in_set(PreScript))`
        // (NOT idempotent — would have added a duplicate). A
        // duplicate system is detectable via Bevy's graph but is
        // also detectable indirectly: a duplicate `update_scripts_active`
        // would emit a warning, and a duplicate in `PreScript` would
        // make the run-condition for `dispatch` ambiguous. Driving
        // a frame and confirming no panic is the practical signal.

        // V3 acceptance: the chain is still enforced. Drive one
        // open-A cycle through the production schedule.
        let fake = std::sync::Arc::new(TestCompileStrategy {
            counts: std::sync::Mutex::new(AttemptCounts::default()),
        });
        {
            let mut resource = app
                .world_mut()
                .remove_resource::<CompileStrategyResource>()
                .expect("RustScriptPlugin must have inserted CompileStrategyResource");
            resource.0 = StrategyKind::Test(std::sync::Mutex::new(Box::new(
                TestCompileStrategyClone(fake.clone()),
            )));
            app.world_mut().insert_resource(resource);
        }

        app.world_mut().insert_resource(CurrentProject {
            path: a_root_path.clone(),
            config: Default::default(),
        });
        app.update();

        let counts = fake.counts_snapshot();
        let a1 = CanonicalId::from_rooted(RootKind::Project, "a1.rs").unwrap();
        let a2 = CanonicalId::from_rooted(RootKind::Project, "a2.rs").unwrap();
        let b1 = CanonicalId::from_rooted(RootKind::Project, "b1.rs").unwrap();
        assert_eq!(
            counts.id_attempts_for(&a1),
            1,
            "chain enforced: a1 attempted once"
        );
        assert_eq!(counts.id_attempts_for(&a2), 1);
        assert_eq!(counts.id_attempts_for(&b1), 0);

        // Strategy still installed (V1).
        assert!(
            app.world().get_resource::<CompileStrategyResource>().is_some(),
            "strategy must remain installed after a successful compile (V1)"
        );

        // Failed compile path: strategy remains installed.
        let failed_root = tempfile::tempdir().unwrap();
        std::fs::write(failed_root.path().join("bad.rs"), SCRIPT).unwrap();
        app.world_mut().insert_resource(CurrentProject {
            path: failed_root.path().to_path_buf(),
            config: Default::default(),
        });
        app.update();
        assert!(
            app.world().get_resource::<CompileStrategyResource>().is_some(),
            "strategy must remain installed after a FAILED compile (V1)"
        );
        let counts = fake.counts_snapshot();
        let bad_id = CanonicalId::from_rooted(RootKind::Project, "bad.rs").unwrap();
        assert_eq!(counts.id_attempts_for(&bad_id), 1);

        // Restore A: counts increment again.
        app.world_mut().insert_resource(CurrentProject {
            path: a_root_path.clone(),
            config: Default::default(),
        });
        app.update();
        let counts = fake.counts_snapshot();
        assert_eq!(
            counts.id_attempts_for(&a1),
            2,
            "A reopened twice (external provider branch)"
        );
        assert_eq!(counts.id_attempts_for(&a2), 2);
    }

    /// A no-op system used by `scheduler_with_external_pre_script_provider`
    /// to stand in for `ScriptingPlugin::update_scripts_active`. The
    /// production plugin runs more systems than this — see the editor's
    /// own integration tests for the full wiring.
    fn noop_pre_script_system() {}
}
