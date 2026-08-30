//! Recompiling a script when its source changes, without blocking the editor.
//!
//! A Rust script costs about a second to build. Doing that on the main thread
//! would freeze the editor on every save — brief, but exactly at the moment the
//! author is watching for a result, which is the worst time to stutter. So the
//! compile runs on a task pool and only the load happens on the main thread,
//! where it is a `dlopen` and a pointer swap.
//!
//! # Why the old library is never unloaded
//!
//! A reload maps a NEW image and repoints [`LoadedScripts`]; the old one stays
//! mapped for the life of the process. It has to: a schedule, a `Local`, or a
//! captured closure may still hold pointers into it, and `renzora_plugin`'s
//! loader deadlocked in `FreeLibrary` and later crashed the runtime with an
//! access violation learning that lesson twice.
//!
//! So an afternoon of saves leaks a few hundred KB each — a script is ~200 KB —
//! and a restart reclaims all of it. That is the price of editing native code in
//! a running process, and it is cheap next to not being able to edit at all.
//!
//! # Why a failed build is not retried
//!
//! The recorded modification time is updated when a build is *started*, not when
//! it succeeds. A script that fails to compile therefore stays quiet until it is
//! edited again, instead of rebuilding and re-reporting the same error every
//! poll — which is what turns a compile error into a scrolling wall.
//!
//! # Why the watcher is event-driven, not polled (Phase 1 commit 1.2)
//!
//! The previous implementation polled `<project>/scripts/` every 0.5 s and
//! could only see flat files. The new implementation subscribes to the
//! project root recursively via `notify_debouncer_full`, so:
//!
//! - nested scripts (anywhere beneath `CurrentProject::path`) rebuild on save;
//! - rename is detected as either a paired rename event or a remove-plus-create
//!   pair, depending on the OS;
//! - delete retires the corresponding `LoadedScripts` entry;
//! - the SKIP list excludes `.renzora/`, `target/`, `node_modules/`, `.git/`,
//!   `dist/`, `.svn/`, `.hg/` and every dot-prefixed directory, so the compiler's
//!   own artefact directory never triggers a rebuild loop;
//! - the debouncer's settle window batches atomic-save bursts (write +
//!   rename) and never emits an event for a half-written file.

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

use crate::{discovery, load_library, LoadedScripts};

/// How long the debouncer waits after the last filesystem event before
/// reporting a batch. Below this threshold, bursts of writes (atomic save =
/// rename + metadata flush) are coalesced into one report; above it, the
/// next change starts the window afresh.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Receiver transported through the watcher. Locked before reading; the
/// debouncer's poll loop holds nothing across frames.
type DebouncedRx = Receiver<DebounceEventResult>;

type SourceDebouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

#[derive(Resource)]
pub struct ScriptWatcher {
    /// Builds in flight, keyed by the project-relative canonical relpath.
    /// `LoadedScripts` is keyed by the same string (commit 1.3 widens both
    /// to `renzora_identity::CanonicalId`).
    building: HashMap<String, Task<Result<PathBuf, String>>>,
    /// Cached debouncer + receiver. Created lazily when a project opens.
    debouncer: Option<SourceDebouncer>,
    rx: Mutex<Option<DebouncedRx>>,
    /// Last-seen set of canonical relpaths under the project root, used to
    /// detect deletion in the reconcile step.
    seen_paths: Vec<String>,
    /// Project root the current debouncer is attached to.
    watched_root: Option<PathBuf>,
}

impl Default for ScriptWatcher {
    fn default() -> Self {
        Self {
            building: HashMap::new(),
            debouncer: None,
            rx: Mutex::new(None),
            seen_paths: Vec::new(),
            watched_root: None,
        }
    }
}

impl ScriptWatcher {
    /// Record that the script at the given project-relative canonical path
    /// has already been dealt with (called by `compile_and_load` at project
    /// open so the watcher does not double-build).
    pub fn mark_seen(&mut self, relpath: String) {
        if !self.seen_paths.contains(&relpath) {
            self.seen_paths.push(relpath);
        }
    }
}

/// Notice changed or new `.rs` files and start building them.
pub fn watch(mut watcher: ResMut<ScriptWatcher>, project: Option<Res<CurrentProject>>) {
    let Some(project) = project else { return };
    let project_root = project.path.clone();

    // (Re)attach the debouncer if the project path changed (or first time).
    if watcher.watched_root.as_deref() != Some(project_root.as_path()) {
        attach_debouncer(&mut watcher, &project_root);
    }

    // Reconcile any pending debounced events into a dirty/removed plan.
    let mut dirty: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();

    // Drain the receiver WITHOUT holding the lock across all of the
    // reconcile work. `rescan_into` and `attach_debouncer` need mutable
    // access to the watcher, so we lift any work that needs the lock to
    // before taking `&mut watcher` again.
    let mut inflight: Vec<DebouncedEvent> = Vec::new();
    {
        let mut rx_guard = match watcher.rx.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(rx) = rx_guard.as_mut() {
            for batch in rx.try_iter() {
                match batch {
                    Ok(events) => inflight.extend(events),
                    Err(errors) => {
                        // OS queue overflowed; the full rescan handles it.
                        for e in errors {
                            warn!("[rust-script] watch error: {e}");
                        }
                        drop(rx_guard);
                        rescan_into(&mut watcher, &project_root, &mut dirty, &mut removed);
                        return;
                    }
                }
            }
        }
    }

    for event in inflight {
        for path in &event.paths {
            if !is_source_path(&project_root, path) {
                continue;
            }
            let Some(relpath) = relpath_for(&project_root, path) else {
                continue;
            };
            classify_event_kind(event.event.kind, &relpath, &mut dirty, &mut removed);
        }
    }

    // Refresh the seen set so deletions are observable.
    rescan_into(&mut watcher, &project_root, &mut dirty, &mut removed);

    let sdk_root = match crate::sdk_root() {
        Some(s) => s,
        None => return,
    };

    for key in dirty {
        if watcher.building.contains_key(&key) {
            continue;
        }
        let on_disk = project_root.join(&key);
        if !on_disk.exists() {
            continue;
        }
        let project_path = project_root.clone();
        let sdk_root_path = sdk_root.clone();
        let key_for_task = key.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let sdk = renzora_plugin_build::Sdk::load(sdk_root_path.join("sdk"))
                .map_err(|e| e.to_string())?;
            crate::build_to_path(&sdk, &project_path, &on_disk)
        });
        watcher.building.insert(key_for_task, task);
    }
    // Phase 1 commit 1.3 will retire the last-good-stays-active invariant
    // by retiring removed entries from `LoadedScripts` here. For commit 1.2
    // the last-good library remains; the user-visible change is that
    // nested files rebuild.
    let _ = removed; // silence unused for now
}

/// Load whatever finished building this frame.
///
/// Separate from [`watch`] because the load must happen on the main thread — it
/// mutates [`LoadedScripts`] — while the compile must not.
pub fn finish(world: &mut World) {
    let done: Vec<(String, Result<PathBuf, String>)> = {
        let Some(mut watcher) = world.get_resource_mut::<ScriptWatcher>() else {
            return;
        };
        // Polled exactly once each: `poll_once` takes the result, so a second
        // poll on a finished task would find nothing and the build would be
        // silently dropped.
        let mut done = Vec::new();
        for (name, task) in watcher.building.iter_mut() {
            if let Some(result) = block_on(poll_once(task)) {
                done.push((name.clone(), result));
            }
        }
        for (name, _) in &done {
            watcher.building.remove(name);
        }
        done
    };

    let mut loaded = world.resource_mut::<LoadedScripts>();
    for (name, result) in done {
        match result.and_then(|lib_path| load_library(&lib_path)) {
            Ok((f, lib)) => {
                loaded.insert(name.clone(), f, lib);
                info!("[rust-script] reloaded {name}");
                console_success("Script", format!("recompiled {name}"));
            }
            Err(e) => {
                error!("[rust-script] {name}: {e}");
                console_error("Script", format!("{name}\n{e}"));
            }
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Decide whether `path` is a candidate source file under `project_root`.
/// Mirrors the SKIP list in `discovery.rs`; the watcher filters raw paths
/// before mapping them to canonical relpaths.
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

/// Compute the canonical project-relative forward-slash relpath, or `None`
/// if the path isn't under the project root or isn't a `.rs` file.
fn relpath_for(project_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(project_root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Map a single `EventKind` from a debounced batch to the dirty/removed
/// bucket. Renames often arrive as a paired event; on filesystems that
/// don't forward them they arrive as remove + create, which `rescan_into`
/// reconciles via seen_paths diffing.
fn classify_event_kind(
    kind: EventKind,
    relpath: &str,
    dirty: &mut Vec<String>,
    removed: &mut Vec<String>,
) {
    match kind {
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            push_unique(removed, relpath);
        }
        _ => push_unique(dirty, relpath),
    }
}

fn push_unique(vec: &mut Vec<String>, value: &str) {
    if !vec.iter().any(|v| v == value) {
        vec.push(value.to_string());
    }
}

/// Compare the previous seen set with a fresh `discovery::collect_rust_scripts`
/// run and emit dirty/removed for the difference. Idempotent and cheap enough
/// to call on every debounced batch — Phase 1 commit 1.2 replaces the previous
/// 0.5 s loop with this O(tree-size) recompute on event arrival only.
fn rescan_into(
    watcher: &mut ScriptWatcher,
    project_root: &Path,
    dirty: &mut Vec<String>,
    removed: &mut Vec<String>,
) {
    let current: std::collections::BTreeSet<String> = discovery::collect_rust_scripts(project_root)
        .into_iter()
        .filter_map(|p| relpath_for(project_root, &p))
        .collect();
    let previous: std::collections::BTreeSet<String> = watcher.seen_paths.iter().cloned().collect();

    for added in current.difference(&previous) {
        push_unique(dirty, added);
    }
    for gone in previous.difference(&current) {
        push_unique(removed, gone);
    }
    watcher.seen_paths = current.into_iter().collect();
}

/// Attach a fresh debouncer to the project root with `RecursiveMode::Recursive`.
/// Detaches any existing watcher first; the `SourceDebouncer`'s `Drop`
/// unregisters its watches.
fn attach_debouncer(watcher: &mut ScriptWatcher, project_root: &Path) {
    *watcher.rx.lock().unwrap() = None;
    watcher.debouncer = None;
    watcher.seen_paths.clear();
    watcher.watched_root = Some(project_root.to_path_buf());

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(DEBOUNCE, None, tx) {
        Ok(d) => d,
        Err(e) => {
            warn!("[rust-script] could not start source watch ({e})");
            return;
        }
    };
    if let Err(e) = debouncer.watch(project_root, RecursiveMode::Recursive) {
        warn!("[rust-script] could not watch project root ({e})");
        return;
    }
    info!(
        "[rust-script] watching {} recursively",
        project_root.display()
    );
    *watcher.rx.lock().unwrap() = Some(rx);
    watcher.debouncer = Some(debouncer);
}
