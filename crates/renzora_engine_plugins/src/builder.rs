//! Background, superseding whole-engine build coordinator.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use renzora::EnginePluginGenerationStamp;
use renzora_compiler_cache::cargo_target::{PartitionKey, PartitionRegistry};
use renzora_compiler_cache::process::{AttemptRecord, CargoSupervisor, CargoSupervisorConfig};
use renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA;
use renzora_identity::CanonicalId;

use crate::{
    select_candidate_generation, stage_generation, GenerationOutputs, PublishedEngineGeneration,
};

/// One executable Cargo must produce for a generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineBinaryTarget {
    /// Cargo package name.
    pub package: String,
    /// Cargo binary target name.
    pub binary: String,
    /// Filename inside Cargo's target profile directory.
    pub output_name: String,
}

/// Complete immutable input to one whole-engine attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineBuildJob {
    /// Materialized overlay root.
    pub workspace: PathBuf,
    /// Phase 5 cache root, containing targets and published generations.
    pub cache_root: PathBuf,
    /// Cargo executable selected by the approved toolchain.
    pub cargo: PathBuf,
    /// Exact compilation target.
    pub target: String,
    /// Approved non-development Cargo profile.
    pub profile: String,
    /// Canonically ordered engine features.
    pub features: BTreeSet<String>,
    /// Runtime output, when at least one runtime half is declared.
    pub runtime: Option<EngineBinaryTarget>,
    /// Editor output, when at least one editor half is declared.
    pub editor: Option<EngineBinaryTarget>,
    /// Identity published with the completed generation.
    pub stamp: EnginePluginGenerationStamp,
}

/// Observable state transition emitted by [`EngineBuildService::poll`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineBuildEvent {
    /// A revision entered the queue.
    Queued { revision: u64 },
    /// A supervised process began.
    Building { revision: u64, step: &'static str },
    /// A newer revision made this result ineligible for publication.
    Superseded { revision: u64 },
    /// A current attempt failed without changing the candidate generation.
    Failed {
        /// Failed revision.
        revision: u64,
        /// Bounded human-readable process diagnostic.
        message: String,
    },
    /// A complete immutable generation is ready for an explicit restart.
    Published {
        /// Successful revision.
        revision: u64,
        /// Validated published generation.
        generation: Box<PublishedEngineGeneration>,
    },
}

/// Build-service setup or process failure.
#[derive(Debug, thiserror::Error)]
pub enum EngineBuildServiceError {
    /// A job is incomplete or disagrees with its generation stamp.
    #[error("invalid engine build job: {0}")]
    InvalidJob(String),
    /// A process could not be started.
    #[error("could not start engine build process: {0}")]
    Spawn(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Sync,
    Compile,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::Sync => "generating plugin wiring",
            Self::Compile => "compiling editor and runtime",
        }
    }
}

struct QueuedJob {
    revision: u64,
    job: EngineBuildJob,
}

struct RunningJob {
    queued: QueuedJob,
    attempt_id: u64,
    step: Step,
    /// Held across both child processes so another editor cannot mutate this
    /// overlay while Cargo is reading it.
    _overlay_lock: File,
}

struct PublishingJob {
    revision: u64,
    result: mpsc::Receiver<Result<(PublishedEngineGeneration, bool), String>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Single-lane background builder. Submitting never blocks on Cargo; callers
/// periodically poll from an ECS drain system or another event loop.
pub struct EngineBuildService {
    supervisor: Arc<CargoSupervisor>,
    next_revision: u64,
    latest_revision: Arc<AtomicU64>,
    queued: Option<QueuedJob>,
    running: Option<RunningJob>,
    publishing: Option<PublishingJob>,
}

impl Default for EngineBuildService {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineBuildService {
    /// Create an isolated one-build-at-a-time supervisor.
    pub fn new() -> Self {
        let partitions = Arc::new(PartitionRegistry::new());
        Self {
            supervisor: Arc::new(CargoSupervisor::new(
                partitions,
                CargoSupervisorConfig {
                    max_children: 1,
                    ..Default::default()
                },
            )),
            next_revision: 1,
            latest_revision: Arc::new(AtomicU64::new(0)),
            queued: None,
            running: None,
            publishing: None,
        }
    }

    /// Queue a snapshot. A running older process is cancelled and reaped before
    /// the newest snapshot starts; intermediate queued snapshots are discarded.
    pub fn submit(
        &mut self,
        job: EngineBuildJob,
    ) -> Result<Vec<EngineBuildEvent>, EngineBuildServiceError> {
        validate_job(&job)?;
        let revision = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        self.latest_revision.store(revision, Ordering::Release);
        let mut events = Vec::new();
        if let Some(replaced) = self.queued.replace(QueuedJob { revision, job }) {
            events.push(EngineBuildEvent::Superseded {
                revision: replaced.revision,
            });
        }
        if let Some(running) = &self.running {
            self.supervisor.cancel(running.attempt_id);
        }
        events.push(EngineBuildEvent::Queued { revision });
        Ok(events)
    }

    /// Make every current result stale before a newer source snapshot has
    /// finished preparation. This closes the window where an older Cargo build
    /// could publish while discovery and hashing for a new edit are running.
    pub fn invalidate(&mut self) -> Vec<EngineBuildEvent> {
        let invalidating_revision = self.next_revision;
        self.next_revision = self.next_revision.saturating_add(1);
        self.latest_revision
            .store(invalidating_revision, Ordering::Release);
        let mut events = Vec::new();
        if let Some(queued) = self.queued.take() {
            events.push(EngineBuildEvent::Superseded {
                revision: queued.revision,
            });
        }
        if let Some(running) = &self.running {
            self.supervisor.cancel(running.attempt_id);
        }
        events
    }

    /// Drain completed work and start the newest eligible work without waiting.
    pub fn poll(&mut self) -> Vec<EngineBuildEvent> {
        let mut events = Vec::new();
        if let Some(mut publishing) = self.publishing.take() {
            match publishing.result.try_recv() {
                Ok(Ok((generation, true)))
                    if publishing.revision == self.latest_revision.load(Ordering::Acquire) =>
                {
                    join_finished_worker(&mut publishing);
                    events.push(EngineBuildEvent::Published {
                        revision: publishing.revision,
                        generation: Box::new(generation),
                    });
                }
                Ok(Ok(_)) => {
                    join_finished_worker(&mut publishing);
                    events.push(EngineBuildEvent::Superseded {
                        revision: publishing.revision,
                    });
                }
                Ok(Err(message)) => {
                    join_finished_worker(&mut publishing);
                    if publishing.revision == self.latest_revision.load(Ordering::Acquire) {
                        events.push(EngineBuildEvent::Failed {
                            revision: publishing.revision,
                            message,
                        });
                    } else {
                        events.push(EngineBuildEvent::Superseded {
                            revision: publishing.revision,
                        });
                    }
                }
                Err(mpsc::TryRecvError::Empty) => self.publishing = Some(publishing),
                Err(mpsc::TryRecvError::Disconnected) => {
                    join_finished_worker(&mut publishing);
                    events.push(EngineBuildEvent::Failed {
                        revision: publishing.revision,
                        message: "generation staging worker stopped unexpectedly".to_string(),
                    });
                }
            }
        }
        if let Some(running) = self.running.take() {
            match self.supervisor.try_reap_result(running.attempt_id) {
                Ok(Some(record)) => self.finish_step(running, record, &mut events),
                Ok(None) => self.running = Some(running),
                Err(error) => {
                    if let Some(mut handle) = self.supervisor.take_handle(running.attempt_id) {
                        let _ = handle.signal_kill();
                    }
                    events.push(EngineBuildEvent::Failed {
                        revision: running.queued.revision,
                        message: format!("could not observe engine build process: {error}"),
                    });
                }
            }
        }
        if self.running.is_none() && self.publishing.is_none() {
            self.start_queued(&mut events);
        }
        events
    }

    /// True while a revision is queued or a child process is owned.
    pub fn is_busy(&self) -> bool {
        self.queued.is_some() || self.running.is_some() || self.publishing.is_some()
    }

    fn finish_step(
        &mut self,
        running: RunningJob,
        record: AttemptRecord,
        events: &mut Vec<EngineBuildEvent>,
    ) {
        let revision = running.queued.revision;
        if revision != self.latest_revision.load(Ordering::Acquire) {
            events.push(EngineBuildEvent::Superseded { revision });
            return;
        }
        if record.exit_status != Some(0) {
            events.push(EngineBuildEvent::Failed {
                revision,
                message: process_message(running.step, &record),
            });
            return;
        }
        if running.step == Step::Sync {
            match self.spawn(running.queued, Step::Compile, running._overlay_lock) {
                Ok(next) => {
                    events.push(EngineBuildEvent::Building {
                        revision,
                        step: Step::Compile.label(),
                    });
                    self.running = Some(next);
                }
                Err(error) => events.push(EngineBuildEvent::Failed {
                    revision,
                    message: error.to_string(),
                }),
            }
            return;
        }

        let job = running.queued.job;
        let outputs = generation_outputs(&job);
        let latest = self.latest_revision.clone();
        let (sender, receiver) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name(format!("engine_plugins.publish.{revision}"))
            .spawn(move || {
                let result = stage_generation(&job.cache_root, job.stamp, &outputs)
                    .map_err(|error| error.to_string())
                    .and_then(|generation| {
                        if latest.load(Ordering::Acquire) != revision {
                            return Ok((generation, false));
                        }
                        select_candidate_generation(&job.cache_root, &generation)
                            .map(|selected| (generation, selected))
                            .map_err(|error| error.to_string())
                    });
                let _ = sender.send(result);
            });
        match spawn {
            Ok(worker) => {
                events.push(EngineBuildEvent::Building {
                    revision,
                    step: "publishing immutable generation",
                });
                self.publishing = Some(PublishingJob {
                    revision,
                    result: receiver,
                    worker: Some(worker),
                });
            }
            Err(error) => events.push(EngineBuildEvent::Failed {
                revision,
                message: format!("could not start generation staging worker: {error}"),
            }),
        }
    }

    fn start_queued(&mut self, events: &mut Vec<EngineBuildEvent>) {
        let Some(queued) = self.queued.take() else {
            return;
        };
        let lock_path = queued.job.workspace.join(".renzora-build.lock");
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(lock) => lock,
            Err(error) => {
                events.push(EngineBuildEvent::Failed {
                    revision: queued.revision,
                    message: format!("could not open overlay build lock: {error}"),
                });
                return;
            }
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                self.queued = Some(queued);
                return;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                events.push(EngineBuildEvent::Failed {
                    revision: queued.revision,
                    message: format!("could not lock overlay build: {error}"),
                });
                return;
            }
        }
        let revision = queued.revision;
        match self.spawn(queued, Step::Sync, lock) {
            Ok(running) => {
                events.push(EngineBuildEvent::Building {
                    revision,
                    step: Step::Sync.label(),
                });
                self.running = Some(running);
            }
            Err(error) => events.push(EngineBuildEvent::Failed {
                revision,
                message: error.to_string(),
            }),
        }
    }

    fn spawn(
        &self,
        queued: QueuedJob,
        step: Step,
        overlay_lock: File,
    ) -> Result<RunningJob, EngineBuildServiceError> {
        let command = command_for(&queued.job, step);
        let partition = partition_for(&queued.job);
        let identity =
            CanonicalId::parse("engine://tier2/generation").expect("static canonical identity");
        let attempt_id = self.supervisor.spawn(identity, partition, command)?;
        Ok(RunningJob {
            queued,
            attempt_id,
            step,
            _overlay_lock: overlay_lock,
        })
    }
}

impl Drop for EngineBuildService {
    fn drop(&mut self) {
        // A detached staging worker may finish its immutable copy, but it must
        // never select a restart candidate after its owning service is gone.
        self.latest_revision.store(u64::MAX, Ordering::Release);
        let mut handles = self.supervisor.drain_handles();
        for handle in &mut handles {
            let _ = handle.signal_stop();
        }
        let graceful_deadline = Instant::now() + Duration::from_secs(1);
        reap_until(&mut handles, graceful_deadline);
        for handle in &mut handles {
            let _ = handle.signal_kill();
        }
        reap_until(&mut handles, Instant::now() + Duration::from_secs(5));
    }
}

fn join_finished_worker(publishing: &mut PublishingJob) {
    if let Some(worker) = publishing.worker.take() {
        let _ = worker.join();
    }
}

fn reap_until(
    handles: &mut Vec<renzora_compiler_cache::process::SupervisorHandle>,
    deadline: Instant,
) {
    while !handles.is_empty() && Instant::now() < deadline {
        handles.retain_mut(|handle| {
            handle
                .child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        });
        if !handles.is_empty() {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn validate_job(job: &EngineBuildJob) -> Result<(), EngineBuildServiceError> {
    if !job.workspace.join("Cargo.toml").is_file() {
        return Err(EngineBuildServiceError::InvalidJob(format!(
            "overlay has no Cargo.toml at {}",
            job.workspace.display()
        )));
    }
    if job.runtime.is_none() && job.editor.is_none() {
        return Err(EngineBuildServiceError::InvalidJob(
            "no editor or runtime target was requested".to_string(),
        ));
    }
    for target in [&job.runtime, &job.editor].into_iter().flatten() {
        if !valid_cargo_name(&target.package) || !valid_cargo_name(&target.binary) {
            return Err(EngineBuildServiceError::InvalidJob(
                "Cargo package and binary names contain unsafe characters".to_string(),
            ));
        }
        if target.output_name.is_empty()
            || target.output_name.contains(['/', '\\'])
            || matches!(target.output_name.as_str(), "." | "..")
        {
            return Err(EngineBuildServiceError::InvalidJob(
                "binary output name must be one portable filename".to_string(),
            ));
        }
    }
    if job.profile == "dev" || job.profile.is_empty() {
        return Err(EngineBuildServiceError::InvalidJob(
            "a non-development profile is required".to_string(),
        ));
    }
    for (field, actual, stamped) in [
        ("target", &job.target, &job.stamp.target),
        ("profile", &job.profile, &job.stamp.profile),
    ] {
        if actual != stamped {
            return Err(EngineBuildServiceError::InvalidJob(format!(
                "{field} disagrees with the generation stamp"
            )));
        }
    }
    let canonical_features: Vec<String> = job.features.iter().cloned().collect();
    if job.stamp.features != canonical_features
        || job
            .features
            .iter()
            .any(|feature| feature.is_empty() || feature.contains(','))
    {
        return Err(EngineBuildServiceError::InvalidJob(
            "features disagree with the generation stamp".to_string(),
        ));
    }
    Ok(())
}

fn valid_cargo_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn command_for(job: &EngineBuildJob, step: Step) -> Command {
    let mut command = Command::new(&job.cargo);
    command.current_dir(&job.workspace);
    match step {
        Step::Sync => {
            command.args([
                "run",
                "--offline",
                "--locked",
                "--manifest-path",
                "xtask/Cargo.toml",
                "--profile",
                "release",
                "--",
                "sync",
            ]);
        }
        Step::Compile => {
            command.args([
                "build",
                "--offline",
                "--locked",
                "--profile",
                &job.profile,
                "--target",
                &job.target,
                "--target-dir",
            ]);
            command.arg(target_dir(job));
            for target in [&job.runtime, &job.editor].into_iter().flatten() {
                command.args(["--package", &target.package, "--bin", &target.binary]);
            }
            if !job.features.is_empty() {
                command.arg("--features");
                command.arg(job.features.iter().cloned().collect::<Vec<_>>().join(","));
            }
        }
    }
    command
}

fn target_dir(job: &EngineBuildJob) -> PathBuf {
    job.cache_root.join("cargo-target")
}

fn generation_outputs(job: &EngineBuildJob) -> GenerationOutputs {
    let profile_dir = target_dir(job).join(&job.target).join(&job.profile);
    GenerationOutputs {
        editor: job
            .editor
            .as_ref()
            .map(|target| profile_dir.join(&target.output_name)),
        runtime: job
            .runtime
            .as_ref()
            .map(|target| profile_dir.join(&target.output_name)),
    }
}

fn partition_for(job: &EngineBuildJob) -> PartitionKey {
    PartitionKey::from_inputs(
        &job.target,
        &job.stamp.toolchain_hash,
        &job.features,
        &job.profile,
        0,
        COMPILER_SERVICE_SCHEMA,
    )
}

fn process_message(step: Step, record: &AttemptRecord) -> String {
    const MAX_LINES: usize = 40;
    let lines: Vec<&str> = record
        .stderr
        .iter()
        .chain(&record.stdout)
        .rev()
        .take(MAX_LINES)
        .map(String::as_str)
        .collect();
    if lines.is_empty() {
        return format!(
            "{} failed with status {:?}",
            step.label(),
            record.exit_status
        );
    }
    let mut lines = lines;
    lines.reverse();
    format!("{} failed:\n{}", step.label(), lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use renzora::ENGINE_PLUGIN_GENERATION_SCHEMA;
    use tempfile::TempDir;

    use super::*;

    fn stamp() -> EnginePluginGenerationStamp {
        EnginePluginGenerationStamp {
            schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
            engine_build: "engine-1".into(),
            build_kit_hash: "kit".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "dist".into(),
            toolchain_hash: "toolchain".into(),
            lockfile_hash: "lock".into(),
            integration_hash: "integration".into(),
            features: Vec::new(),
            plugins: BTreeMap::new(),
        }
    }

    fn job(temp: &TempDir) -> EngineBuildJob {
        let workspace = temp.path().join("overlay");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("Cargo.toml"), "[workspace]\n").expect("manifest");
        EngineBuildJob {
            workspace,
            cache_root: temp.path().join("cache"),
            cargo: PathBuf::from("cargo"),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "dist".into(),
            features: BTreeSet::new(),
            runtime: Some(EngineBinaryTarget {
                package: "renzora_app".into(),
                binary: "renzora".into(),
                output_name: "renzora".into(),
            }),
            editor: None,
            stamp: stamp(),
        }
    }

    #[test]
    fn rejects_development_profile_and_stamp_disagreement() {
        let temp = TempDir::new().expect("temp");
        let mut invalid = job(&temp);
        invalid.profile = "dev".into();
        assert!(matches!(
            validate_job(&invalid),
            Err(EngineBuildServiceError::InvalidJob(_))
        ));

        let mut mismatch = job(&temp);
        mismatch.target = "aarch64-apple-darwin".into();
        assert!(matches!(
            validate_job(&mismatch),
            Err(EngineBuildServiceError::InvalidJob(_))
        ));

        let mut escaping_output = job(&temp);
        escaping_output
            .runtime
            .as_mut()
            .expect("runtime")
            .output_name = "../renzora".into();
        assert!(matches!(
            validate_job(&escaping_output),
            Err(EngineBuildServiceError::InvalidJob(_))
        ));
    }

    #[test]
    fn compile_command_is_locked_offline_and_deterministic() {
        let temp = TempDir::new().expect("temp");
        let mut job = job(&temp);
        job.features.extend(["zeta".into(), "alpha".into()]);

        let command = command_for(&job, Step::Compile);
        let arguments: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["--profile", "dist"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["--features", "alpha,zeta"]));
        assert!(arguments.contains(&"--locked".to_string()));
        assert!(arguments.contains(&"--offline".to_string()));
    }

    #[test]
    fn output_paths_are_partitioned_by_target_and_profile() {
        let temp = TempDir::new().expect("temp");
        let job = job(&temp);

        let outputs = generation_outputs(&job);

        assert_eq!(
            outputs.runtime,
            Some(
                temp.path()
                    .join("cache/cargo-target/x86_64-unknown-linux-gnu/dist/renzora")
            )
        );
        assert_eq!(outputs.editor, None);
    }

    #[cfg(unix)]
    #[test]
    fn three_rapid_revisions_publish_only_the_newest() {
        use std::thread;

        let temp = TempDir::new().expect("temp");
        let output = temp
            .path()
            .join("cache/cargo-target/x86_64-unknown-linux-gnu/dist/renzora");
        fs::create_dir_all(output.parent().expect("parent")).expect("target directory");
        fs::write(&output, "newest").expect("compiler output");

        let mut service = EngineBuildService::new();
        let mut first = job(&temp);
        // The coordinator owns command arguments and publication behavior; a
        // no-op executable isolates that state machine from Cargo in this test.
        first.cargo = PathBuf::from("/bin/true");
        let first_events = service.submit(first.clone()).expect("first submit");
        assert_eq!(first_events, vec![EngineBuildEvent::Queued { revision: 1 }]);
        assert!(matches!(
            service.poll().as_slice(),
            [EngineBuildEvent::Building { revision: 1, .. }]
        ));
        let second_events = service.submit(first.clone()).expect("second submit");
        assert_eq!(
            second_events,
            vec![EngineBuildEvent::Queued { revision: 2 }]
        );
        let third_events = service.submit(first).expect("third submit");
        assert_eq!(
            third_events,
            vec![
                EngineBuildEvent::Superseded { revision: 2 },
                EngineBuildEvent::Queued { revision: 3 }
            ]
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut observed = Vec::new();
        while Instant::now() < deadline {
            observed.extend(service.poll());
            if observed
                .iter()
                .any(|event| matches!(event, EngineBuildEvent::Published { revision: 3, .. }))
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        assert!(
            observed.contains(&EngineBuildEvent::Superseded { revision: 1 }),
            "observed {observed:#?}"
        );
        assert!(!observed.iter().any(|event| matches!(
            event,
            EngineBuildEvent::Published {
                revision: 1 | 2,
                ..
            }
        )));
        assert!(observed
            .iter()
            .any(|event| matches!(event, EngineBuildEvent::Published { revision: 3, .. })));
    }

    #[cfg(unix)]
    #[test]
    fn failed_build_preserves_existing_candidate() {
        use std::thread;

        let temp = TempDir::new().expect("temp");
        let known_good = temp.path().join("known-good");
        fs::write(&known_good, "working generation").expect("known good executable");
        let existing = crate::publish_generation(
            &temp.path().join("cache"),
            stamp(),
            &GenerationOutputs {
                editor: None,
                runtime: Some(known_good),
            },
        )
        .expect("publish known good");

        let mut failing = job(&temp);
        failing.cargo = PathBuf::from("/bin/false");
        let mut service = EngineBuildService::new();
        service.submit(failing).expect("submit");
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure = loop {
            if let Some(event) = service
                .poll()
                .into_iter()
                .find(|event| matches!(event, EngineBuildEvent::Failed { .. }))
            {
                break event;
            }
            assert!(Instant::now() < deadline, "failure was not reported");
            thread::sleep(Duration::from_millis(10));
        };

        assert!(matches!(
            failure,
            EngineBuildEvent::Failed { revision: 1, .. }
        ));
        let still_selected =
            crate::load_candidate_generation(&temp.path().join("cache")).expect("load candidate");
        assert_eq!(
            still_selected.manifest.generation,
            existing.manifest.generation
        );
        assert_eq!(
            still_selected.manifest.content_hash,
            existing.manifest.content_hash
        );
    }
}
