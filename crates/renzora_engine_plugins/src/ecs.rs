//! Thin ECS adapter for the background Tier 2 pipeline.

use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use renzora::{
    CurrentProject, EnginePluginBuildRequest, EnginePluginBuildState, EnginePluginDiagnostic,
    EnginePluginDiagnosticLevel, EnginePluginDiagnostics, EnginePluginRestartRequest,
};

use crate::{
    prepare_engine_build, EngineBuildEvent, EngineBuildJob, EngineBuildPreparation,
    EngineBuildService,
};

/// Installed-editor inputs for Tier 2 reconciliation.
#[derive(Clone, Debug, Resource)]
pub struct EnginePluginEcsConfig {
    /// Preparation template; `plugins_root` is replaced by the open project.
    pub preparation: EngineBuildPreparation,
}

struct PreparationResult {
    revision: u64,
    result: Result<EngineBuildJob, String>,
}

struct PendingPreparation {
    revision: u64,
    preparation: EngineBuildPreparation,
}

struct Coordinator {
    builder: BuildWorker,
    next_revision: u64,
    latest_revision: u64,
    queued: Option<PendingPreparation>,
    preparing: Option<mpsc::Receiver<PreparationResult>>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            builder: BuildWorker::new(),
            next_revision: 1,
            latest_revision: 0,
            queued: None,
            preparing: None,
        }
    }
}

enum BuildWorkerCommand {
    Submit {
        revision: u64,
        job: Box<EngineBuildJob>,
    },
    Invalidate {
        revision: u64,
    },
    Shutdown,
}

struct BuildWorker {
    commands: mpsc::Sender<BuildWorkerCommand>,
    events: mpsc::Receiver<EngineBuildEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BuildWorker {
    fn new() -> Self {
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("engine_plugins.builder".to_string())
            .spawn(move || {
                let mut service = EngineBuildService::new();
                let mut visible_revision = 0;
                loop {
                    match command_receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(BuildWorkerCommand::Submit { revision, job }) => {
                            visible_revision = revision;
                            match service.submit(*job) {
                                Ok(events) => send_events(&event_sender, events, visible_revision),
                                Err(error) => {
                                    let _ = event_sender.send(EngineBuildEvent::Failed {
                                        revision: visible_revision,
                                        message: error.to_string(),
                                    });
                                }
                            }
                        }
                        Ok(BuildWorkerCommand::Invalidate { revision }) => {
                            visible_revision = revision;
                            send_events(&event_sender, service.invalidate(), visible_revision);
                        }
                        Ok(BuildWorkerCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    send_events(&event_sender, service.poll(), visible_revision);
                }
            })
            .expect("spawn Tier 2 build coordinator");
        Self {
            commands: command_sender,
            events: event_receiver,
            thread: Some(thread),
        }
    }

    fn invalidate(&self, revision: u64) {
        let _ = self
            .commands
            .send(BuildWorkerCommand::Invalidate { revision });
    }

    fn submit(&self, revision: u64, job: EngineBuildJob) -> Result<(), String> {
        self.commands
            .send(BuildWorkerCommand::Submit {
                revision,
                job: Box::new(job),
            })
            .map_err(|_| "Tier 2 build worker has stopped".to_string())
    }

    fn drain(&self) -> Vec<EngineBuildEvent> {
        self.events.try_iter().collect()
    }
}

impl Default for BuildWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BuildWorker {
    fn drop(&mut self) {
        let _ = self.commands.send(BuildWorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn send_events(
    sender: &mpsc::Sender<EngineBuildEvent>,
    events: Vec<EngineBuildEvent>,
    revision: u64,
) {
    for event in events {
        let event = with_revision(event, revision);
        if sender.send(event).is_err() {
            break;
        }
    }
}

fn with_revision(event: EngineBuildEvent, revision: u64) -> EngineBuildEvent {
    match event {
        EngineBuildEvent::Queued { .. } => EngineBuildEvent::Queued { revision },
        EngineBuildEvent::Building { step, .. } => EngineBuildEvent::Building { revision, step },
        EngineBuildEvent::Superseded { .. } => EngineBuildEvent::Superseded { revision },
        EngineBuildEvent::Failed { message, .. } => EngineBuildEvent::Failed { revision, message },
        EngineBuildEvent::Published { generation, .. } => EngineBuildEvent::Published {
            revision,
            generation,
        },
    }
}

#[derive(Resource, Default)]
struct EnginePluginCoordinator(Mutex<Coordinator>);

/// Editor-only ECS integration for restart-required engine plugins.
#[derive(Default)]
pub struct EnginePluginEcsPlugin;

impl Plugin for EnginePluginEcsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EnginePluginBuildState>()
            .init_resource::<EnginePluginDiagnostics>()
            .init_resource::<EnginePluginCoordinator>()
            .add_message::<EnginePluginBuildRequest>()
            .add_message::<EnginePluginRestartRequest>()
            .add_systems(
                Update,
                (
                    request_on_project_change,
                    queue_build_requests,
                    drain_build_pipeline,
                )
                    .chain(),
            );
    }
}

fn request_on_project_change(
    project: Option<Res<CurrentProject>>,
    mut previous: Local<Option<PathBuf>>,
    mut requests: MessageWriter<EnginePluginBuildRequest>,
) {
    let current = project.as_ref().map(|project| project.path.clone());
    if *previous == current {
        return;
    }
    *previous = current;
    if project.is_some() {
        requests.write(EnginePluginBuildRequest {
            plugin_id: None,
            reason: renzora::EnginePluginBuildReason::Reconcile,
        });
    }
}

fn queue_build_requests(
    mut requests: MessageReader<EnginePluginBuildRequest>,
    project: Option<Res<CurrentProject>>,
    config: Option<Res<EnginePluginEcsConfig>>,
    coordinator: Res<EnginePluginCoordinator>,
    mut state: ResMut<EnginePluginBuildState>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
) {
    let Some(_request) = requests.read().last() else {
        return;
    };
    let (Some(project), Some(config)) = (project, config) else {
        fail_state(
            &mut state,
            &mut diagnostics,
            0,
            "Tier 2 build inputs are unavailable for this editor installation",
        );
        return;
    };
    let Ok(mut coordinator) = coordinator.0.lock() else {
        fail_state(
            &mut state,
            &mut diagnostics,
            0,
            "Tier 2 build coordinator is unavailable",
        );
        return;
    };
    let revision = coordinator.next_revision;
    coordinator.next_revision = coordinator.next_revision.saturating_add(1);
    coordinator.latest_revision = revision;
    coordinator.builder.invalidate(revision);
    let mut preparation = config.preparation.clone();
    preparation.plugins_root = project.path.join("plugins");
    coordinator.queued = Some(PendingPreparation {
        revision,
        preparation,
    });
    *state = EnginePluginBuildState::Queued { revision };
}

fn drain_build_pipeline(
    coordinator: Res<EnginePluginCoordinator>,
    mut state: ResMut<EnginePluginBuildState>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
) {
    let Ok(mut coordinator) = coordinator.0.lock() else {
        return;
    };
    if let Some(receiver) = coordinator.preparing.take() {
        match receiver.try_recv() {
            Ok(prepared) if prepared.revision == coordinator.latest_revision => {
                match prepared.result {
                    Ok(job) => match coordinator.builder.submit(prepared.revision, job) {
                        Ok(()) => {}
                        Err(error) => fail_state(
                            &mut state,
                            &mut diagnostics,
                            prepared.revision,
                            &error.to_string(),
                        ),
                    },
                    Err(message) => {
                        fail_state(&mut state, &mut diagnostics, prepared.revision, &message)
                    }
                }
            }
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {}
            Err(mpsc::TryRecvError::Empty) => coordinator.preparing = Some(receiver),
        }
    }
    if coordinator.preparing.is_none() {
        if let Some(pending) = coordinator.queued.take() {
            let (sender, receiver) = mpsc::channel();
            let revision = pending.revision;
            let spawn = std::thread::Builder::new()
                .name(format!("engine_plugins.prepare.{revision}"))
                .spawn(move || {
                    let result = prepare_engine_build(&pending.preparation)
                        .map_err(|error| error.to_string());
                    let _ = sender.send(PreparationResult { revision, result });
                });
            match spawn {
                Ok(_) => {
                    coordinator.preparing = Some(receiver);
                    *state = EnginePluginBuildState::Building {
                        revision,
                        step: "validating and snapshotting engine plugins".to_string(),
                    };
                }
                Err(error) => fail_state(
                    &mut state,
                    &mut diagnostics,
                    revision,
                    &format!("could not start Tier 2 preparation: {error}"),
                ),
            }
        }
    }
    let revision = coordinator.latest_revision;
    let events = coordinator.builder.drain();
    apply_events(events, revision, &mut state, &mut diagnostics);
}

fn apply_events(
    events: Vec<EngineBuildEvent>,
    revision: u64,
    state: &mut EnginePluginBuildState,
    diagnostics: &mut EnginePluginDiagnostics,
) {
    for event in events {
        let event_revision = match &event {
            EngineBuildEvent::Queued { revision }
            | EngineBuildEvent::Building { revision, .. }
            | EngineBuildEvent::Superseded { revision }
            | EngineBuildEvent::Failed { revision, .. }
            | EngineBuildEvent::Published { revision, .. } => *revision,
        };
        if event_revision != revision {
            continue;
        }
        match event {
            EngineBuildEvent::Queued { .. } => {
                *state = EnginePluginBuildState::Queued { revision };
            }
            EngineBuildEvent::Building { step, .. } => {
                *state = EnginePluginBuildState::Building {
                    revision,
                    step: step.to_string(),
                };
            }
            EngineBuildEvent::Superseded { .. } => {}
            EngineBuildEvent::Failed { message, .. } => {
                fail_state(state, diagnostics, revision, &message);
            }
            EngineBuildEvent::Published { generation, .. } => {
                *state = EnginePluginBuildState::RestartReady {
                    generation: generation.manifest.generation,
                    stamp: generation.manifest.stamp.clone(),
                };
                diagnostics.push(EnginePluginDiagnostic {
                    plugin_id: None,
                    level: EnginePluginDiagnosticLevel::Info,
                    message: "Engine plugins are ready; restart when convenient".to_string(),
                    source: None,
                });
            }
        }
    }
}

fn fail_state(
    state: &mut EnginePluginBuildState,
    diagnostics: &mut EnginePluginDiagnostics,
    revision: u64,
    message: &str,
) {
    *state = EnginePluginBuildState::Failed {
        revision,
        message: message.to_string(),
    };
    diagnostics.push(EnginePluginDiagnostic {
        plugin_id: None,
        level: EnginePluginDiagnosticLevel::Error,
        message: message.to_string(),
        source: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_installation_config_fails_without_panicking_editor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        app.add_plugins(EnginePluginEcsPlugin);
        app.insert_resource(CurrentProject {
            path: temp.path().to_path_buf(),
            config: renzora::ProjectConfig::default(),
        });

        app.update();

        assert!(matches!(
            app.world().resource::<EnginePluginBuildState>(),
            EnginePluginBuildState::Failed { .. }
        ));
        let diagnostics = app.world().resource::<EnginePluginDiagnostics>();
        assert_eq!(diagnostics.entries.len(), 1);
        assert!(diagnostics.entries[0].message.contains("unavailable"));
    }

    #[test]
    fn published_event_becomes_restart_ready_and_notifies() {
        let mut state = EnginePluginBuildState::Idle;
        let mut diagnostics = EnginePluginDiagnostics::default();
        let generation = crate::PublishedEngineGeneration {
            root: PathBuf::from("generation"),
            manifest: crate::GenerationManifest {
                schema: renzora::ENGINE_PLUGIN_GENERATION_SCHEMA,
                generation: 7,
                stamp: renzora::EnginePluginGenerationStamp {
                    schema: renzora::ENGINE_PLUGIN_GENERATION_SCHEMA,
                    ..Default::default()
                },
                artifacts: Vec::new(),
                content_hash: "hash".into(),
            },
        };

        apply_events(
            vec![EngineBuildEvent::Published {
                revision: 3,
                generation: Box::new(generation),
            }],
            3,
            &mut state,
            &mut diagnostics,
        );

        assert!(matches!(
            state,
            EnginePluginBuildState::RestartReady { generation: 7, .. }
        ));
        assert_eq!(diagnostics.entries.len(), 1);
        assert_eq!(
            diagnostics.entries[0].level,
            EnginePluginDiagnosticLevel::Info
        );
    }
}
