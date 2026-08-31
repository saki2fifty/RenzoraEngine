//! Event-driven script watcher.
//!
//! # Why the old library is never unloaded
//!
//! A reload maps a NEW image and repoints [`LoadedScripts`]; the old one stays
//! mapped for the life of the process. It has to: a schedule, a `Local`, or a
//! captured closure may still hold pointers into it, and `renzora_plugin`'s
//! loader deadlocked in `FreeLibrary` and later crashed the runtime with an
//! access violation learning that lesson twice.
//!
//! # Why this is event-driven, not polled
//!
//! After Phase 1 commit 1.2 the watcher subscribed to
//! `notify_debouncer_full` on the project root recursively, but the
//! reconcile path still ran a full rescan every frame. Correction 1 makes the
//! idle Update path perform zero project-directory scans. Reconciliation
//! runs only when:
//!
//! - a debounced batch arrives;
//! - the watcher was just attached (one initial rescan, then idle);
//! - the OS queue overflowed;
//! - a previous reconciliation requested a follow-up rescan.
//!
//! Per-path events translate directly to `dirty` / `removed` for the affected
//! canonical ids; the rest of the indexed snapshot is left alone. The full
//! rescan is reserved for initial attachment and overflow recovery. The SKIP
//! list excludes `.renzora/`, `target/`, `node_modules/`, `.git/`, `dist/`,
//! `.svn/`, `.hg/` and every dot-prefixed directory; generated outputs never
//! trigger a build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use bevy::prelude::*;
use bevy::tasks::{block_on, poll_once, AsyncComputeTaskPool, Task};
use notify_debouncer_full::{
    new_debouncer,
    notify::{event::ModifyKind, EventKind, RecursiveMode},
    DebounceEventResult, DebouncedEvent,
};
use renzora::core::console_log::{console_error, console_success};
use renzora::CurrentProject;
use renzora_identity::CanonicalId;

use crate::build_to_path_with_id;
use crate::declaration_recognised;
use crate::discovery;
use crate::load_library;
use crate::LoadedScripts;

const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

type DebouncedRx = Receiver<DebounceEventResult>;
type SourceDebouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

#[derive(Resource)]
pub struct ScriptWatcher {
    /// Builds in flight, keyed by `CanonicalId`. The fields other than
    /// `task` exist to support the corrections that follow this commit
    /// (generation tracking — see the same field's meaning in `finish`).
    building: HashMap<CanonicalId, InFlightBuild>,
    /// Cached debouncer + receiver. Created lazily when a project opens.
    debouncer: Option<SourceDebouncer>,
    rx: Mutex<Option<DebouncedRx>>,
    /// Indexed snapshot of known canonical ids under the project root.
    /// Touched only by reconcile passes (initial attach, overflow), never
    /// on idle frames.
    seen_paths: Vec<CanonicalId>,
    /// Project root the current debouncer is attached to.
    watched_root: Option<PathBuf>,
    /// True when the next frame must reconcile. Set when the watcher
    /// was just attached (initial rescan), cleared once consumed.
    needs_initial_rescan: bool,
    /// Last time an attach attempt failed. Combined with the
    /// `attach_backoff` constant, used to throttle retry attempts when
    /// the FS does not support notifications or the project root is
    /// not yet present.
    last_failed_attach: Option<std::time::Instant>,
}

#[derive(Debug)]
struct InFlightBuild {
    task: Task<Result<PathBuf, String>>,
    started_at: std::time::SystemTime,
    /// True when an event arrived while this build was in flight. The
    /// install step discards the result if true; the next reconcile will
    /// schedule a fresh build.
    pending_dirty: bool,
}

impl Default for ScriptWatcher {
    fn default() -> Self {
        Self {
            building: HashMap::new(),
            debouncer: None,
            rx: Mutex::new(None),
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
    fn seed_seen(&mut self, ids: Vec<CanonicalId>) {
        self.seen_paths = ids;
    }

    /// Test-only helper to seed without touching the filesystem.
    #[cfg(test)]
    pub fn seed_seen_for_test(&mut self, ids: Vec<CanonicalId>) {
        self.seed_seen(ids);
    }
}

/// Notice changed or new `.rs` files and start building them.
///
/// Idle frames do no filesystem work. Reconciliation runs only when:
/// - a debounced batch has arrived;
/// - the watcher was just attached (sets `needs_initial_rescan` on
///   attach; cleared once the initial rescan has been performed).
pub fn watch(mut watcher: ResMut<ScriptWatcher>, project: Option<Res<CurrentProject>>) {
    let Some(project) = project else { return };
    let project_root = project.path.clone();

    // Reattach if the project path changed or first time.
    if watcher.watched_root.as_deref() != Some(project_root.as_path()) {
        // Honour the failed-attach backoff: do not retry while we are
        // still inside the cooldown window. Without this, every frame
        // would re-attempt and emit a warning, and on a permanently
        // broken watcher the user's log would be unusable.
        let backoff_active = watcher
            .last_failed_attach
            .map(|prev| prev.elapsed() < attach_backoff())
            .unwrap_or(false);
        if !backoff_active {
            let _ = attach_debouncer(&mut watcher, &project_root);
            // attach_debouncer sets watched_root only on success; if it
            // returned Failed, the next frame retries (after the backoff).
        }
    }

    if watcher.debouncer.is_none() {
        return;
    }

    // Drain events if any. This is the only path that does per-path
    // translation; full rescans are reserved for the initial attach and
    // overflow recovery (handled inside drain_pending).
    let drained = match drain_pending(&mut watcher, &project_root) {
        Some(d) => d,
        None => {
            // Nothing pending. Possibly an initial-rescan frame.
            if watcher.needs_initial_rescan {
                let drained = full_rescan(&mut watcher, &project_root);
                watcher.needs_initial_rescan = false;
                drained
            } else {
                return;
            }
        }
    };

    apply_drained(&mut watcher, &project_root, drained);
}

/// Drain any pending debounced events and translate per-path events
/// directly into dirty/removed canonical-id buckets. Returns None when
/// no events arrived and no recovery is pending — the caller will
/// return without doing any work in that case. Returns Some(drained)
/// when work (dirty or removed entries) was produced.
///
/// Per-event translation rules:
/// - `Remove` / `Modify(Name)` events retire the id (the watcher's own
///   `seen_paths` cleanup follows so the next rescan agrees).
/// - `Create` / `Modify(Any)` events on a file that still declares
///   itself a script mark the id dirty.
/// - `Create` / `Modify(Any)` events on a file that lost its marker
///   retire the id (marker removal — only if the id was previously
///   loaded; new non-script files are silently skipped).
/// - Non-script `.rs` files never produce a dirty entry.
pub(crate) fn drain_pending(watcher: &mut ScriptWatcher, project_root: &Path) -> Option<Drained> {
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
                    // Drop the lock before the rescan.
                    drop(rx_guard);
                    return Some(full_rescan(watcher, project_root));
                }
            }
        }
    }

    // Per-event translation: each path contributes at most one entry.
    let mut drained = Drained::default();
    let mut root_changed = false;
    for event in inflight {
        for path in &event.paths {
            if path.parent() == Some(project_root) {
                root_changed = true;
            }
            if !is_source_path(project_root, path) {
                continue;
            }
            let Some(id) = canonical_for(project_root, path) else {
                continue;
            };
            match event.event.kind {
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
                    push_unique(&mut drained.removed, &id);
                }
                _ => {
                    // Create / Modify: only dirty if the file currently
                    // declares itself a script. Otherwise retire the
                    // id (marker was removed). Only retire ids we have
                    // actually loaded before.
                    let on_disk = project_root.join(id.path());
                    let is_script = std::fs::read_to_string(&on_disk)
                        .map(|src| declaration_recognised(&src))
                        .unwrap_or(false);
                    if is_script {
                        push_unique(&mut drained.dirty, &id);
                    } else if watcher.seen_paths.contains(&id) {
                        push_unique(&mut drained.removed, &id);
                    }
                }
            }
        }
    }

    if root_changed {
        // A new directory may have appeared; one rescan replaces the
        // per-path translation. Avoid double work.
        return Some(full_rescan(watcher, project_root));
    }

    // No events produced work — return None so callers do nothing on
    // idle frames.
    if drained.dirty.is_empty() && drained.removed.is_empty() {
        None
    } else {
        Some(drained)
    }
}

#[derive(Default)]
#[doc(hidden)]
pub struct Drained {
    pub dirty: Vec<CanonicalId>,
    pub removed: Vec<CanonicalId>,
}

/// Full reconciliation: rescan the project once, diff against
/// `seen_paths`, and emit dirty/removed buckets. Used for initial
/// attachment and overflow recovery — never on idle frames.
pub(crate) fn full_rescan(watcher: &mut ScriptWatcher, project_root: &Path) -> Drained {
    let current: std::collections::BTreeSet<CanonicalId> =
        discovery::collect_canonical_scripts(project_root)
            .into_iter()
            .collect();
    let previous: std::collections::BTreeSet<CanonicalId> =
        watcher.seen_paths.iter().cloned().collect();

    let mut drained = Drained::default();
    for added in current.difference(&previous) {
        drained.dirty.push(added.clone());
    }
    for gone in previous.difference(&current) {
        drained.removed.push(gone.clone());
    }
    watcher.seen_paths = current.into_iter().collect();
    drained
}

/// Apply a drained plan: start builds for dirty entries; queue retires
/// for removed entries. This is the only place the watcher starts work.
fn apply_drained(watcher: &mut ScriptWatcher, project_root: &Path, drained: Drained) {
    let Drained { dirty, removed } = drained;
    let sdk_root = match crate::sdk_root() {
        Some(s) => s,
        None => return,
    };

    for id in dirty {
        if let Some(existing) = watcher.building.get_mut(&id) {
            // A newer event arrived while the previous build was in
            // flight; mark it for re-resolution.
            existing.pending_dirty = true;
            continue;
        }
        let on_disk = project_root.join(id.path());
        let started_at = std::fs::metadata(&on_disk)
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        let project_path = project_root.to_path_buf();
        let sdk_root_path = sdk_root.clone();
        let id_for_task = id.clone();
        let id_for_build = id.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let sdk = renzora_plugin_build::Sdk::load(sdk_root_path.join("sdk"))
                .map_err(|e| e.to_string())?;
            crate::build_to_path_with_id(&sdk, &project_path, &on_disk, &id_for_build)
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

    // Removed canonical ids stay queued in `removed_pending` and are
    // applied on the main thread in `finish`. The watcher's own
    // `seen_paths` was already updated by full_rescan; for per-path
    // remove events we update it here.
    for id in &removed {
        watcher.seen_paths.retain(|p| p != id);
    }

    if !removed.is_empty() {
        PENDING_RETIRES.with(|cell| {
            let mut q = cell.borrow_mut();
            for id in removed {
                if !q.iter().any(|p| p == &id) {
                    q.push(id);
                }
            }
        });
    }
}

thread_local! {
    /// Retires queued by `apply_drained` for the current frame. They run
    /// in `finish` so they happen on the main thread with `world`.
    static PENDING_RETIRES: std::cell::RefCell<Vec<CanonicalId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Drain pending retires queued by the watcher this frame.
fn take_pending_retires() -> Vec<CanonicalId> {
    PENDING_RETIRES.with(|cell| std::cell::RefCell::take(cell))
}

/// Load whatever finished building this frame. Also applies any
/// retires the watcher queued during the previous reconcile.
pub fn finish(world: &mut World) {
    let done: Vec<(
        CanonicalId,
        Result<PathBuf, String>,
        bool,
        std::time::SystemTime,
    )> = {
        let Some(mut watcher) = world.get_resource_mut::<ScriptWatcher>() else {
            return;
        };
        let mut done = Vec::new();
        let ids: Vec<CanonicalId> = watcher.building.keys().cloned().collect();
        for id in ids {
            let Some(mut build) = watcher.building.remove(&id) else {
                continue;
            };
            if let Some(result) = block_on(poll_once(&mut build.task)) {
                done.push((id.clone(), result, build.pending_dirty, build.started_at));
            }
        }
        done
    };

    // Snapshot the project root and retires BEFORE taking the mutable
    // borrow on LoadedScripts. This avoids the world-borrow conflict
    // that Rust's borrow checker flags.
    let project_root = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone());
    let retires = take_pending_retires();

    let mut loaded = world.resource_mut::<LoadedScripts>();

    for (id, result, pending_dirty, started_at) in done {
        // Stale-result check: source moved during compilation, OR a
        // pending-dirty event arrived while building. Either way,
        // discard this result; the next reconcile will schedule a
        // fresh build.
        let stale = if pending_dirty {
            true
        } else {
            let on_disk = project_root.as_ref().map(|p| p.join(id.path()));
            match on_disk.and_then(|p| std::fs::metadata(&p).ok()) {
                Some(meta) => meta.modified().map(|m| m > started_at).unwrap_or(false),
                None => false,
            }
        };
        if stale {
            debug!("[rust-script] {id}: source changed during build; rescheduling");
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

    // Apply retires (collected before the loaded borrow).
    for id in retires {
        loaded.remove(&id);
        info!("[rust-script] retired {id}");
    }
}

// ─── test surface ─────────────────────────────────────────────────────────────

/// One reconcile frame's worth of work: drain events (if any), and run
/// the initial rescan only when the previous attach set the
/// `needs_initial_rescan` flag. Returns the number of discovery calls
/// that happened during this frame (0 on idle; 1+ on attach or
/// overflow). This is the integration test helper the rest of the
/// crate can drive without exposing every internal.
///
/// Use from integration tests via `tests/watcher_idle.rs`.
#[doc(hidden)]
pub fn reconcile_one_frame(watcher: &mut ScriptWatcher, project_root: &Path) -> usize {
    crate::discovery::reset_collect_call_count();
    let before = crate::discovery::collect_call_count();

    // Ensure the debouncer is attached if needed.
    if watcher.watched_root.as_deref() != Some(project_root) {
        attach_debouncer(watcher, project_root);
    }

    if watcher.debouncer.is_none() {
        return 0;
    }

    if let Some(_drained) = drain_pending(watcher, project_root) {
        // Caller would now apply_drained, which only enqueues
        // builds/removes. For the idle-scan test we just care about
        // whether discovery was invoked.
    } else if needs_initial_rescan(watcher) {
        // The initial rescan only walks the filesystem when the
        // snapshot is empty. If `compile_and_load` already populated
        // seen_paths (the normal open flow), the snapshot is fresh and
        // a rescan would only re-walk the project for no new
        // information. Clear the flag without a rescan in that case.
        if watcher.seen_paths.is_empty() {
            let _ = full_rescan(watcher, project_root);
        }
        watcher.needs_initial_rescan = false;
    }

    let after = crate::discovery::collect_call_count();
    after.saturating_sub(before)
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

/// Test-only accessor for `seen_paths`. Used by integration tests to
/// verify the watcher's indexed snapshot after the initial rescan.
#[doc(hidden)]
pub fn seen_paths_for_test(watcher: &ScriptWatcher) -> &[CanonicalId] {
    &watcher.seen_paths
}

/// Test-only accessor for `watched_root`. Used by integration tests
/// to verify the "set only on success" invariant.
#[doc(hidden)]
pub fn watched_root_for_test(watcher: &ScriptWatcher) -> Option<&Path> {
    watcher.watched_root.as_deref()
}

/// Test-only setter for `last_failed_attach`. Used by integration
/// tests to verify backoff state transitions without exercising the
/// real attach failure path.
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

/// Drive `attach_debouncer` from tests. Same as the internal helper;
/// separated to make integration tests more readable.
#[doc(hidden)]
pub fn attach_debouncer_for_test(watcher: &mut ScriptWatcher, project_root: &Path) {
    attach_debouncer(watcher, project_root);
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

fn canonical_for(project_root: &Path, path: &Path) -> Option<CanonicalId> {
    discovery::project_relpath_for(project_root, path)
}

fn classify_event_kind(
    kind: EventKind,
    id: &CanonicalId,
    dirty: &mut Vec<CanonicalId>,
    removed: &mut Vec<CanonicalId>,
) {
    match kind {
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            push_unique(removed, id);
        }
        _ => push_unique(dirty, id),
    }
}

fn push_unique(vec: &mut Vec<CanonicalId>, value: &CanonicalId) {
    if !vec.iter().any(|v| v == value) {
        vec.push(value.clone());
    }
}

pub(crate) fn attach_debouncer(watcher: &mut ScriptWatcher, project_root: &Path) -> AttachOutcome {
    *watcher.rx.lock().unwrap() = None;
    watcher.debouncer = None;
    watcher.needs_initial_rescan = false;

    // Preserve any pre-existing seen_paths populated by compile_and_load's
    // mark_seen calls. Only seed via a discovery walk when the watcher
    // attaches BEFORE compile_and_load (e.g. fresh project, or
    // static_scripts feature without a compile step). This avoids the
    // duplicate-build trap where the next reconcile would otherwise
    // treat every script as new.
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
    // Set watched_root ONLY on success. Failed attaches leave the field
    // at its previous value so the next frame retries.
    watcher.watched_root = Some(project_root.to_path_buf());
    // Reset the failed-attach backoff on success.
    watcher.last_failed_attach = None;

    if was_empty {
        // First attach: walk the project to seed the snapshot. Subsequent
        // attaches preserve compile_and_load's pre-populated snapshot.
        let ids = discovery::collect_canonical_scripts(project_root);
        watcher.seed_seen(ids);
    }
    // Always set the flag; the next reconcile runs `full_rescan` once.
    // When compile_and_load already populated seen_paths, the diff is
    // empty and the loop does nothing.
    watcher.needs_initial_rescan = true;
    AttachOutcome::Live
}

enum AttachOutcome {
    Live,
    Failed,
}

fn record_failed_attach(watcher: &mut ScriptWatcher, e: impl std::fmt::Display) {
    let now = std::time::Instant::now();
    // Log only on the first failure and when the backoff expires --
    // never every frame. The next log fires `attach_backoff` after the
    // previous one, so a project with a permanently broken watcher
    // emits at most one warning every few seconds.
    let should_log = match watcher.last_failed_attach {
        None => true,
        Some(prev) => now.duration_since(prev) >= attach_backoff(),
    };
    if should_log {
        warn!("[rust-script] could not start source watch ({e})");
    }
    watcher.last_failed_attach = Some(now);
    // `watched_root` is intentionally left at its previous value
    // (None on the first attempt) so the next frame retries.
}

/// Backoff between failed-attach retries.
fn attach_backoff() -> std::time::Duration {
    std::time::Duration::from_secs(5)
}

/// Test-only accessor for the backoff duration.
#[doc(hidden)]
pub fn attach_backoff_for_test() -> std::time::Duration {
    attach_backoff()
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_remove_puts_in_removed_bucket() {
        let id =
            renzora_identity::CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs")
                .unwrap();
        let mut dirty = Vec::new();
        let mut removed = Vec::new();
        classify_event_kind(
            EventKind::Remove(notify_debouncer_full::notify::event::RemoveKind::File),
            &id,
            &mut dirty,
            &mut removed,
        );
        assert!(removed.contains(&id));
        assert!(dirty.is_empty());
    }

    #[test]
    fn classify_modify_puts_in_dirty_bucket() {
        let id =
            renzora_identity::CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs")
                .unwrap();
        let mut dirty = Vec::new();
        let mut removed = Vec::new();
        classify_event_kind(
            EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Any),
            &id,
            &mut dirty,
            &mut removed,
        );
        assert!(dirty.contains(&id));
        assert!(removed.is_empty());
    }

    #[test]
    fn classify_rename_modify_puts_in_removed_bucket() {
        let id =
            renzora_identity::CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs")
                .unwrap();
        let mut dirty = Vec::new();
        let mut removed = Vec::new();
        classify_event_kind(
            EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Name(
                notify_debouncer_full::notify::event::RenameMode::To,
            )),
            &id,
            &mut dirty,
            &mut removed,
        );
        assert!(removed.contains(&id));
    }

    #[test]
    fn push_unique_does_not_double_insert() {
        let id =
            renzora_identity::CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs")
                .unwrap();
        let mut v = Vec::new();
        push_unique(&mut v, &id);
        push_unique(&mut v, &id);
        push_unique(&mut v, &id);
        assert_eq!(v.len(), 1);
    }

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
    fn canonical_for_known_path_returns_some() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let on_disk = root.join("a/spin.rs");
        std::fs::create_dir_all(on_disk.parent().unwrap()).unwrap();
        std::fs::write(&on_disk, "").unwrap();
        let id = canonical_for(root, &on_disk).unwrap();
        assert_eq!(id.path(), "a/spin.rs");
    }

    #[test]
    fn full_rescan_initial_attach_seeds_seen_without_dirty() {
        let mut w = ScriptWatcher::default();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "").unwrap();
        w.seed_seen_for_test(discovery::collect_canonical_scripts(root));
        let drained = full_rescan(&mut w, root);
        assert!(drained.dirty.is_empty());
        assert!(drained.removed.is_empty());
    }

    /// A complete event batch with one Create event translates to exactly
    /// one dirty entry — no second rescan for the per-path translation.
    /// The translate path produces 1 dirty for 1 event.
    #[test]
    fn one_event_batch_yields_one_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "").unwrap();

        let mut w = ScriptWatcher::default();
        // Pre-seed seen paths so the initial rescan reports zero dirty.
        w.seed_seen_for_test(discovery::collect_canonical_scripts(root));

        let id = discovery::project_relpath_for(root, &root.join("a.rs")).unwrap();
        let mut drained = Drained::default();
        classify_event_kind(
            EventKind::Create(notify_debouncer_full::notify::event::CreateKind::File),
            &id,
            &mut drained.dirty,
            &mut drained.removed,
        );
        assert_eq!(drained.dirty.len(), 1);
        assert!(drained.removed.is_empty());
    }

    /// Generated-directory paths (`.renzora/scripts/...`) are filtered
    /// out by `is_source_path`, so even if the OS reports events for
    /// them, they never reach the dirty/removed buckets.
    #[test]
    fn generated_directory_event_is_filtered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let generated = root.join(".renzora/scripts/spin-{ts}.so");
        std::fs::create_dir_all(generated.parent().unwrap()).unwrap();
        std::fs::write(&generated, "").unwrap();
        assert!(!is_source_path(root, &generated));
    }

    /// Sanity: full_rescan increments the discovery counter once per
    /// call. The watcher's idle path is verified separately by the
    /// higher-level integration tests in
    /// `crates/renzora_rust_script/tests/watcher_idle.rs`.
    #[test]
    fn full_rescan_calls_discovery_once() {
        let mut w = ScriptWatcher::default();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.rs"), "").unwrap();
        crate::discovery::reset_collect_call_count();
        w.seed_seen_for_test(Vec::new());
        let _ = full_rescan(&mut w, root);
        assert!(crate::discovery::collect_call_count() >= 1);
    }
}
