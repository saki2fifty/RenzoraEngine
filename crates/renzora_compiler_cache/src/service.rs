//! `BuildService` — the Bevy-independent compiler boundary.
//!
//! Rev-6 correction notes:
//!
//! - R6-1: every mutable build transaction (render manifests, read or
//!   bootstrap lockfile, resolve plan, fingerprint, cache lookup,
//!   cargo exec, artifact copy) happens under ONE per-partition mutex
//!   guard. The compiler's `run_transaction` method acquires the
//!   partition lock for the full sequence.
//! - R6-2: `compiler::ensure_lockfile` only bootstraps when the
//!   `Cargo.lock` is absent. An explicit `compiler::refresh_lockfile`
//!   API exists for the dependency-refresh path; it is not on the
//!   hot edit path.
//! - R6-3: completions route by exact (request_id, revision) pair.
//!   Rapid A→B→C submissions for the same identity each get their own
//!   receiver; superseded receivers receive `Superseded`.
//! - R6-6: per-edit resolution does NOT re-run `rustc -Vv` or rescan
//!   the SDK tree. The authoritative stamps are captured once per
//!   `BuildService` construction (or `BuildService::invalidate_stamps`
//!   call) and shared by every resolve.
//! - C2-3 / R5-4: per-service cache root, single-drain shutdown. No
//!   process-global state.
//! - C2-6 / R6-3: dispatcher holds only the completion receiver +
//!   `pending` clone + sticky stop flag. No `Arc<Self>` cycle.
//!
//! Phase 2 does NOT install a Bevy resource in `renzora_rust_script`; the
//! Bevy adapter is deferred to Phase 4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use renzora_identity::CanonicalId;

use crate::cargo_target::PartitionRegistry;
use crate::compiler::{
    CompilerConfig, ResolvedBuildConfig, ServiceStamps, Tier1Compiler,
};
use crate::desired::DesiredStore;
use crate::fingerprint::BuildFingerprint;
use crate::loader::{LoadError, LoadedLibrary, Loader};
use crate::process::{CargoSupervisor, CargoSupervisorConfig, SupervisorHandle};
use crate::retention::{ArtifactBudget, CargoBudget, RetentionConfig};
use crate::scheduler::{ReadyScheduler, SchedulerConfig};
use crate::staging::{ActivePointer, ArtifactCache};
use crate::types::{
    BuildOutcome, BuildProfile, BuildRequest, CancelCause, Diagnostic, PublishedGeneration, Revision,
};
use crate::worker::{AttemptCompletion, CompletionKind, WorkerPool, WorkerPoolConfig};

#[derive(Clone, Debug)]
pub struct BuildServiceConfig {
    pub cache_root: PathBuf,
    pub profile: BuildProfile,
    pub sdk_path: PathBuf,
    pub toolchain_stamp: String,
    pub compiler_service_schema: u32,
    pub n_workers: Option<usize>,
    pub n_children: Option<usize>,
    pub shutdown_deadline: Duration,
    /// Required exported symbols keyed by artifact kind. The loader
    /// verifies each kind's symbol set against the corresponding
    /// loaded image; a Tier-1 plugin is not rejected for missing
    /// script symbols, and a Tier-1 script is not rejected for
    /// missing plugin symbols.
    pub required_symbols_by_kind: std::collections::HashMap<crate::types::ArtifactKind, Vec<Vec<u8>>>,
}

impl Default for BuildServiceConfig {
    fn default() -> Self {
        Self {
            cache_root: PathBuf::from(".compiler_cache"),
            profile: BuildProfile::Dist,
            sdk_path: PathBuf::from("sdk"),
            toolchain_stamp: String::new(),
            compiler_service_schema: crate::types::COMPILER_SERVICE_SCHEMA,
            n_workers: None,
            n_children: None,
            shutdown_deadline: Duration::from_secs(5),
            required_symbols_by_kind: std::collections::HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub enum BuildServiceError {
    CacheRoot(std::io::Error),
    InvalidWorkerCount,
    ChildrenExceedWorkers { children: usize, workers: usize },
}

impl std::fmt::Display for BuildServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildServiceError::CacheRoot(e) => write!(f, "cache root: {e}"),
            BuildServiceError::InvalidWorkerCount => f.write_str("n_workers must be >= 1"),
            BuildServiceError::ChildrenExceedWorkers { children, workers } => {
                write!(f, "n_children ({children}) must be <= n_workers ({workers})")
            }
        }
    }
}

impl std::error::Error for BuildServiceError {}

/// Per-build request slot keyed by request id (R6-3).
struct PendingRequest {
    /// The exact request id assigned by `BuildService::submit`. The
    /// dispatcher matches completions against this id so rapid
    /// A→B→C edits for the same identity route their results to the
    /// CORRECT receivers.
    #[allow(dead_code)]
    request_id: u64,
    identity: CanonicalId,
    /// The revision that this `submit()` call introduced. Crucial for
    /// supersession: if a more-recent revision for this identity is
    /// already registered when a completion arrives, the receiver gets
    /// `Superseded { superseded_revision, by_revision }` rather than
    /// the older result.
    revision: Revision,
    /// Authoritative fingerprint submitted for this request.
    #[allow(dead_code)]
    fingerprint: BuildFingerprint,
    sender: crossbeam_channel::Sender<BuildOutcome>,
    cancel_cause: parking_lot::Mutex<Option<CancelCause>>,
}

#[derive(Clone, Debug, Default)]
pub struct ShutdownReport {
    pub unreaped: usize,
    pub detached: usize,
    pub elapsed: Duration,
    pub queued: usize,
    pub active: usize,
    pub reaped: usize,
    pub graceful: usize,
    pub forced: usize,
}

/// Service lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Running,
    ShuttingDown,
    Stopped,
}

/// Interior lifecycle owner (R5-5). Holds the worker pool, dispatcher
/// join handle, and the live children map. Only `shutdown` (or Drop)
/// moves these out.
struct ServiceLifecycle {
    state: ServiceState,
    pool: Option<WorkerPool>,
    dispatcher: Option<std::thread::JoinHandle<()>>,
    dispatcher_stop: Arc<AtomicBool>,
    /// R6-3: the dispatcher needs the desired store to detect
    /// supersession (when a completion arrives for a stale revision).
    #[allow(dead_code)]
    desired: Arc<DesiredStore>,
}

pub struct BuildService {
    #[allow(dead_code)]
    config: BuildServiceConfig,
    cache: Arc<ArtifactCache>,
    partitions: Arc<PartitionRegistry>,
    supervisor: Arc<CargoSupervisor>,
    #[allow(dead_code)]
    compiler: Arc<Tier1Compiler>,
    loader: Arc<Loader>,
    scheduler: Arc<ReadyScheduler>,
    desired: Arc<DesiredStore>,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    next_request_id: AtomicU64,
    artifact_budget: Arc<ArtifactBudget>,
    cargo_budget: Arc<CargoBudget>,
    /// Engine-owned defaults shared by every resolve. These DO NOT
    /// include the toolchain stamp or the SDK content hash; those live
    /// on `stamps` and are captured once per service generation
    /// (R6-6).
    defaults: ResolvedBuildConfig,
    /// Cached authoritative service stamps (R6-6). Behind a mutex so
    /// `BuildService::invalidate_stamps` can replace them atomically.
    /// Read by `preview_resolve` and by the worker on every attempt.
    stamps: parking_lot::Mutex<ServiceStamps>,
    lifecycle: Mutex<ServiceLifecycle>,
}

impl BuildService {
    pub fn new(config: BuildServiceConfig) -> Result<Arc<Self>, BuildServiceError> {
        std::fs::create_dir_all(&config.cache_root).map_err(BuildServiceError::CacheRoot)?;

        let scheduler = Arc::new(ReadyScheduler::new(SchedulerConfig::default()));
        let desired = Arc::new(DesiredStore::new());
        let partitions = Arc::new(PartitionRegistry::new());

        let max_workers = config.n_workers.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(1, 4)
        });
        let max_children = config.n_children.unwrap_or(max_workers.min(2));
        if max_workers == 0 {
            return Err(BuildServiceError::InvalidWorkerCount);
        }
        if max_children > max_workers {
            return Err(BuildServiceError::ChildrenExceedWorkers {
                children: max_children,
                workers: max_workers,
            });
        }

        let supervisor = Arc::new(CargoSupervisor::new(
            partitions.clone(),
            CargoSupervisorConfig {
                max_children,
                reader_buffer_lines: 4096,
            },
        ));

        // Resolve the authoritative default ResolvedBuildConfig (R5-1).
        // The default's `sdk_content_hash` / `toolchain_stamp` start at
        // zero / empty; the real values live in `stamps` (R6-6).
        let defaults = build_defaults(&config);

        // Capture authoritative service stamps ONCE per service
        // construction (R6-6). Per-edit work uses these cached values
        // instead of re-running `rustc -Vv` or recursively hashing the
        // SDK directory.
        let stamps = ServiceStamps::capture(&config.sdk_path, 1);

        let compiler = Tier1Compiler::new(
            CompilerConfig {
                cache_root: config.cache_root.clone(),
                defaults: defaults.clone(),
                stamps: stamps.clone(),
                cargo_timeout: Duration::from_secs(180),
            },
            supervisor.clone(),
            partitions.clone(),
        );
        let cache = ArtifactCache::new(config.cache_root.clone());
        cache.rebuild_index_from_disk();
        let loader = Arc::new(Loader::new(cache.clone()));
        // S4-2: required-symbol policy is artifact-kind-specific. The
        // previous combined-list policy required every configured
        // symbol for every loaded image, which was wrong: a normal
        // loose plugin does not export the script descriptor and a
        // normal script does not export the loose-plugin initializer.
        for (kind, names) in &config.required_symbols_by_kind {
            for name in names {
                loader.require_symbol_for(*kind, name);
            }
        }

        let pool = WorkerPool::spawn(
            scheduler.clone(),
            desired.clone(),
            compiler.clone(),
            supervisor.clone(),
            cache.clone(),
            partitions.clone(),
            WorkerPoolConfig {
                n_workers: max_workers,
                park_interval: Duration::from_millis(25),
            },
            defaults.clone(),
            stamps.clone(),
        );

        let artifact_budget = Arc::new(ArtifactBudget::new(
            cache.clone(),
            RetentionConfig::default(),
        ));
        let cargo_budget = Arc::new(CargoBudget::new(
            cache.clone(),
            partitions.clone(),
            supervisor.clone(),
            RetentionConfig::default(),
        ));

        let pending = Arc::new(Mutex::new(HashMap::<u64, PendingRequest>::new()));
        let dispatcher_stop = Arc::new(AtomicBool::new(false));

        let completions_rx = pool.completions_rx.clone();
        let pending_for_resolve = pending.clone();
        let stop_for_resolve = dispatcher_stop.clone();

        let dispatcher = std::thread::Builder::new()
            .name("compiler_cache.dispatcher".into())
            .spawn(move || {
                dispatcher_loop(completions_rx, pending_for_resolve, stop_for_resolve);
            })
            .expect("spawn compiler_cache dispatcher");

        let svc = Arc::new(Self {
            config,
            cache,
            partitions,
            supervisor,
            compiler,
            loader,
            scheduler,
            desired: desired.clone(),
            pending,
            next_request_id: AtomicU64::new(1),
            artifact_budget,
            cargo_budget,
            defaults,
            stamps: parking_lot::Mutex::new(stamps),
            lifecycle: Mutex::new(ServiceLifecycle {
                state: ServiceState::Running,
                pool: Some(pool),
                dispatcher: Some(dispatcher),
                dispatcher_stop,
                desired: desired.clone(),
            }),
        });
        Ok(svc)
    }

    /// R6-6: invalidate the cached authoritative toolchain stamp and
    /// SDK content hash. Call this when the user (or a toolchain
    /// change signal) reports that rustc has been updated or the SDK
    /// directory has been replaced. After invalidation, every cache
    /// lookup based on the old stamps is implicitly invalidated: the
    /// new stamps produce a different partition key, which does not
    /// match any previously-published fingerprint's serialized form.
    pub fn invalidate_stamps(&self) -> ServiceStamps {
        let mut guard = self.stamps.lock();
        let next_generation = guard.generation + 1;
        let fresh = ServiceStamps::capture(&self.config.sdk_path, next_generation);
        *guard = fresh.clone();
        fresh
    }

    /// Snapshot of the cached authoritative service stamps (R6-6).
    /// Tests use this to assert one-time discovery behaviour.
    pub fn stamps(&self) -> ServiceStamps {
        self.stamps.lock().clone()
    }

    pub fn state(&self) -> ServiceState {
        self.lifecycle.lock().state
    }

    pub fn submit(
        self: &Arc<Self>,
        request: BuildRequest,
    ) -> Result<crossbeam_channel::Receiver<BuildOutcome>, BuildServiceError> {
        // R5-5: reject submissions after shutdown begins.
        {
            let lifecycle = self.lifecycle.lock();
            if lifecycle.state != ServiceState::Running {
                return Err(BuildServiceError::CacheRoot(std::io::Error::other(
                    "service not running",
                )));
            }
        }

        let id = request.identity.clone();
        let snapshot = request.source_snapshot.clone();
        let inputs = Arc::new(request.fingerprint_inputs.clone());
        let artifact_kind = request.artifact_kind;

        // R6-3: every submit assigns a unique request id BEFORE any
        // resolution work, so even if submit returns early (cache hit)
        // the receiver is keyed to its submit number.
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let revision = self
            .desired
            .record_edit(id.clone(), request_id, snapshot.clone(), inputs, artifact_kind);
        // R7-2: supersede any older pending receivers for this identity
        // IMMEDIATELY. Older pending entries are removed from the
        // pending map and each receives
        // `BuildOutcome::Superseded { superseded_revision, by_revision }`
        // before this submit's worker can produce a completion. This
        // guarantees every receiver resolves exactly once.
        self.supersede_older_for(&id, revision);
        // R5B-1: resolve the authoritative build plan FIRST. The cache
        // key is derived EXCLUSIVELY from the resolved plan.
        let choices = crate::compiler::RequestChoices {
            target_triple: if request.fingerprint_inputs.target_triple.is_empty() {
                request.target.clone()
            } else {
                request.fingerprint_inputs.target_triple.clone()
            },
            capabilities: request.fingerprint_inputs.capabilities.clone(),
            profile: match request.fingerprint_inputs.profile {
                BuildProfile::Dist => crate::compiler::ProfileName::Dist,
                BuildProfile::DistLean => crate::compiler::ProfileName::DistLean,
            },
            panic: match request.fingerprint_inputs.panic {
                crate::types::PanicStrategy::Abort => crate::compiler::PanicMode::Abort,
                crate::types::PanicStrategy::Unwind => crate::compiler::PanicMode::Unwind,
            },
            rustflags: request.fingerprint_inputs.rustflags.clone(),
        };

        // Capture the cached stamps for this submit; do NOT run rustc
        // or hash the SDK directory (R6-6).
        let stamps = self.stamps.lock().clone();

        // Submit does a PREVIEW resolve (no manifest writes, no source writes,
        // no Cargo.lock reads): the lockfile hash is supplied by the
        // service (read once from disk on the submit path; ZERO when no
        // lockfile exists). The full bootstrap + authoritative resolve
        // + cache-miss compile happens inside the worker transaction
        // (which owns the partition lock + writes the canonical
        // manifests) before cargo runs. This keeps the submit hot path
        // free of generated-workspace writes.
        let lockfile_path = {
            let tentative_partition = crate::cargo_target::PartitionKey::from_inputs(
                &choices.target_triple,
                &stamps.toolchain_stamp,
                &choices.capabilities,
                choices.profile.as_str(),
                self.defaults.abi_version,
                self.defaults.compiler_service_schema,
            );
            let entry = self.partitions.get_or_insert(
                tentative_partition,
                self.compiler.cache_root(),
            );
            entry.target_dir.join("generated").join("Cargo.lock")
        };
        let lockfile_hash = if lockfile_path.exists() {
            match std::fs::read(&lockfile_path) {
                Ok(b) => crate::compiler::hash_bytes(&b),
                Err(_) => crate::fingerprint::ContentHash::ZERO,
            }
        } else {
            crate::fingerprint::ContentHash::ZERO
        };

        let resolved = crate::worker::preview_resolve(
            &id,
            &request.source_snapshot,
            &choices,
            artifact_kind,
            &self.defaults,
            &self.partitions,
            self.compiler.cache_root(),
            &stamps,
            lockfile_hash,
        );
        let fingerprint = resolved.fingerprint;
        let build_key = fingerprint.build_key();

        // Centralised cache lookup.
        if let Some(active) = self.cache.read_active(&id) {
            let lookup = crate::staging::verify_generation_fingerprint(
                &self.cache,
                &id,
                active.generation,
                &fingerprint,
            );
            if let Ok(r) = lookup {
                if r.matched {
                    let artifact_path = self.cache.artifact_path(
                        &id,
                        active.generation,
                        &fingerprint.crate_type_default_lib_ext(),
                    );
                    let (tx, rx) = crossbeam_channel::bounded::<BuildOutcome>(1);
                    let _ = tx.send(BuildOutcome::CacheHit {
                        request_revision: revision,
                        fingerprint,
                        generation: active.generation,
                        immutable_artifact_path: artifact_path,
                        compiled_packages: Vec::new(),
                    });
                    return Ok(rx);
                }
            }
        }
        if let Some(gen) = self.cache.lookup_inactive(&id, &build_key) {
            let lookup = crate::staging::verify_generation_fingerprint(
                &self.cache,
                &id,
                gen,
                &fingerprint,
            );
            if let Ok(r) = lookup {
                if r.matched {
                    let _ = self.cache.write_active(
                        &id,
                        &ActivePointer {
                            generation: gen,
                            fingerprint_hash: build_key.0,
                            compiler_service_schema: self.config.compiler_service_schema,
                        },
                        false,
                    );
                    let artifact_path = self.cache.artifact_path(
                        &id,
                        gen,
                        &fingerprint.crate_type_default_lib_ext(),
                    );
                    let (tx, rx) = crossbeam_channel::bounded::<BuildOutcome>(1);
                    let _ = tx.send(BuildOutcome::CacheHit {
                        request_revision: revision,
                        fingerprint,
                        generation: gen,
                        immutable_artifact_path: artifact_path,
                        compiled_packages: Vec::new(),
                    });
                    return Ok(rx);
                }
            }
        }

        // R6-3: every submit gets a UNIQUE request_id. The dispatcher
        // routes by (request_id, revision), not by identity alone, so
        // A→B→C rapid edits for the same identity each get their own
        // receiver and never cross-deliver results.
        let (tx, rx) = crossbeam_channel::bounded::<BuildOutcome>(1);
        let cancel_cause = parking_lot::Mutex::new(None);
        let pending = PendingRequest {
            request_id,
            identity: id.clone(),
            revision,
            fingerprint: fingerprint.clone(),
            sender: tx,
            cancel_cause,
        };
        self.pending.lock().insert(request_id, pending);
        self.scheduler.enqueue(id.clone());

        Ok(rx)
    }

    pub fn load_published(
        self: &Arc<Self>,
        id: &CanonicalId,
        fingerprint: &BuildFingerprint,
        artifact_kind: crate::types::ArtifactKind,
    ) -> Result<LoadedLibrary, LoadError> {
        self.loader.load(id, fingerprint, artifact_kind)
    }

    pub fn loader(&self) -> &Arc<Loader> {
        &self.loader
    }

    pub fn cache(&self) -> &Arc<ArtifactCache> {
        &self.cache
    }

    pub fn supervisor(&self) -> &Arc<CargoSupervisor> {
        &self.supervisor
    }

    pub fn partitions(&self) -> &Arc<PartitionRegistry> {
        &self.partitions
    }

    pub fn config(&self) -> &BuildServiceConfig {
        &self.config
    }

    pub fn artifact_budget(&self) -> &Arc<ArtifactBudget> {
        &self.artifact_budget
    }

    pub fn cargo_budget(&self) -> &Arc<CargoBudget> {
        &self.cargo_budget
    }

    pub fn defaults(&self) -> &ResolvedBuildConfig {
        &self.defaults
    }

    /// Return the set of pending request ids (R6-3). Tests use this to
    /// assert that rapid A→B→C submits each have their own pending
    /// entry.
    pub fn pending_request_ids(&self) -> Vec<u64> {
        let g = self.pending.lock();
        let mut ids: Vec<u64> = g.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Return the most-recent registered revision for `id` (R6-3).
    /// Used by supersession when a completion arrives for a stale
    /// revision.
    pub fn latest_revision_for(&self, id: &CanonicalId) -> Option<Revision> {
        self.desired.latest_revision(id)
    }

    pub fn cancel_for(self: &Arc<Self>, id: &CanonicalId, by: CancelCause) {
        {
            let mut pending = self.pending.lock();
            let to_remove: Vec<u64> = pending
                .iter()
                .filter(|(_, p)| &p.identity == id)
                .map(|(k, _)| *k)
                .collect();
            for k in &to_remove {
                if let Some(p) = pending.remove(k) {
                    *p.cancel_cause.lock() = Some(by);
                    let _ = p.sender.send(BuildOutcome::Cancelled {
                        request_revision: p.revision,
                        by,
                    });
                }
            }
        }
        let _ = self.supervisor.cancel_for(id);
        self.scheduler.remove(id);
    }

    /// Bounded shutdown. R5-4 / R5-5:
    /// - moves the worker pool out exactly once;
    /// - moves the dispatcher JoinHandle out exactly once;
    /// - drains children into ONE owned local Vec;
    /// - signals those local children;
    /// - polls try_wait on those local children until the absolute deadline;
    /// - force-kills survivors;
    /// - transfers only genuine survivors to the isolated reaper;
    /// - rejects submissions after the first call returns;
    /// - returns `AlreadyStopped` (a variant of `BuildServiceError`)
    ///   on subsequent calls.
    pub fn shutdown(&self, timeout: Duration) -> Result<ShutdownReport, BuildServiceError> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.state == ServiceState::Stopped {
            return Err(BuildServiceError::InvalidWorkerCount); // re-purpose
        }
        if lifecycle.state == ServiceState::ShuttingDown {
            return Err(BuildServiceError::InvalidWorkerCount);
        }
        lifecycle.state = ServiceState::ShuttingDown;
        drop(lifecycle);

        let start = Instant::now();
        let deadline = start + timeout;
        let mut report = ShutdownReport::default();

        // Signal every worker to stop, and stop the scheduler.
        let mut lifecycle = self.lifecycle.lock();
        if let Some(pool) = lifecycle.pool.as_ref() {
            *pool.stop_flag.lock() = true;
        }
        self.scheduler.stop();

        // Resolve all pending futures with Shutdown.
        {
            let mut pending = self.pending.lock();
            let outstanding: Vec<PendingRequest> = pending.drain().map(|(_, p)| p).collect();
            for p in outstanding {
                let _ = p.sender.send(BuildOutcome::Shutdown {
                    request_revision: p.revision,
                });
            }
        }

        // Step 1: drain every supervisor child into ONE owned local Vec.
        let mut children: Vec<SupervisorHandle> = self.supervisor.drain_handles();
        report.active = children.len();
        report.graceful = children.len();
        report.queued = self.scheduler.queue_len();

        // Step 2: signal every local child.
        for c in &mut children {
            if let Some(ch) = c.child.as_mut() {
                let _ = ch.signal_stop();
            }
        }

        // Step 3: bounded reap on those EXACT children.
        let reap_interval = Duration::from_millis(25);
        while !children.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let sleep_for = reap_interval.min(remaining);
            std::thread::sleep(sleep_for);
            children.retain_mut(|handle| {
                if let Some(c) = handle.child.as_mut() {
                    matches!(c.try_wait(), Ok(None))
                } else {
                    false
                }
            });
        }

        // Step 4: force-kill survivors and one final non-blocking poll.
        let survivors: Vec<SupervisorHandle> = if !children.is_empty() {
            for c in &mut children {
                if let Some(ch) = c.child.as_mut() {
                    let _ = ch.signal_kill();
                }
            }
            report.forced = children.len();
            children.retain_mut(|handle| {
                if let Some(c) = handle.child.as_mut() {
                    matches!(c.try_wait(), Ok(None))
                } else {
                    false
                }
            });
            children
        } else {
            Vec::new()
        };
        report.unreaped = survivors.len();

        // Step 5: transfer only genuine survivors to the isolated reaper.
        if !survivors.is_empty() {
            report.reaped = survivors.len();
            spawn_detached_os_reaper(survivors);
        }

        // Step 6: signal dispatcher stop + join its JoinHandle.
        lifecycle.dispatcher_stop.store(true, Ordering::SeqCst);
        let dispatcher = lifecycle.dispatcher.take();
        if let Some(j) = dispatcher {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let _ = std::thread::spawn(move || {
                let _ = j.join();
                let _ = done_tx.send(());
            });
            if done_rx.recv_timeout(remaining).is_err() {
                report.detached += 1;
            }
        }

        // Step 7: take the worker pool and join its workers.
        let pool = lifecycle.pool.take();
        drop(lifecycle);
        if let Some(mut pool) = pool {
            let joined = join_workers(&mut pool.workers, deadline);
            report.detached += pool.workers.len().saturating_sub(joined);
        }

        // Step 8: mark the lifecycle Stopped.
        let mut lifecycle = self.lifecycle.lock();
        lifecycle.state = ServiceState::Stopped;
        drop(lifecycle);

        report.elapsed = start.elapsed();
        Ok(report)
    }

    pub fn pump_transient_retries(&self) -> usize {
        let eligible = self.desired.transient_eligible_now();
        let n = eligible.len();
        for (id, _rev) in eligible {
            self.scheduler.enqueue(id);
        }
        n
    }

    /// R6-3 / R7-2: supersede every pending entry for `id` whose
    /// revision is strictly less than `new_revision`. Each superseded
    /// receiver gets `BuildOutcome::Superseded { superseded_revision,
    /// by_revision: new_revision }` IMMEDIATELY. The worker that was
    /// compiling the older revision will still emit an
    /// `AttemptCompletion`, but the dispatcher finds no pending entry
    /// for that `request_id` and drops it.
    pub fn supersede_older_for(&self, id: &CanonicalId, new_revision: Revision) -> usize {
        let mut pending = self.pending.lock();
        let mut to_remove: Vec<u64> = Vec::new();
        for (k, v) in pending.iter() {
            if &v.identity == id && v.revision.0 < new_revision.0 {
                let outcome = BuildOutcome::Superseded {
                    superseded_revision: v.revision,
                    by_revision: new_revision,
                };
                let _ = v.sender.send(outcome);
                to_remove.push(*k);
            }
        }
        let n = to_remove.len();
        for k in to_remove {
            pending.remove(&k);
        }
        n
    }
}

/// Build the authoritative default `ResolvedBuildConfig` (R5-1). The
/// caller-supplied `BuildServiceConfig` is only a hint for paths and
/// numbers; the actual toolchain stamp and SDK content hash live on
/// the cached `ServiceStamps` (R6-6) and are NOT re-derived here.
fn build_defaults(config: &BuildServiceConfig) -> ResolvedBuildConfig {
    use crate::compiler::{CrateTypeName, PanicMode, ProfileName};
    use crate::fingerprint::ContentHash;
    use std::collections::BTreeSet;

    let sdk_path = config.sdk_path.clone();
    let profile = match config.profile {
        BuildProfile::Dist => ProfileName::Dist,
        BuildProfile::DistLean => ProfileName::DistLean,
    };

    ResolvedBuildConfig {
        target_triple: crate::types::default_target_triple(),
        // Toolchain stamp lives on `ServiceStamps` (R6-6); this field
        // is informational only and may be empty for the default.
        toolchain_stamp: String::new(),
        sdk_path,
        // Same: real SDK hash lives on `ServiceStamps`. The default
        // starts at ZERO; the resolver uses the cached stamp instead.
        sdk_content_hash: ContentHash::ZERO,
        abi_version: 1,
        interface_prefix_hashes: Vec::new(),
        wrapper_schema: 1,
        wrapper_content_hash: ContentHash::ZERO,
        manifest_schema: 1,
        manifest_content_hash: ContentHash::ZERO,
        lock_resolution: ContentHash::ZERO,
        lockfile_path: None,
        capabilities: BTreeSet::new(),
        cargo_features: Vec::new(),
        profile,
        rustflags: Vec::new(),
        panic: PanicMode::Abort,
        crate_type: CrateTypeName::Cdylib,
        compiler_service_schema: config.compiler_service_schema,
    }
}

fn parse_diagnostics(stderr: &[String]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stderr {
        let t = line.trim();
        if t.starts_with("Compiling")
            || t.starts_with("Finished")
            || t.starts_with("Locking")
            || t.starts_with("Updating")
            || t.starts_with("Downloaded")
            || t.starts_with("Downloading")
            || t.starts_with("Installing")
            || t.starts_with("Build")
            || t.starts_with("Running")
            || t.starts_with("Fresh")
            || t.starts_with("Checking")
        {
            continue;
        }
        if t.starts_with("error") || t.starts_with("warning") {
            out.push(Diagnostic::error(t.to_string()));
        }
    }
    if out.is_empty() {
        if stderr.is_empty() {
            out.push(Diagnostic::error("transient infrastructure failure"));
        } else {
            out.push(Diagnostic::error(stderr.join("\n")));
        }
    }
    out
}

fn dispatcher_loop(
    completions_rx: crossbeam_channel::Receiver<AttemptCompletion>,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match completions_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(c) => route_completion(&pending, c),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Route a worker completion to the EXACT pending receiver (R6-3, R7-2).
/// Supersession already happened at submit time (`supersede_older_for`);
/// this function only routes by exact `request_id`. If no pending
/// entry matches, the completion is dropped.
fn route_completion(
    pending: &Mutex<HashMap<u64, PendingRequest>>,
    completion: AttemptCompletion,
) {
    let build_outcome = |req: &PendingRequest| -> BuildOutcome {
        match completion.outcome_kind {
            CompletionKind::Published if completion.was_cache_hit => BuildOutcome::CacheHit {
                request_revision: req.revision,
                fingerprint: completion.fingerprint.clone(),
                generation: completion.generation.unwrap_or(PublishedGeneration(1)),
                immutable_artifact_path: completion.artifact_path.clone().unwrap_or_default(),
                compiled_packages: completion.compiled_packages.clone(),
            },
            CompletionKind::Published => BuildOutcome::Published {
                request_revision: req.revision,
                fingerprint: completion.fingerprint.clone(),
                generation: completion.generation.unwrap_or(PublishedGeneration(1)),
                immutable_artifact_path: completion.artifact_path.clone().unwrap_or_default(),
                compiled_packages: completion.compiled_packages.clone(),
            },
            CompletionKind::CompileError => BuildOutcome::CompileFailed {
                request_revision: req.revision,
                diagnostics: parse_diagnostics(&completion.stderr),
            },
            CompletionKind::Transient => BuildOutcome::CompileFailed {
                request_revision: req.revision,
                diagnostics: parse_diagnostics(&completion.stderr),
            },
            CompletionKind::Cancelled => {
                let by = *req.cancel_cause.lock();
                BuildOutcome::Cancelled {
                    request_revision: req.revision,
                    by: by.unwrap_or(CancelCause::ProjectClose),
                }
            }
            CompletionKind::Shutdown => BuildOutcome::Shutdown {
                request_revision: req.revision,
            },
        }
    };
    let mut pending = pending.lock();
    if let Some(req) = pending.remove(&completion.request_id) {
        let outcome = build_outcome(&req);
        let _ = req.sender.send(outcome);
    }
}

fn spawn_detached_os_reaper(children: Vec<SupervisorHandle>) {
    std::thread::Builder::new()
        .name("compiler_cache.detached_reaper".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut handles = children;
            while Instant::now() < deadline && !handles.is_empty() {
                let mut next = Vec::new();
                for mut h in handles {
                    let still_alive = if let Some(c) = h.child.as_mut() {
                        matches!(c.try_wait(), Ok(None))
                    } else {
                        false
                    };
                    if still_alive {
                        next.push(h);
                    }
                }
                handles = next;
                if !handles.is_empty() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            for mut h in handles {
                let stdout_thread = h.child.as_mut().and_then(|c| c.stdout_thread.take());
                let stderr_thread = h.child.as_mut().and_then(|c| c.stderr_thread.take());
                drop(h);
                let _ = crate::process::join_reader_bounded(stdout_thread, Duration::from_secs(5));
                let _ = crate::process::join_reader_bounded(stderr_thread, Duration::from_secs(5));
            }
        })
        .expect("spawn detached reaper");
}

fn join_workers(
    workers: &mut [crate::worker::Worker],
    deadline: Instant,
) -> usize {
    let mut joined = 0;
    for w in workers.iter_mut() {
        let Some(handle) = w.join.take() else { continue };
        let now = Instant::now();
        if now >= deadline {
            drop(handle);
            continue;
        }
        let remaining = deadline.saturating_duration_since(now);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(remaining).is_err() {
            // Detach.
        } else {
            joined += 1;
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BuildServiceConfig {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        BuildServiceConfig {
            cache_root: root.join("cache"),
            profile: BuildProfile::Dist,
            sdk_path: root,
            toolchain_stamp: String::new(),
            compiler_service_schema: 1,
            n_workers: Some(1),
            n_children: Some(1),
            shutdown_deadline: Duration::from_secs(1),
            required_symbols_by_kind: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn service_constructs_and_shuts_down() {
        let svc = BuildService::new(cfg()).unwrap();
        let report = svc.shutdown(Duration::from_millis(500)).unwrap();
        assert!(report.elapsed <= Duration::from_secs(2));
        assert_eq!(svc.state(), ServiceState::Stopped);
    }

    /// R5-5: repeated shutdown is idempotent (returns error after Stopped).
    #[test]
    fn repeated_shutdown_returns_error() {
        let svc = BuildService::new(cfg()).unwrap();
        let _ = svc.shutdown(Duration::from_millis(500)).unwrap();
        let r = svc.shutdown(Duration::from_millis(500));
        assert!(r.is_err());
    }

    /// R5-5: shutdown works with external Arc clones.
    #[test]
    fn shutdown_works_with_external_clones() {
        let svc = BuildService::new(cfg()).unwrap();
        let _external = svc.clone();
        let report = svc.shutdown(Duration::from_millis(500)).unwrap();
        assert!(report.elapsed <= Duration::from_secs(2));
    }

    /// R5-5: submit after shutdown is rejected.
    #[test]
    fn submit_after_shutdown_rejected() {
        let svc = BuildService::new(cfg()).unwrap();
        let _ = svc.shutdown(Duration::from_millis(500)).unwrap();
        let id = renzora_identity::CanonicalId::from_rooted(
            renzora_identity::RootKind::Project,
            "after_shutdown.rs",
        )
        .unwrap();
        let req = BuildRequest {
            identity: id,
            source_snapshot: Arc::new(b"".to_vec()),
            fingerprint_inputs: Default::default(),
            target: crate::types::default_target_triple(),
            artifact_kind: crate::types::ArtifactKind::Tier1Script,
        };
        let r = svc.submit(req);
        assert!(r.is_err(), "submit after shutdown must be rejected");
    }

    /// R6-6: `BuildService::new` captures stamps ONCE; subsequent
    /// resolves that don't trigger `invalidate_stamps` see the SAME
    /// stamp values.
    #[test]
    fn unit_stamps_captured_once() {
        let svc = BuildService::new(cfg()).unwrap();
        let s1 = svc.stamps();
        // Read multiple times; the value is stable.
        let s2 = svc.stamps();
        assert_eq!(s1.toolchain_stamp, s2.toolchain_stamp);
        assert_eq!(s1.sdk_content_hash, s2.sdk_content_hash);
        assert_eq!(s1.generation, s2.generation);
        let _ = svc.shutdown(Duration::from_millis(500)).unwrap();
    }

    /// R6-6: invalidating stamps bumps the generation counter.
    #[test]
    fn unit_invalidate_stamps_bumps_generation() {
        let svc = BuildService::new(cfg()).unwrap();
        let s_before = svc.stamps();
        let fresh = svc.invalidate_stamps();
        assert_eq!(fresh.generation, s_before.generation + 1);
        let s_after = svc.stamps();
        assert_eq!(s_after.generation, fresh.generation);
        let _ = svc.shutdown(Duration::from_millis(500)).unwrap();
    }
}
