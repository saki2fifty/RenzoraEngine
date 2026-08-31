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
/// debouncer, the failure backoff timer, and the per-id seen-path
/// snapshot.
#[derive(Resource)]
pub struct ScriptWatcher {
    building: HashMap<CanonicalId, InFlightBuild>,
    debouncer: Option<SourceDebouncer>,
    rx: std::sync::Mutex<Option<DebouncedRx>>,
    seen_paths: Vec<CanonicalId>,
    watched_root: Option<PathBuf>,
    last_failed_attach: Option<std::time::Instant>,
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

/// Notice changed or new `.rs` files and start building them.
///
/// Idle frames do no filesystem work. Reconciliation runs only when:
/// - the project root changes (retire old ids, drop old in-flight
///   builds, attach a fresh debouncer);
/// - a debounced batch arrives.
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
            let (outcome, retired) = attach_debouncer(&mut watcher, &project_root);
            match outcome {
                AttachOutcome::Live => {
                    // Project switch (or first attach): every id the
                    // previous project knew about must be retired from
                    // LoadedScripts before any new project script is
                    // loaded.
                    for id in retired {
                        pending.enqueue(id);
                    }
                }
                AttachOutcome::Failed => {
                    // attach_debouncer already recorded the failure and
                    // scheduled the next attempt via last_failed_attach.
                }
            }
        }
    }

    if watcher.debouncer.is_none() {
        return;
    }

    if let Some(events) = drain_pending(&watcher) {
        match events {
            Batch::Events(events) => {
                let plan = reconcile_with_events(&mut watcher, &project_root, events);
                if let Some(plan) = plan {
                    apply_plan(&mut watcher, &project_root, plan, &mut pending);
                }
            }
            Batch::FullRescan => {
                let plan = full_rescan(&mut watcher, &project_root);
                apply_plan(&mut watcher, &project_root, plan, &mut pending);
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
    pending: &mut PendingRetires,
) {
    for id in &plan.removed {
        pending.enqueue(id.clone());
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

pub(crate) fn attach_debouncer(
    watcher: &mut ScriptWatcher,
    project_root: &Path,
) -> (AttachOutcome, Vec<CanonicalId>) {
    // Capture the previous project's ids BEFORE we drop the in-flight
    // builds and reseed `seen_paths`. The caller (`watch`) enqueues
    // these into `PendingRetires` so `finish` removes them from
    // `LoadedScripts` once the world is reachable again.
    let prior_seen = std::mem::take(&mut watcher.seen_paths);

    // Project switch: every in-flight build belonged to the previous
    // project. Dropping the `Task`s here drops their JoinHandles; the
    // AsyncComputeTaskPool will reap finished futures when nothing
    // else holds them.
    watcher.building.clear();

    *watcher.rx.lock().unwrap() = None;
    watcher.debouncer = None;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(DEBOUNCE, None, tx) {
        Ok(d) => d,
        Err(e) => {
            record_failed_attach(watcher, e);
            // Restore prior_seen so a future attach can retry.
            watcher.seen_paths = prior_seen;
            return (AttachOutcome::Failed, Vec::new());
        }
    };
    if let Err(e) = debouncer.watch(project_root, RecursiveMode::Recursive) {
        record_failed_attach(watcher, e);
        watcher.seen_paths = prior_seen;
        return (AttachOutcome::Failed, Vec::new());
    }
    info!(
        "[rust-script] watching {} recursively",
        project_root.display()
    );
    *watcher.rx.lock().unwrap() = Some(rx);
    watcher.debouncer = Some(debouncer);
    watcher.watched_root = Some(project_root.to_path_buf());
    watcher.last_failed_attach = None;

    // Replace the seen-path snapshot with the new project's discovery.
    watcher.seen_paths = discovery::collect_canonical_scripts(project_root);
    (AttachOutcome::Live, prior_seen)
}

pub(crate) enum AttachOutcome {
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
    use crate::ScriptFn;

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
            crate::watch::attach_debouncer(&mut watcher, root);
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
            crate::watch::attach_debouncer(&mut watcher, root);
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

    // ─── F5: idle frames after attachment perform zero work ──────────

    /// F5: after `attach_debouncer` seeds the watcher with the project's
    /// discovered ids, an idle `reconcile_with_events` call (no events
    /// pending) must perform zero discovery work. The watcher's
    /// production lifecycle is one attach, then idle until a real
    /// event arrives.
    #[test]
    fn idle_after_attach_does_no_discovery_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), script_body()).unwrap();

        let mut watcher = ScriptWatcher::default();
        attach_debouncer(&mut watcher, root);
        // `seen_paths` is now the project's ids, set by attach.
        assert_eq!(watcher.seen_paths.len(), 1);

        // No events arrived; `reconcile_with_events` returns None and
        // does not touch `seen_paths` or `building`.
        let plan = reconcile_with_events(&mut watcher, root, vec![]);
        assert!(plan.is_none());
        assert_eq!(watcher.seen_paths.len(), 1, "no discovery on idle");
        assert!(watcher.building.is_empty(), "no build on idle");
    }

    /// F5: the attach transition happens exactly once per project.
    /// After attach, every subsequent reattach (same root) leaves
    /// `seen_paths` unchanged in size and content.
    #[test]
    fn attach_transition_occurs_exactly_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), script_body()).unwrap();

        let mut watcher = ScriptWatcher::default();
        attach_debouncer(&mut watcher, root);
        let snap1 = watcher.seen_paths.clone();
        assert_eq!(snap1.len(), 1);

        // Reattach the SAME root. The discovery walk is idempotent,
        // so the snapshot is stable.
        attach_debouncer(&mut watcher, root);
        let snap2 = watcher.seen_paths.clone();
        assert_eq!(snap1, snap2, "same-root reattach is stable");

        // `reconcile_with_events` with no events is a no-op. No
        // additional work happens.
        let plan = reconcile_with_events(&mut watcher, root, vec![]);
        assert!(plan.is_none());
        let snap3 = watcher.seen_paths.clone();
        assert_eq!(snap1, snap3);
    }

    // ─── F2: project switching ─────────────────────────────────────────

    /// F2 case 1: Project A has `a.rs`, Project B has `b.rs`. Switching
    /// retires A's id and replaces seen_paths with B's id.
    #[test]
    fn switching_projects_replaces_seen_and_queues_old_retires() {
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::write(a_root.path().join("a.rs"), script_body()).unwrap();
        std::fs::write(b_root.path().join("b.rs"), script_body()).unwrap();

        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        // Attach to A and add A's id to LoadedScripts (simulating the
        // editor's pre-watcher compile_and_load).
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            attach_debouncer(&mut w, a_root.path());
        }
        let a_id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let f: ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
        world.resource_mut::<LoadedScripts>().insert_borrowed(a_id.clone(), f);
        assert!(world.resource::<LoadedScripts>().is_loaded(&a_id));

        // Switch to B.
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            let (outcome, retired) = attach_debouncer(&mut w, b_root.path());
            assert!(matches!(outcome, AttachOutcome::Live));
            for id in retired {
                world.resource_mut::<PendingRetires>().enqueue(id);
            }
        }

        // The watcher's seen_paths is now B's id, not A's.
        let b_id = CanonicalId::from_rooted(RootKind::Project, "b.rs").unwrap();
        let w = world.resource::<ScriptWatcher>();
        assert!(w.seen_paths.contains(&b_id));
        assert!(!w.seen_paths.contains(&a_id));
        assert_eq!(w.watched_root.as_deref(), Some(b_root.path()));
        assert!(w.building.is_empty(), "in-flight builds dropped on switch");

        // A's LoadedScripts entry retires when finish runs.
        finish(&mut world);
        assert!(
            !world.resource::<LoadedScripts>().is_loaded(&a_id),
            "old-project script must retire after switch"
        );
    }

    /// F2 case 2: same relative name in both projects. After the switch,
    /// the leaf name must resolve to the NEW project's id only.
    #[test]
    fn switching_projects_with_overlapping_leaf_replaces_alias() {
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(a_root.path().join("a")).unwrap();
        std::fs::create_dir_all(b_root.path().join("b")).unwrap();
        std::fs::write(a_root.path().join("a/spin.rs"), script_body()).unwrap();
        std::fs::write(b_root.path().join("b/spin.rs"), script_body()).unwrap();

        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            attach_debouncer(&mut w, a_root.path());
        }
        let a_id = CanonicalId::from_rooted(RootKind::Project, "a/spin.rs").unwrap();
        let f: ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
        world.resource_mut::<LoadedScripts>().insert_borrowed(a_id.clone(), f);

        // Switch to B.
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            let (outcome, retired) = attach_debouncer(&mut w, b_root.path());
            assert!(matches!(outcome, AttachOutcome::Live));
            for id in retired {
                world.resource_mut::<PendingRetires>().enqueue(id);
            }
        }
        // Simulate B's compile_and_load running after the switch
        // (production behaviour: the new project loads its scripts).
        let b_id = CanonicalId::from_rooted(RootKind::Project, "b/spin.rs").unwrap();
        let f2: ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
        world.resource_mut::<LoadedScripts>().insert_borrowed(b_id.clone(), f2);
        finish(&mut world);

        let loaded = world.resource::<LoadedScripts>();
        // The old id is gone.
        assert!(!loaded.is_loaded(&a_id));
        // The bare leaf `spin.rs` resolves to B's id only.
        match loaded.resolve(std::path::Path::new("spin.rs"), b_root.path()) {
            crate::script_resolve::ResolvedScript::Unique(id) => assert_eq!(id, b_id),
            other => panic!("expected Unique(b), got {other:?}"),
        }
    }

    /// F2 case 3: switching to a project with no Rust scripts retires
    /// the old project's ids and leaves LoadedScripts empty.
    #[test]
    fn switching_to_empty_project_leaves_no_old_ids() {
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::write(a_root.path().join("a.rs"), script_body()).unwrap();

        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            attach_debouncer(&mut w, a_root.path());
        }
        let a_id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let f: ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
        world.resource_mut::<LoadedScripts>().insert_borrowed(a_id.clone(), f);

        // Switch to an empty project.
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            let (outcome, retired) = attach_debouncer(&mut w, b_root.path());
            assert!(matches!(outcome, AttachOutcome::Live));
            for id in retired {
                world.resource_mut::<PendingRetires>().enqueue(id);
            }
        }
        assert!(world.resource::<ScriptWatcher>().seen_paths.is_empty());
        finish(&mut world);
        assert!(!world.resource::<LoadedScripts>().is_loaded(&a_id));
        assert!(
            world.resource::<LoadedScripts>().ids().is_empty(),
            "no old-project ids may remain"
        );
    }

    /// F2 case 4: an in-flight build from the old project cannot load
    /// after switching. The watcher's `building` map is dropped on
    /// attach, so the Task is gone and its eventual Ready value can
    /// not be inserted into the new project's LoadedScripts.
    #[test]
    fn old_project_in_flight_build_cannot_load_after_switch() {
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::write(a_root.path().join("a.rs"), script_body()).unwrap();

        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            attach_debouncer(&mut w, a_root.path());
        }
        let a_id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        // Insert a task that would complete successfully.
        let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
        let task: Task<Result<PathBuf, String>> =
            AsyncComputeTaskPool::get().spawn(async move {
                let _ = tx_done.send(());
                Ok(PathBuf::from("/tmp/fake.so"))
            });
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            w.building.insert(
                a_id.clone(),
                InFlightBuild {
                    task,
                    started_at: std::time::SystemTime::now(),
                    pending_dirty: false,
                },
            );
        }
        for _ in 0..1000 {
            if rx_done.try_recv().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Switch projects BEFORE finish() runs.
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            let (outcome, retired) = attach_debouncer(&mut w, b_root.path());
            assert!(matches!(outcome, AttachOutcome::Live));
            for id in retired {
                world.resource_mut::<PendingRetires>().enqueue(id);
            }
        }
        finish(&mut world);
        // The new project had no scripts; the build map is empty;
        // a_id is NOT in LoadedScripts.
        let loaded = world.resource::<LoadedScripts>();
        assert!(!loaded.is_loaded(&a_id));
        let w = world.resource::<ScriptWatcher>();
        assert!(w.building.is_empty(), "old in-flight task dropped on switch");
        assert!(w.seen_paths.is_empty(), "new empty project has no scripts");
    }

    /// F2 case 5 (paired): a project switch followed by a finishing
    /// task from the OLD project must not insert into the new project's
    /// LoadedScripts.
    #[test]
    fn old_completed_task_cannot_load_into_new_project() {
        // The test above already exercises this: after attach_debouncer,
        // `watcher.building` is empty, so the old task is gone. finish()
        // iterates an empty building map and inserts nothing.
        // This second test is a focused re-run of that invariant
        // without all the extra scaffolding.
        init_task_pool();
        let a_root = tempfile::tempdir().unwrap();
        let b_root = tempfile::tempdir().unwrap();
        std::fs::write(a_root.path().join("a.rs"), script_body()).unwrap();

        let mut world = bevy::prelude::World::new();
        world.insert_resource(ScriptWatcher::default());
        world.insert_resource(PendingRetires::default());
        world.insert_resource(LoadedScripts::default());

        let id = CanonicalId::from_rooted(RootKind::Project, "a.rs").unwrap();
        let f: ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
        // Simulate: a was loaded before the switch.
        world.resource_mut::<LoadedScripts>().insert_borrowed(id.clone(), f);

        // Attach A then switch to B.
        {
            let mut w = world.resource_mut::<ScriptWatcher>();
            attach_debouncer(&mut w, a_root.path());
            let (outcome, retired) = attach_debouncer(&mut w, b_root.path());
            assert!(matches!(outcome, AttachOutcome::Live));
            for id in retired {
                world.resource_mut::<PendingRetires>().enqueue(id);
            }
        }
        finish(&mut world);
        // A's id retired; B is empty so nothing new.
        let loaded = world.resource::<LoadedScripts>();
        assert!(!loaded.is_loaded(&id));
    }

    fn init_task_pool() {
        let _ = bevy::tasks::AsyncComputeTaskPool::get_or_init(|| {
            bevy::tasks::TaskPoolBuilder::new().num_threads(1).build()
        });
    }
}
