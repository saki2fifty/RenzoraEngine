//! Keep the current editor alive until its explicitly selected replacement is ready.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

use bevy::prelude::*;
use renzora::{
    CurrentProject, EditorUnsavedWork, EnginePluginBuildState, EnginePluginDiagnostic,
    EnginePluginDiagnosticLevel, EnginePluginDiagnostics, EnginePluginRestartRequest,
    EnginePluginRunningGeneration,
};

use crate::replacement::{PendingReplacement, ReplacementProgress};

struct Attempt {
    request: EnginePluginRestartRequest,
    cancel: Arc<AtomicBool>,
    ready: Option<mpsc::Receiver<Result<PendingReplacement, String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // This runs on retirement workers, or synchronously during final world
        // teardown. A queued ready guard must be cancelled before exit returns.
        drop(self.ready.take());
    }
}

#[derive(Resource, Default)]
pub(crate) struct RestartCoordinator(
    Mutex<Option<Attempt>>,
    Mutex<Vec<std::thread::JoinHandle<()>>>,
);

impl Drop for RestartCoordinator {
    fn drop(&mut self) {
        if let Ok(active) = self.0.get_mut() {
            drop(active.take());
        }
        if let Ok(workers) = self.1.get_mut() {
            for worker in workers.drain(..) {
                let _ = worker.join();
            }
        }
    }
}

fn retire(coordinator: &RestartCoordinator, attempt: Option<Attempt>) {
    let Some(attempt) = attempt else {
        return;
    };
    attempt.cancel.store(true, Ordering::Release);
    if let Ok(worker) = std::thread::Builder::new()
        .name("engine_plugins.retire".into())
        .spawn(move || drop(attempt))
    {
        if let Ok(mut workers) = coordinator.1.lock() {
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    let _ = workers.swap_remove(index).join();
                } else {
                    index += 1;
                }
            }
            workers.push(worker);
        } else {
            let _ = worker.join();
        }
    }
}

fn matches_offer(
    request: &EnginePluginRestartRequest,
    project: Option<&Path>,
    state: &EnginePluginBuildState,
    unsaved: &EditorUnsavedWork,
) -> bool {
    project.is_some_and(|project| project == request.project)
        && unsaved.is_empty()
        && matches!(state, EnginePluginBuildState::RestartReady { generation, stamp }
            if *generation == request.generation && stamp == &request.stamp)
}

fn report(diagnostics: &mut EnginePluginDiagnostics, message: impl Into<String>) {
    diagnostics.push(EnginePluginDiagnostic {
        plugin_id: None,
        level: EnginePluginDiagnosticLevel::Warning,
        message: message.into(),
        source: None,
    });
}

/// Runs after every unsaved-work owner has reported for this frame.
pub(crate) fn gate(
    mut requests: MessageReader<EnginePluginRestartRequest>,
    coordinator: Res<RestartCoordinator>,
    project: Option<Res<CurrentProject>>,
    pending_project: Option<Res<renzora::EnginePluginPendingProject>>,
    config: Option<Res<crate::EnginePluginEcsConfig>>,
    state: Res<EnginePluginBuildState>,
    unsaved: Res<EditorUnsavedWork>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
    mut exit: MessageWriter<bevy::app::AppExit>,
) {
    let Ok(mut active) = coordinator.0.lock() else {
        return;
    };
    let project = renzora::engine_plugin_project(project.as_deref(), pending_project.as_deref());
    if let Some(attempt) = active.as_ref() {
        if !matches_offer(&attempt.request, project, &state, &unsaved) {
            // The attempt moves queued guards to background cleanup on drop.
            retire(&coordinator, active.take());
            report(
                &mut diagnostics,
                "Restart cancelled: the project, build, or unsaved work changed",
            );
        } else {
            match attempt
                .ready
                .as_ref()
                .expect("active attempt receiver")
                .try_recv()
            {
                Ok(Ok(mut replacement)) => {
                    match replacement.try_finish() {
                        Ok(mut child) => {
                            // The child is independent after handoff. Reap it
                            // if this process remains alive during shutdown.
                            let _ = std::thread::Builder::new()
                                .name("engine_plugins.reap".into())
                                .spawn(move || {
                                    let _ = child.wait();
                                });
                            exit.write(bevy::app::AppExit::Success);
                        }
                        Err(error) => {
                            report(&mut diagnostics, error.to_string());
                            cleanup(&coordinator, replacement);
                        }
                    }
                    retire(&coordinator, active.take());
                }
                Ok(Err(error)) => {
                    report(&mut diagnostics, error);
                    retire(&coordinator, active.take());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    report(
                        &mut diagnostics,
                        "Restart worker stopped; keeping this editor open",
                    );
                    retire(&coordinator, active.take());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }
    let Some(request) = requests.read().last() else {
        return;
    };
    if active.is_some() {
        return;
    }
    if !matches_offer(request, project, &state, &unsaved) {
        report(
            &mut diagnostics,
            "Save all edited documents before restarting; an outdated restart offer cannot be used",
        );
        return;
    }
    let Some(config) = config else {
        return;
    };
    let cache = config.preparation.cache_root.clone();
    let kit = config.preparation.build_kit_root.clone();
    let request = request.clone();
    let requested = request.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancelled = cancel.clone();
    let (sender, ready) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("engine_plugins.restart".into())
        .spawn(move || {
            let result = (|| {
                let mut replacement = PendingReplacement::launch(
                    &cache,
                    requested.generation,
                    &requested.stamp,
                    Duration::from_secs(120),
                    |cache, token| {
                        vec![
                            OsString::from("--project"),
                            requested.project.into_os_string(),
                            OsString::from("--engine-build-kit"),
                            kit.into_os_string(),
                            OsString::from("--engine-startup-cache"),
                            cache.as_os_str().to_owned(),
                            OsString::from("--engine-startup-token"),
                            OsString::from(token),
                        ]
                    },
                )
                .map_err(|error| error.to_string())?;
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        return Err("Restart cancelled; keeping this editor open".into());
                    }
                    match replacement.poll().map_err(|error| error.to_string())? {
                        ReplacementProgress::Ready(_) => return Ok(replacement),
                        ReplacementProgress::Pending => {
                            std::thread::sleep(Duration::from_millis(25))
                        }
                    }
                }
            })();
            // A disconnected frame receiver drops/cancels the guard on this worker.
            let _ = sender.send(result);
        });
    match spawn {
        Ok(worker) => {
            *active = Some(Attempt {
                request,
                cancel,
                ready: Some(ready),
                worker: Some(worker),
            })
        }
        Err(error) => report(
            &mut diagnostics,
            format!("Could not start restart worker: {error}"),
        ),
    }
}

fn cleanup(coordinator: &RestartCoordinator, replacement: PendingReplacement) {
    // A failed thread spawn drops the closure (and its guard), preserving
    // correctness even if cleanup must fall back to the calling thread.
    if let Ok(worker) = std::thread::Builder::new()
        .name("engine_plugins.cancel".into())
        .spawn(move || {
            drop(replacement);
        })
    {
        if let Ok(mut workers) = coordinator.1.lock() {
            workers.push(worker);
        } else {
            let _ = worker.join();
        }
    }
}

#[derive(Resource, Default)]
pub(crate) struct StartupAcknowledgement {
    attempted: bool,
    ready_frames: u8,
    result: Mutex<Option<mpsc::Receiver<Result<PathBuf, String>>>>,
}

/// A process is not ready merely because its main function ran.
pub(crate) fn acknowledge(
    project: Option<Res<CurrentProject>>,
    splash: Option<Res<State<renzora::SplashState>>>,
    windows: Query<&Window>,
    running: Option<Res<EnginePluginRunningGeneration>>,
    mut pending: ResMut<StartupAcknowledgement>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
    mut trust: ResMut<renzora::EnginePluginTrust>,
    mut builds: MessageWriter<renzora::EnginePluginBuildRequest>,
) {
    let completed = pending.result.get_mut().ok().and_then(|receiver| {
        receiver
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Startup confirmation worker stopped".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            })
    });
    if let Some(result) = completed {
        if let Ok(receiver) = pending.result.get_mut() {
            *receiver = None;
        }
        match result {
            Ok(approved)
                if project
                    .as_ref()
                    .is_some_and(|project| project.path == approved) =>
            {
                trust.project = Some(approved);
                builds.write(renzora::EnginePluginBuildRequest {
                    plugin_id: None,
                    reason: renzora::EnginePluginBuildReason::Reconcile,
                });
            }
            Ok(_) => report(
                &mut diagnostics,
                "Project changed during startup confirmation",
            ),
            Err(error) => report(&mut diagnostics, error),
        }
    }
    if pending.attempted {
        return;
    }
    let argument = |name: &str| std::env::args_os().skip_while(|arg| arg != name).nth(1);
    let Some(token) = argument("--engine-startup-token") else {
        pending.attempted = true;
        return;
    };
    let Some(project) = project else {
        return;
    };
    if splash
        .as_ref()
        .is_none_or(|state| *state.get() != renzora::SplashState::Editor)
        || windows.is_empty()
    {
        pending.ready_frames = 0;
        return;
    }
    pending.ready_frames += 1;
    if pending.ready_frames < 3 {
        return;
    }
    pending.attempted = true;
    let Some(stamp) = running.and_then(|running| running.0.clone()) else {
        report(
            &mut diagnostics,
            "Replacement has no embedded build identity",
        );
        return;
    };
    let Some(cache) = argument("--engine-startup-cache") else {
        report(&mut diagnostics, "Replacement has no startup cache");
        return;
    };
    let Some(expected_project) = argument("--project") else {
        return;
    };
    let opened = project.path.clone();
    let (sender, receiver) = mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("engine_plugins.acknowledge".into())
        .spawn(move || {
            let result = (|| {
                if std::fs::canonicalize(expected_project).map_err(|error| error.to_string())?
                    != std::fs::canonicalize(&opened).map_err(|error| error.to_string())?
                {
                    return Err("Replacement opened a different project".into());
                }
                crate::startup::acknowledge_startup(
                    &PathBuf::from(cache),
                    token.to_str().ok_or("Invalid startup token")?,
                    &stamp,
                )
                .map_err(|error| error.to_string())?;
                Ok(opened)
            })();
            let _ = sender.send(result);
        });
    match spawn {
        Ok(_) => {
            if let Ok(pending) = pending.result.get_mut() {
                *pending = Some(receiver);
            }
        }
        Err(error) => report(&mut diagnostics, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_requires_exact_project_stamp_and_no_unsaved_owner() {
        let request = EnginePluginRestartRequest {
            project: PathBuf::from("project-a"),
            generation: 3,
            stamp: renzora::EnginePluginGenerationStamp::default(),
        };
        let mut project = CurrentProject {
            path: request.project.clone(),
            config: Default::default(),
        };
        let state = EnginePluginBuildState::RestartReady {
            generation: 3,
            stamp: request.stamp.clone(),
        };
        let mut unsaved = EditorUnsavedWork::default();
        assert!(matches_offer(
            &request,
            Some(&project.path),
            &state,
            &unsaved
        ));
        unsaved.report("code", 1);
        assert!(!matches_offer(
            &request,
            Some(&project.path),
            &state,
            &unsaved
        ));
        unsaved.report("code", 0);
        project.path = PathBuf::from("project-b");
        assert!(!matches_offer(
            &request,
            Some(&project.path),
            &state,
            &unsaved
        ));
        project.path = request.project.clone();
        let mut changed = request.stamp.clone();
        changed.integration_hash = "new".into();
        assert!(!matches_offer(
            &request,
            Some(&project.path),
            &EnginePluginBuildState::RestartReady {
                generation: 3,
                stamp: changed
            },
            &unsaved
        ));
    }
}
