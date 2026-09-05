//! Thin ECS adapter for the background Tier 2 pipeline.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

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
    result: Result<Prepared, String>,
}

enum Prepared {
    Unchanged,
    AwaitingTrust,
    Build(Box<EngineBuildJob>),
}

struct PendingPreparation {
    revision: u64,
    preparation: EngineBuildPreparation,
    not_before: Instant,
    trusted: bool,
    running: Option<renzora::EnginePluginGenerationStamp>,
    force_restart: bool,
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
    Invalidate,
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
                let mut revisions = BTreeMap::new();
                loop {
                    let command = if service.is_busy() {
                        command_receiver.recv_timeout(Duration::from_millis(10))
                    } else {
                        command_receiver
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    };
                    match command {
                        Ok(BuildWorkerCommand::Submit { revision, job }) => {
                            match service.submit(*job) {
                                Ok(events) => {
                                    for event in &events {
                                        if let EngineBuildEvent::Queued {
                                            revision: service_revision,
                                        } = event
                                        {
                                            revisions.insert(*service_revision, revision);
                                        }
                                    }
                                    send_events(&event_sender, events, &mut revisions);
                                }
                                Err(error) => {
                                    let _ = event_sender.send(EngineBuildEvent::Failed {
                                        revision,
                                        message: error.to_string(),
                                    });
                                }
                            }
                        }
                        Ok(BuildWorkerCommand::Invalidate) => {
                            send_events(&event_sender, service.invalidate(), &mut revisions);
                        }
                        Ok(BuildWorkerCommand::Shutdown)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    send_events(&event_sender, service.poll(), &mut revisions);
                }
            })
            .ok();
        Self {
            commands: command_sender,
            events: event_receiver,
            thread,
        }
    }

    fn invalidate(&self) {
        let _ = self.commands.send(BuildWorkerCommand::Invalidate);
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
    revisions: &mut BTreeMap<u64, u64>,
) {
    for event in events {
        let service_revision = match &event {
            EngineBuildEvent::Queued { revision }
            | EngineBuildEvent::Building { revision, .. }
            | EngineBuildEvent::Superseded { revision }
            | EngineBuildEvent::Failed { revision, .. }
            | EngineBuildEvent::Published { revision, .. } => *revision,
        };
        let Some(&revision) = revisions.get(&service_revision) else {
            continue;
        };
        if matches!(
            &event,
            EngineBuildEvent::Superseded { .. }
                | EngineBuildEvent::Failed { .. }
                | EngineBuildEvent::Published { .. }
        ) {
            revisions.remove(&service_revision);
        }
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
            .init_resource::<renzora::EnginePluginShutdownGuard>()
            .init_resource::<renzora::EditorUnsavedWork>()
            .init_resource::<crate::restart::RestartCoordinator>()
            .init_resource::<crate::restart::StartupAcknowledgement>()
            .add_message::<bevy::app::AppExit>()
            .add_systems(
                Last,
                crate::restart::gate.in_set(renzora::EnginePluginRestartGate),
            )
            .init_resource::<crate::installation::PendingInstallation>()
            .add_systems(Startup, crate::installation::begin)
            .init_resource::<renzora::EnginePluginTrust>()
            .add_message::<renzora::EnginePluginTrustRequest>()
            .init_resource::<EnginePluginDiagnostics>()
            .init_resource::<EnginePluginCoordinator>()
            .init_resource::<crate::watch::EnginePluginWatch>()
            .add_message::<EnginePluginBuildRequest>()
            .add_message::<EnginePluginRestartRequest>()
            .add_systems(
                Update,
                (
                    crate::installation::drain,
                    request_on_project_change,
                    crate::watch::reconcile_watch,
                    crate::watch::drain_watch,
                    apply_trust_request,
                    queue_build_requests,
                    drain_build_pipeline,
                    crate::restart::acknowledge,
                )
                    .chain(),
            );
    }
}

fn apply_trust_request(
    mut requests: MessageReader<renzora::EnginePluginTrustRequest>,
    project: Option<Res<CurrentProject>>,
    pending: Option<Res<renzora::EnginePluginPendingProject>>,
    mut trust: ResMut<renzora::EnginePluginTrust>,
    mut builds: MessageWriter<EnginePluginBuildRequest>,
) {
    let Some(project) = renzora::engine_plugin_project(project.as_deref(), pending.as_deref())
    else {
        requests.clear();
        trust.project = None;
        return;
    };
    let Some(request) = requests
        .read()
        .filter(|request| request.project == project)
        .last()
    else {
        return;
    };
    trust.project = request.trusted.then(|| project.to_path_buf());
    builds.write(EnginePluginBuildRequest {
        plugin_id: None,
        reason: renzora::EnginePluginBuildReason::UserRequested,
    });
}

fn request_on_project_change(
    project: Option<Res<CurrentProject>>,
    pending: Option<Res<renzora::EnginePluginPendingProject>>,
    mut previous: Local<Option<PathBuf>>,
    mut requests: MessageWriter<EnginePluginBuildRequest>,
) {
    let current = renzora::engine_plugin_project(project.as_deref(), pending.as_deref())
        .map(|path| path.to_path_buf());
    if *previous == current {
        return;
    }
    *previous = current;
    requests.write(EnginePluginBuildRequest {
        plugin_id: None,
        reason: renzora::EnginePluginBuildReason::Reconcile,
    });
}

fn queue_build_requests(
    mut requests: MessageReader<EnginePluginBuildRequest>,
    project: Option<Res<CurrentProject>>,
    pending: Option<Res<renzora::EnginePluginPendingProject>>,
    config: Option<Res<EnginePluginEcsConfig>>,
    trust: Res<renzora::EnginePluginTrust>,
    running: Option<Res<renzora::EnginePluginRunningGeneration>>,
    coordinator: Res<EnginePluginCoordinator>,
    mut state: ResMut<EnginePluginBuildState>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
) {
    let Some(request) = requests.read().last() else {
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
    coordinator.builder.invalidate();
    // Invalidate even when the replacement project cannot build. Otherwise a
    // late result from the previous project can restore its restart offer.
    coordinator.queued = None;
    let Some(project) = renzora::engine_plugin_project(project.as_deref(), pending.as_deref())
    else {
        *state = EnginePluginBuildState::Idle;
        return;
    };
    let Some(config) = config else {
        fail_state(
            &mut state,
            &mut diagnostics,
            revision,
            "Tier 2 build inputs are unavailable for this editor installation",
        );
        return;
    };
    let mut preparation = config.preparation.clone();
    preparation.plugins_root = project.join("plugins");
    coordinator.queued = Some(PendingPreparation {
        revision,
        preparation,
        trusted: trust.project.as_deref() == Some(project),
        running: running.and_then(|running| running.0.clone()),
        force_restart: pending.is_some(),
        not_before: Instant::now()
            + if request.reason == renzora::EnginePluginBuildReason::SourceChanged {
                Duration::from_millis(300)
            } else {
                Duration::ZERO
            },
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
                    Ok(Prepared::Unchanged) => *state = EnginePluginBuildState::Idle,
                    Ok(Prepared::AwaitingTrust) => {
                        *state = EnginePluginBuildState::AwaitingTrust {
                            revision: prepared.revision,
                        }
                    }
                    Ok(Prepared::Build(job)) => {
                        match coordinator.builder.submit(prepared.revision, *job) {
                            Ok(()) => {}
                            Err(error) => fail_state(
                                &mut state,
                                &mut diagnostics,
                                prepared.revision,
                                &error.to_string(),
                            ),
                        }
                    }
                    Err(message) => {
                        fail_state(&mut state, &mut diagnostics, prepared.revision, &message)
                    }
                }
            }
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {}
            Err(mpsc::TryRecvError::Empty) => coordinator.preparing = Some(receiver),
        }
    }
    if coordinator.preparing.is_none()
        && coordinator
            .queued
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.not_before)
    {
        if let Some(pending) = coordinator.queued.take() {
            let (sender, receiver) = mpsc::channel();
            let revision = pending.revision;
            let spawn = std::thread::Builder::new()
                .name(format!("engine_plugins.prepare.{revision}"))
                .spawn(move || {
                    let result = prepare_pending(&pending);
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

fn prepare_pending(pending: &PendingPreparation) -> Result<Prepared, String> {
    // Discovery reads declarations but does not invoke Cargo or project code.
    // Projects without native extensions should not see a trust warning.
    let declarations = crate::discover_engine_plugins(&pending.preparation.plugins_root)
        .map_err(|error| error.to_string())?;
    if declarations.is_empty()
        && pending
            .running
            .as_ref()
            .is_none_or(|stamp| stamp.plugins.is_empty())
    {
        return Ok(Prepared::Unchanged);
    }
    if !pending.trusted {
        return Ok(Prepared::AwaitingTrust);
    }
    let job = prepare_engine_build(&pending.preparation).map_err(|error| error.to_string())?;
    if !pending.force_restart
        && pending
            .running
            .as_ref()
            .is_some_and(|running| same_build_inputs(running, &job.stamp))
    {
        return Ok(Prepared::Unchanged);
    }
    Ok(Prepared::Build(Box::new(job)))
}

fn same_build_inputs(
    running: &renzora::EnginePluginGenerationStamp,
    prepared: &renzora::EnginePluginGenerationStamp,
) -> bool {
    // Preparation has the kit lockfile; a published executable has the extended
    // lockfile. The kit/vendor digest and plugin hashes pin all resolution inputs.
    let mut expected = prepared.clone();
    expected.lockfile_hash.clone_from(&running.lockfile_hash);
    running == &expected
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
    fn empty_project_does_not_require_trust_or_open_the_build_kit() {
        let temp = tempfile::tempdir().expect("project");
        let mut preparation = test_config().preparation;
        preparation.plugins_root = temp.path().join("plugins");
        let pending = PendingPreparation {
            revision: 1,
            preparation,
            not_before: Instant::now(),
            trusted: false,
            running: None,
            force_restart: false,
        };
        assert!(matches!(
            prepare_pending(&pending).expect("empty project"),
            Prepared::Unchanged
        ));
    }

    #[test]
    fn published_lock_extension_does_not_rebuild_unchanged_sources() {
        let running = renzora::EnginePluginGenerationStamp {
            lockfile_hash: "resolved lock".into(),
            ..Default::default()
        };
        let mut prepared = running.clone();
        prepared.lockfile_hash = "kit lock".into();
        assert!(same_build_inputs(&running, &prepared));
        prepared
            .plugins
            .insert("com.example.changed".into(), "new source".into());
        assert!(!same_build_inputs(&running, &prepared));
    }

    fn test_config() -> EnginePluginEcsConfig {
        EnginePluginEcsConfig {
            preparation: EngineBuildPreparation {
                plugins_root: PathBuf::new(),
                build_kit_root: PathBuf::from("not-opened-kit"),
                expected_build_kit_hash: "not-opened-kit".into(),
                cache_root: PathBuf::from("not-opened-cache"),
                cargo: PathBuf::from("not-started-cargo"),
                rustc: PathBuf::from("not-started-rustc"),
                requirement: crate::BuildKitRequirement {
                    engine_build: "test".into(),
                    target: "test-target".into(),
                    profile: "dist".into(),
                    toolchain_stamp: "test-toolchain".into(),
                },
                features: Default::default(),
                runtime: crate::EngineBinaryTarget {
                    package: "runtime".into(),
                    binary: "runtime".into(),
                    output_name: "runtime".into(),
                },
                editor: crate::EngineBinaryTarget {
                    package: "editor".into(),
                    binary: "editor".into(),
                    output_name: "editor".into(),
                },
            },
        }
    }

    #[test]
    fn source_events_invalidate_immediately_but_debounce_preparation() {
        let project = PathBuf::from("project");
        let mut app = App::new();
        app.init_resource::<EnginePluginCoordinator>()
            .init_resource::<EnginePluginBuildState>()
            .init_resource::<EnginePluginDiagnostics>()
            .init_resource::<renzora::EnginePluginTrust>()
            .add_message::<EnginePluginBuildRequest>()
            .add_systems(Update, queue_build_requests)
            .insert_resource(test_config())
            .insert_resource(CurrentProject {
                path: project.clone(),
                config: renzora::ProjectConfig::default(),
            });
        app.world_mut().write_message(EnginePluginBuildRequest {
            plugin_id: None,
            reason: renzora::EnginePluginBuildReason::SourceChanged,
        });
        app.update();
        assert!(matches!(
            app.world().resource::<EnginePluginBuildState>(),
            EnginePluginBuildState::Queued { .. }
        ));
        {
            let coordinator = app
                .world()
                .resource::<EnginePluginCoordinator>()
                .0
                .lock()
                .expect("coordinator");
            assert!(
                !coordinator
                    .queued
                    .as_ref()
                    .expect("discovery queued")
                    .trusted
            );
            assert!(coordinator.preparing.is_none());
        }
        app.world_mut()
            .resource_mut::<renzora::EnginePluginTrust>()
            .project = Some(project);
        let mut previous = 0;
        for _ in 0..3 {
            app.world_mut().write_message(EnginePluginBuildRequest {
                plugin_id: None,
                reason: renzora::EnginePluginBuildReason::SourceChanged,
            });
            let before = Instant::now();
            app.update();
            let coordinator = app
                .world()
                .resource::<EnginePluginCoordinator>()
                .0
                .lock()
                .expect("coordinator");
            let queued = coordinator.queued.as_ref().expect("debounced preparation");
            assert!(queued.revision > previous);
            assert_eq!(queued.revision, coordinator.latest_revision);
            assert!(queued.not_before >= before + Duration::from_millis(300));
            assert!(coordinator.preparing.is_none());
            previous = queued.revision;
        }
    }

    #[test]
    fn trust_decision_cannot_follow_a_project_switch() {
        let mut app = App::new();
        app.init_resource::<renzora::EnginePluginTrust>()
            .add_message::<renzora::EnginePluginTrustRequest>()
            .add_message::<EnginePluginBuildRequest>()
            .add_systems(Update, apply_trust_request);
        app.insert_resource(CurrentProject {
            path: PathBuf::from("project-b"),
            config: renzora::ProjectConfig::default(),
        });
        app.world_mut()
            .write_message(renzora::EnginePluginTrustRequest {
                project: PathBuf::from("project-a"),
                trusted: true,
            });
        app.update();
        assert!(app
            .world()
            .resource::<renzora::EnginePluginTrust>()
            .project
            .is_none());

        app.world_mut()
            .write_message(renzora::EnginePluginTrustRequest {
                project: PathBuf::from("project-b"),
                trusted: true,
            });
        app.update();
        assert_eq!(
            app.world().resource::<renzora::EnginePluginTrust>().project,
            Some(PathBuf::from("project-b"))
        );

        app.world_mut()
            .write_message(renzora::EnginePluginTrustRequest {
                project: PathBuf::from("project-b"),
                trusted: false,
            });
        app.update();
        assert!(app
            .world()
            .resource::<renzora::EnginePluginTrust>()
            .project
            .is_none());
    }

    #[test]
    fn delayed_worker_failure_keeps_its_original_revision() {
        let (sender, receiver) = mpsc::channel();
        let mut revisions = BTreeMap::from([(1, 100), (2, 200)]);
        send_events(
            &sender,
            vec![
                EngineBuildEvent::Failed {
                    revision: 1,
                    message: "old failure".into(),
                },
                EngineBuildEvent::Building {
                    revision: 2,
                    step: "new build",
                },
            ],
            &mut revisions,
        );
        let events: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            events[0],
            EngineBuildEvent::Failed { revision: 100, .. }
        ));
        assert!(matches!(
            events[1],
            EngineBuildEvent::Building { revision: 200, .. }
        ));
        assert_eq!(revisions, BTreeMap::from([(2, 200)]));
        let mut state = EnginePluginBuildState::Idle;
        let mut diagnostics = EnginePluginDiagnostics::default();
        apply_events(events, 200, &mut state, &mut diagnostics);
        assert!(matches!(
            state,
            EnginePluginBuildState::Building { revision: 200, .. }
        ));
        assert!(diagnostics.entries.is_empty());
    }

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
    fn closing_project_invalidates_preparation_and_stays_idle_after_late_result() {
        let mut app = App::new();
        app.add_plugins(EnginePluginEcsPlugin);
        app.insert_resource(CurrentProject {
            path: PathBuf::from("project-a"),
            config: renzora::ProjectConfig::default(),
        });
        app.update();
        let (sender, receiver) = mpsc::channel();
        let old_revision = {
            let coordinator = app.world().resource::<EnginePluginCoordinator>();
            let mut coordinator = coordinator.0.lock().expect("coordinator");
            coordinator.preparing = Some(receiver);
            coordinator.latest_revision
        };

        app.world_mut().remove_resource::<CurrentProject>();
        app.update();
        assert!(matches!(
            app.world().resource::<EnginePluginBuildState>(),
            EnginePluginBuildState::Idle
        ));
        sender
            .send(PreparationResult {
                revision: old_revision,
                result: Err("obsolete preparation failure".into()),
            })
            .expect("preparation receiver");
        app.update();
        assert!(matches!(
            app.world().resource::<EnginePluginBuildState>(),
            EnginePluginBuildState::Idle
        ));
        let coordinator = app.world().resource::<EnginePluginCoordinator>();
        let coordinator = coordinator.0.lock().expect("coordinator");
        assert!(coordinator.latest_revision > old_revision);
        assert!(coordinator.queued.is_none());
        assert!(coordinator.preparing.is_none());
        assert!(!app
            .world()
            .resource::<EnginePluginDiagnostics>()
            .entries
            .iter()
            .any(|entry| entry.message.contains("obsolete")));
    }

    #[test]
    fn switching_to_project_without_config_invalidates_previous_preparation() {
        let mut app = App::new();
        app.add_plugins(EnginePluginEcsPlugin);
        app.insert_resource(CurrentProject {
            path: PathBuf::from("project-a"),
            config: renzora::ProjectConfig::default(),
        });
        app.update();
        let (sender, receiver) = mpsc::channel();
        let old_revision = {
            let coordinator = app.world().resource::<EnginePluginCoordinator>();
            let mut coordinator = coordinator.0.lock().expect("coordinator");
            coordinator.preparing = Some(receiver);
            coordinator.latest_revision
        };
        sender
            .send(PreparationResult {
                revision: old_revision,
                result: Err("obsolete preparation failure".into()),
            })
            .expect("preparation receiver");
        app.world_mut().resource_mut::<CurrentProject>().path = PathBuf::from("project-b");
        app.update();

        match app.world().resource::<EnginePluginBuildState>() {
            EnginePluginBuildState::Failed { revision, message } => {
                assert!(*revision > old_revision);
                assert!(message.contains("unavailable"));
            }
            state => panic!("expected missing-input failure, got {state:?}"),
        }
        assert!(!app
            .world()
            .resource::<EnginePluginDiagnostics>()
            .entries
            .iter()
            .any(|entry| entry.message.contains("obsolete")));
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
