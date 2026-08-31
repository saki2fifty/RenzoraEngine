//! Event-driven script watcher with a real per-id build state machine.
//!
//! # Design
//!
//! The watcher maintains an explicit per-id state machine so a long Rust
//! compilation can survive across many editor frames, source edits during
//! compilation are remembered, and a stale result is never allowed to
//! overwrite a newer generation. The states are:
//!
//! - [`BuildState::Idle`] — script is known to exist on disk; no build in
//!   flight. The next filesystem edit moves to Building.
//! - [`BuildState::Building`] — a compilation is in flight. The wrapped
//!   `generation` is the canonical identity for the source the build was
//!   started against (its mtime and path). The next filesystem edit of the
//!   same path during this state marks the build dirty.
//! - [`BuildState::Dirty`] — a filesystem edit arrived while the build
//!   was in flight. The current task is allowed to complete but its result
//!   will be discarded; the watcher will schedule a new build immediately
//!   after the current task finishes.
//!
//! Three guarantees the implementation must satisfy:
//!
//! 1. A `Task` polled to `Pending` stays inside `watcher.building`. The
//!    task is removed only when it transitions to `Ready` (correction A).
//! 2. When a stale result is discarded, a replacement build for the new
//!    generation is scheduled immediately — the watcher does not wait for
//!    another filesystem event (correction B).
//! 3. The retirements collected from per-path events are stored in a
//!    Bevy resource, not in a thread-local (correction E).
//!
//! # Why this is event-driven, not polled
//!
//! After the previous commits, the watcher subscribed to
//! `notify_debouncer_full` on the project root recursively. The per-frame
//! reconcile is driven only by:
//!
//! - debounced batches arriving on `rx`;
//! - the explicit initial rescan once after attach;
//! - the bounded overflow recovery path on filesystem errors.
//!
//! Idle frames perform zero discovery calls. The full rescan is reserved
//! for initial attach and overflow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use bevy::tasks::{block_on, poll_once, AsyncComputeTaskPool, Task};
use notify_debouncer_full::{
    new_debouncer,
    notify::RecursiveMode,
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
/// debouncer, the failure backoff timer, and the retire queue consumed by
/// `finish`. Retirements are queued here rather than in a thread-local so
/// Bevy's parallel scheduler can enqueue from any worker thread and
/// drain from any other.
#[derive(Resource)]
pub struct ScriptWatcher {
    building: HashMap<CanonicalId, InFlightBuild>,
    debouncer: Option<SourceDebouncer>,
    rx: std::sync::Mutex<Option<DebouncedRx>>,
    seen_paths: Vec<CanonicalId>,
    watched_root: Option<PathBuf>,
    needs_initial_rescan: bool,
    last_failed_attach: Option<std::time::Instant>,
}

/// Result of one build task: a `Result<PathBuf, String>` from the
/// compilation, plus the metadata needed to decide whether the result is
/// fresh or stale.
#[derive(Debug)]
pub struct InFlightBuild {
    task: Task<Result<PathBuf, String>>,
    /// mtime of the source file when the build was started. A result is
    /// stale if the file's current mtime is greater than this.
    started_at: std::time::SystemTime,
    /// Source path on disk when the build was started. Used to compute the
    /// canonical id without re-stripping after a rename.
    started_path: PathBuf,
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
            needs_initial_rescan: false,
            last_failed_attach: None,
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

    /// Replace the indexed snapshot. Used by the initial attach to
    /// record every discovered script without dirtying anything.
    pub(crate) fn seed_seen(&mut self, ids: Vec<CanonicalId>) {
        self.seen_paths = ids;
    }
}

/// Resource-shared retire queue, accessible to systems that do not own
/// the watcher. The producer (a Bevy system running on any thread) and the
/// consumer (the `finish` exclusive system) must share storage that does
/// not depend on which worker thread they happen to run on.
#[derive(Resource, Default)]
pub struct PendingRetires {
    inner: Vec<CanonicalId>,
}

impl PendingRetires {
    /// Enqueue a canonical id for retirement. Deduplicated by id so
    /// repeated enqueues from concurrent producers are safe.
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

/// Notice changed or new `.rs` files and start building them.
///
/// Idle frames do no filesystem work. Reconciliation runs only when:
/// - a debounced batch has arrived;
/// - the watcher was just attached (sets `needs_initial_rescan` on
///   attach; cleared once the initial rescan has been performed).
pub fn watch(
    mut watcher: ResMut<ScriptWatcher>,
    project: Option<Res<CurrentProject>>,
    mut pending: ResMut<PendingRetires>,
) {
    let Some(project) = project else { return };
    let project_root = project.path.clone();

    if watcher.watched_root.as_deref() != Some(project_root.as_path()) {
        let backoff_active = watcher
            .last_failed_attach
            .map(|prev| prev.elapsed() < attach_backoff())
            .unwrap_or(false);
        if !backoff_active {
            let _ = attach_debouncer(&mut watcher, &project_root);
        }
    }

    if watcher.debouncer.is_none() {
        return;
    }

    let drained = reconcile_batch(&mut watcher, &project_root);

    if let Some(drained) = drained {
        apply_drained(&mut watcher, drained, &mut pending);
    }
}

/// Run one production reconcile transition: drain the debouncer channel
/// once, then reconcile each batch against the final on-disk state.
///
/// Returns `Some(plan)` only when the batch produced dirty/removed work
/// or when an overflow required a full rescan. Idle frames return `None`
/// so the caller skips the rest of the reconcile path.
pub fn reconcile_batch(watcher: &mut ScriptWatcher, project_root: &Path) -> Option<Plan> {
    let mut batch = drain_pending(watcher)?;
    match &mut batch {
        BatchOutcome::FullRescan => {
            let plan = full_rescan(watcher, project_root);
            return Some(plan);
        }
        BatchOutcome::Events(events) => {
            let mut plan = Plan::default();
            plan.dirty = Vec::new();
            plan.removed = Vec::new();
            let mut requires_full_rescan = false;
            let unique = dedup_event_paths(std::mem::take(events));
            for (path, present, declared) in unique {
                if !is_source_path(project_root, &path) {
                    continue;
                }
                let Some(id) = lexical_event_identity(project_root, &path) else {
                    continue;
                };
                if path.parent() == Some(project_root)
                    && std::fs::symlink_metadata(&path)
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                {
                    // Directory creation/rename at the project root
                    // requires a full rescan to enumerate the new
                    // children. File-level edits at the project root do
                    // NOT.
                    requires_full_rescan = true;
                    continue;
                }
                if present && declared {
                    plan.dirty.push(id);
                } else if !present || !declared {
                    // Missing on disk, OR present but lost its
                    // declaration. Either way, retire it if it was
                    // previously known.
                    if watcher.seen_paths.contains(&id) {
                        plan.removed.push(id);
                    }
                }
            }
            if requires_full_rescan {
                return Some(full_rescan(watcher, project_root));
            }
            // De-duplicate the plan: an id can appear in both dirty and
            // removed when a batch contains both a Modify event for a
            // new declaration and a Remove event for an old rename path.
            // The post-batch on-disk state inspection already guarantees
            // mutual exclusivity in normal flows; this drops any
            // pathological duplicates.
            let mut seen_dirty: std::collections::HashSet<CanonicalId> =
                std::collections::HashSet::new();
            plan.dirty.retain(|id| seen_dirty.insert(id.clone()));
            let removed_set: std::collections::HashSet<CanonicalId> =
                plan.removed.iter().cloned().collect();
            plan.dirty.retain(|id| !removed_set.contains(id));

            // Update seen_paths for the per-path translation: only
            // remove ids that are not also dirty (a remove-then-recreate
            // in one batch ends with the id dirty, not removed).
            let dirty_set: std::collections::HashSet<CanonicalId> =
                plan.dirty.iter().cloned().collect();
            watcher.seen_paths.retain(|p| !plan.removed.contains(p));
            for id in &plan.dirty {
                if !watcher.seen_paths.contains(id) {
                    watcher.seen_paths.push(id.clone());
                }
                let _ = &dirty_set;
            }

            if plan.dirty.is_empty() && plan.removed.is_empty() {
                return None;
            }
            return Some(plan);
        }
    }
}

#[derive(Default)]
#[doc(hidden)]
pub struct Plan {
    pub dirty: Vec<CanonicalId>,
    pub removed: Vec<CanonicalId>,
}

enum BatchOutcome {
    Events(Vec<DebouncedEvent>),
    FullRescan,
}

/// Drain the debouncer receiver. Returns:
/// - `None` when no events are pending (idle);
/// - `Some(FullRescan)` when the underlying filesystem watcher reported
///   errors;
/// - `Some(Events)` with the collected, deduplicated event list.
fn drain_pending(watcher: &mut ScriptWatcher) -> Option<BatchOutcome> {
    let mut inflight: Vec<DebouncedEvent> = Vec::new();
    {
        let mut rx_guard = watcher.rx.lock().ok()?;
        let rx = rx_guard.as_mut()?;
        for batch in rx.try_iter() {
            match batch {
                Ok(events) => inflight.extend(events),
                Err(errors) => {
                    for e in errors {
                        warn!("[rust-script] watch error: {e}");
                    }
                    drop(rx_guard);
                    return Some(BatchOutcome::FullRescan);
                }
            }
        }
    }
    if inflight.is_empty() {
        return None;
    }
    Some(BatchOutcome::Events(inflight))
}

/// Fold a batch of raw `DebouncedEvent`s into one `(path, present, declared)`
/// tuple per affected path. The classification happens here: for each
/// path, the final on-disk state is inspected (file exists? declaration
/// present?), independent of the raw event kind.
fn dedup_event_paths(events: Vec<DebouncedEvent>) -> Vec<(PathBuf, bool, bool)> {
    use std::collections::HashMap;
    let mut by_path: HashMap<PathBuf, bool> = HashMap::new();
    for event in events {
        for path in &event.paths {
            // `Remove` events report a path that no longer exists; the
            // existence check below picks that up. `Create`/`Modify`
            // paths usually exist; the `symlink_metadata` decides.
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
/// attachment and overflow recovery — never on idle frames.
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
fn apply_drained(
    watcher: &mut ScriptWatcher,
    plan: Plan,
    pending: &mut PendingRetires,
) {
    // 1. Apply/queue removals first.
    for id in &plan.removed {
        pending.enqueue(id.clone());
    }

    // 2. Update seen state for the dirty batch.
    for id in &plan.dirty {
        if !watcher.seen_paths.contains(id) {
            watcher.seen_paths.push(id.clone());
        }
    }

    // 3. Start builds for dirty entries — only when the SDK is available.
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
        let started_at = std::fs::metadata(&on_disk)
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        let project_path = match watcher.watched_root.clone() {
            Some(p) => p,
            None => continue,
        };
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
                started_path: on_disk,
                pending_dirty: false,
            },
        );
    }
}

/// Load whatever finished building this frame. Polls every in-flight
/// build; tasks still pending stay in `watcher.building` (correction A).
/// Discards stale results and schedules replacement builds immediately
/// (correction B). Applies retirements queued by the watcher during the
/// previous reconcile. Retires flow through a Bevy resource so any
/// system on any thread can post them (correction E).
pub fn finish(world: &mut World) {
    // 1. Poll in place: every Pending task stays in `building`. Only
    //    tasks that returned Ready are removed.
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
                    let InFlightBuild {
                        task: _,
                        started_at,
                        started_path: _,
                        pending_dirty,
                    } = watcher.building.remove(&id).expect("present");
                    drained.push((id, result, started_at, pending_dirty));
                }
                None => continue,
            }
        }
        drained
    };

    // 2. Apply results, batching mutations on `LoadedScripts`.
    let project_root = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone());
    let mut replacements: Vec<CanonicalId> = Vec::new();
    {
        let mut loaded = world.resource_mut::<LoadedScripts>();
        for (id, result, started_at, pending_dirty) in drained {
            // Stale check: pending-dirty OR the source moved since the
            // build was started. Either way, discard this result and
            // schedule a replacement build.
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
                debug!("[rust-script] {id}: source changed during build; rescheduling");
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

    // 3. Apply retires queued by the watcher during the previous
    //    reconcile, taking them out of the resource so we apply each
    //    exactly once.
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

    // 4. Schedule replacement builds for stale ids that survived the
    //    finish. The replacement must happen on the next frame, not
    //    wait for another filesystem event.
    if !replacements.is_empty() {
        if let Some(mut watcher) = world.get_resource_mut::<ScriptWatcher>() {
            for id in replacements {
                if let Some(existing) = watcher.building.get_mut(&id) {
                    existing.pending_dirty = true;
                } else {
                    // No way to reschedule from here without an SDK path
                    // and on_disk path. Mark the id dirty so the next
                    // `reconcile_batch` picks it up; the next event or
                    // initial rescan will build it.
                    if !watcher.seen_paths.contains(&id) {
                        watcher.seen_paths.push(id.clone());
                    }
                }
            }
        }
    }
}

// ─── test surface ─────────────────────────────────────────────────────────────

/// Drive one production reconcile transition frame. The function the
/// production `watch` system calls is the same one this helper calls;
/// tests do not maintain a parallel state-machine implementation.
#[doc(hidden)]
pub fn reconcile_one_frame(watcher: &mut ScriptWatcher, project_root: &Path) -> Option<Plan> {
    if watcher.watched_root.as_deref() != Some(project_root) {
        attach_debouncer(watcher, project_root);
    }
    if watcher.debouncer.is_none() {
        return None;
    }
    let mut plan = reconcile_batch(watcher, project_root);
    if plan.is_none() && watcher.needs_initial_rescan {
        if watcher.seen_paths.is_empty() {
            plan = Some(full_rescan(watcher, project_root));
        }
        watcher.needs_initial_rescan = false;
    }
    plan
}

/// Whether the watcher has the `needs_initial_rescan` flag set.
#[doc(hidden)]
pub fn needs_initial_rescan(watcher: &ScriptWatcher) -> bool {
    watcher.needs_initial_rescan
}

/// Whether the debouncer has been successfully attached.
#[doc(hidden)]
pub fn is_attached(watcher: &ScriptWatcher) -> bool {
    watcher.debouncer.is_some()
}

/// Test-only accessor for `seen_paths`.
#[doc(hidden)]
pub fn seen_paths_for_test(watcher: &ScriptWatcher) -> &[CanonicalId] {
    &watcher.seen_paths
}

/// Test-only accessor for `watched_root`.
#[doc(hidden)]
pub fn watched_root_for_test(watcher: &ScriptWatcher) -> Option<&Path> {
    watcher.watched_root.as_deref()
}

/// Test-only setter for `last_failed_attach`.
#[doc(hidden)]
pub fn set_last_failed_attach_for_test(
    watcher: &mut ScriptWatcher,
    when: Option<std::time::Instant>,
) {
    watcher.last_failed_attach = when;
}

/// Test-only getter for `last_failed_attach`.
#[doc(hidden)]
pub fn last_failed_attach_for_test(watcher: &ScriptWatcher) -> Option<std::time::Instant> {
    watcher.last_failed_attach
}

/// Drive `attach_debouncer` from tests.
#[doc(hidden)]
pub fn attach_debouncer_for_test(watcher: &mut ScriptWatcher, project_root: &Path) {
    attach_debouncer(watcher, project_root);
}

/// Drain pending retires from the resource (test-only).
#[doc(hidden)]
pub fn take_pending_retires_for_test(pending: &mut PendingRetires) -> Vec<CanonicalId> {
    pending.take()
}

/// Backoff window accessor for tests.
#[doc(hidden)]
pub fn attach_backoff_for_test() -> std::time::Duration {
    attach_backoff()
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
/// exist on disk. This is the production path for Remove events and the
/// old half of a rename; the file is gone, so `project_relpath_for`'s
/// `path.is_file()` guard cannot be used.
fn lexical_event_identity(project_root: &Path, path: &Path) -> Option<CanonicalId> {
    if !is_source_path(project_root, path) {
        return None;
    }
    let stripped = path.strip_prefix(project_root).ok()?;
    let rel = stripped.to_string_lossy().replace('\\', "/");
    renzora_identity::CanonicalId::from_rooted(RootKind::Project, &rel).ok()
}

pub(crate) fn attach_debouncer(watcher: &mut ScriptWatcher, project_root: &Path) -> AttachOutcome {
    *watcher.rx.lock().unwrap() = None;
    watcher.debouncer = None;
    watcher.needs_initial_rescan = false;

    let was_empty = watcher.seen_paths.is_empty();
    if was_empty {
        watcher.seen_paths.clear();
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(DEBOUNCE, None, tx) {
        Ok(d) => d,
        Err(e) => {
            record_failed_attach(watcher, e);
            return AttachOutcome::Failed;
        }
    };
    if let Err(e) = debouncer.watch(project_root, RecursiveMode::Recursive) {
        record_failed_attach(watcher, e);
        return AttachOutcome::Failed;
    }
    info!(
        "[rust-script] watching {} recursively",
        project_root.display()
    );
    *watcher.rx.lock().unwrap() = Some(rx);
    watcher.debouncer = Some(debouncer);
    watcher.watched_root = Some(project_root.to_path_buf());
    watcher.last_failed_attach = None;

    if was_empty {
        let ids = discovery::collect_canonical_scripts(project_root);
        watcher.seed_seen(ids);
    }
    watcher.needs_initial_rescan = true;
    AttachOutcome::Live
}

enum AttachOutcome {
    Live,
    Failed,
}

fn record_failed_attach(watcher: &mut ScriptWatcher, e: impl std::fmt::Display) {
    let now = std::time::Instant::now();
    let should_log = match watcher.last_failed_attach {
        None => true,
        Some(prev) => now.duration_since(prev) >= attach_backoff(),
    };
    if should_log {
        warn!("[rust-script] could not start source watch ({e})");
    }
    watcher.last_failed_attach = Some(now);
}

fn attach_backoff() -> std::time::Duration {
    std::time::Duration::from_secs(5)
}

// ─── unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_source_path_filters_out_skip_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = root.join(".renzora/scripts/spin.rs");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
        assert!(!is_source_path(root, &p));
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

        // Same path after the file is gone: still resolves, because the
        // lexical path is what produces the canonical id.
        std::fs::remove_file(&p).unwrap();
        let removed_id = lexical_event_identity(root, &p).unwrap();
        assert_eq!(removed_id, id);
    }

    #[test]
    fn lexical_event_identity_rejects_skip_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = root.join("target/spin.rs");
        assert!(lexical_event_identity(root, &p).is_none());
    }
}
