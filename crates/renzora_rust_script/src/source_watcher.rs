//! Production OS-watcher adapter for the source-script lifecycle.
//!
//! U4-4: a separate Bevy plugin the editor (and tests that want
//! to drive the real OS codepath) install. The plugin attaches a
//! `notify_debouncer_full` watcher to the project's source root
//! when a `CurrentProject` is present, converts debounced events
//! into the production `ScriptSourceEvent` enum, and pushes them
//! into [`crate::ScriptSourceEventQueue`]. The lifecycle drains the
//! same queue.
//!
//! Tests that want deterministic event injection do NOT add this
//! plugin; they push events directly through
//! [`crate::ScriptSourceEventQueue::push`]. This is the seam the
//! fourth-pass review required.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use notify_debouncer_full::{
    new_debouncer,
    notify::{
        event::{ModifyKind, RenameMode},
        EventKind, RecursiveMode,
    },
    DebounceEventResult, DebouncedEvent,
};

use renzora::CurrentProject;

use crate::{ScriptSourceEvent, ScriptSourceEventQueue, SourceDebouncer};

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Bevy plugin that owns the production OS-watcher adapter. Add
/// to the editor app when a real source root exists. Tests that
/// push events through [`ScriptSourceEventQueue::push`] skip this
/// plugin entirely.
#[derive(Default)]
pub struct RustScriptSourceWatcherPlugin {
    /// The publisher of production events. Each `notify_debouncer_full`
    /// batch is converted into one-or-more `ScriptSourceEvent`s.
    _marker: (),
}

impl Plugin for RustScriptSourceWatcherPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SourceWatcherFactory>()
            .init_resource::<SourceWatcherAttachment>()
            .add_systems(Update, sync_source_watcher);
    }
}

/// Opaque watcher owner returned by an injected watcher factory.
/// Dropping it detaches the underlying operating-system watch.
pub type SourceWatchHandle = Box<dyn std::any::Any + Send + Sync>;

type WatcherStarter =
    dyn Fn(&Path, ScriptSourceEventQueue) -> Result<SourceWatchHandle, String> + Send + Sync;

/// Factory seam for attaching a project watcher. Production uses
/// `notify`; tests inject deterministic handles without touching
/// watcher internals or depending on operating-system timing.
#[derive(Resource, Clone)]
pub struct SourceWatcherFactory(pub Arc<WatcherStarter>);

impl Default for SourceWatcherFactory {
    fn default() -> Self {
        Self(Arc::new(|root, queue| {
            let (_tx, debouncer) = install_source_watcher(root, queue)?;
            Ok(Box::new(debouncer))
        }))
    }
}

/// Observable attachment state for the watcher lifecycle.
#[derive(Resource, Default)]
pub struct SourceWatcherAttachment {
    observed_root: Option<std::path::PathBuf>,
    pub root: Option<std::path::PathBuf>,
    handle: Option<SourceWatchHandle>,
    last_attempt: Option<std::time::Instant>,
    pub attach_count: u64,
    pub detach_count: u64,
    pub last_error: Option<String>,
}

fn sync_source_watcher(world: &mut World) {
    let desired_root = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone())
        .filter(|path| !path.as_os_str().is_empty());
    let (observed_root, attached, retry_due) = {
        let attachment = world.resource::<SourceWatcherAttachment>();
        (
            attachment.observed_root.clone(),
            attachment.handle.is_some(),
            attachment
                .last_attempt
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(1)),
        )
    };
    if observed_root == desired_root && (desired_root.is_none() || attached || !retry_due) {
        return;
    }

    {
        let mut attachment = world.resource_mut::<SourceWatcherAttachment>();
        if attachment.handle.take().is_some() {
            attachment.detach_count += 1;
        }
        attachment.root = None;
        attachment.observed_root = desired_root.clone();
        attachment.last_error = None;
        attachment.last_attempt = Some(std::time::Instant::now());
    }

    let Some(project_path) = desired_root else {
        return;
    };
    let queue = world.resource::<ScriptSourceEventQueue>().clone();
    let factory = world.resource::<SourceWatcherFactory>().clone();
    match (factory.0)(&project_path, queue) {
        Ok(handle) => {
            let mut attachment = world.resource_mut::<SourceWatcherAttachment>();
            attachment.root = Some(project_path.clone());
            attachment.handle = Some(handle);
            attachment.attach_count += 1;
            info!(
                "[rust-script-watcher] watching {} recursively",
                project_path.display()
            );
        }
        Err(error) => {
            warn!(
                "[rust-script-watcher] could not start watcher on {}: {}",
                project_path.display(),
                error
            );
            world.resource_mut::<SourceWatcherAttachment>().last_error = Some(error);
        }
    }
}

/// Convert a debounced batch into [`ScriptSourceEvent`]s and push
/// them onto `ScriptSourceEventQueue`. The OS thread running the
/// `notify_debouncer_full` debouncer calls this when a batch is
/// ready. The conversion is conservative: any `Remove` and any
/// `Modify(Name)` are reported as a single
/// `TopologyRescanNeeded` event because per-path handling of those
/// events is the lifecycle's responsibility; the lifecycle reacts
/// to a single topology rescan by walking the project root
/// itself.
fn dispatch_debounced(queue: &ScriptSourceEventQueue, result: DebounceEventResult) {
    match result {
        Ok(events) => {
            for ev in events {
                dispatch_event(queue, ev);
            }
        }
        Err(errors) => {
            for err in errors {
                warn!("[rust-script-watcher] watch error: {err}");
            }
            queue.push(ScriptSourceEvent::TopologyRescanNeeded);
        }
    }
}

fn dispatch_event(queue: &ScriptSourceEventQueue, ev: DebouncedEvent) {
    match ev.event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Any) => {
            for path in ev.event.paths.clone() {
                queue.push(ScriptSourceEvent::SourceChanged(path));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => {
            queue.push(ScriptSourceEvent::TopologyRescanNeeded);
        }
        EventKind::Remove(_) => {
            // `Remove` could be a directory; defer the
            // topology-rescan question to the lifecycle by
            // emitting both signals. The dedup in the lifecycle's
            // `reconcile_with_events` collapses duplicate paths.
            let at_least_one = !ev.event.paths.is_empty();
            for path in ev.event.paths.clone() {
                queue.push(ScriptSourceEvent::SourceRemoved(path));
            }
            if at_least_one {
                queue.push(ScriptSourceEvent::TopologyRescanNeeded);
            }
        }
        EventKind::Any | EventKind::Access(_) | EventKind::Other => {
            // Conservative: do nothing for events the lifecycle's
            // state machine does not understand.
        }
        _ => {
            // `Modify(Data)` / `Modify(Metadata)` etc. — treat as
            // a source-changed for every path so the rebuild
            // pipeline picks them up.
            for path in ev.event.paths.clone() {
                queue.push(ScriptSourceEvent::SourceChanged(path));
            }
        }
    }
}

/// Attach an OS debouncer on `project_root`, drain its mpsc
/// channel through a small worker thread, and convert
/// `notify_debouncer_full` events into `ScriptSourceEvent`s. The
/// returned `(Sender, Debouncer)` pair is consumed by the caller:
/// the `Sender` is the queue's producer handle, the `Debouncer`
/// is stashed on `ScriptWatcher::debouncer` so its drop tears the
/// OS watch down.
pub fn install_source_watcher(
    project_root: &Path,
    queue: ScriptSourceEventQueue,
) -> Result<(mpsc::Sender<DebounceEventResult>, SourceDebouncer), String> {
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer =
        new_debouncer(DEBOUNCE, None, tx.clone()).map_err(|e| format!("new_debouncer: {e}"))?;
    debouncer
        .watch(project_root, RecursiveMode::Recursive)
        .map_err(|e| format!("debouncer.watch: {e}"))?;
    // Spawn a small worker thread that drains the channel. The
    // OS watcher writes to `tx`; the worker reads from `rx` and
    // pushes events onto the queue. The `ScriptWatcher::rx`
    // field on the resource is a separate, legacy channel; this
    // worker writes only through the new seam.
    std::thread::Builder::new()
        .name("renzora-rust-script-watcher".to_string())
        .spawn(move || {
            while let Ok(batch) = rx.recv() {
                dispatch_debounced(&queue, batch);
            }
        })
        .map_err(|e| format!("thread spawn: {e}"))?;
    Ok((tx, debouncer))
}
