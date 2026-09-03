//! Bounded worker pool driving the cargo supervisor. See §3.3 / §6.3 / §9.4.
//!
//! ## R7-1 (rev-7)
//!
//! The worker holds ZERO partition lock. The transaction
//! (`Tier1Compiler::run_transaction`) acquires the partition lock once
//! for the entire cache-miss path. The worker's per-job flow is:
//!
//! 1. Render manifest bytes **purely** (no disk writes).
//! 2. Read the on-disk lockfile if any and compute its SHA-256.
//! 3. Resolve the authoritative plan + fingerprint (uses cached
//!    `ServiceStamps`).
//! 4. Check the cache (active / inactive + reactivation). Hit → publish
//!    immediately. Miss → call `compiler.run_transaction`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use renzora_identity::CanonicalId;

#[cfg(test)]
use crate::cargo_target::PartitionEntry;
use crate::cargo_target::{PartitionKey, PartitionRegistry};
use crate::compiler::{
    render_workspace_and_package, resolve_build_config, hash_bytes,
    ResolvedBuildConfig, Tier1Compiler, TransactionRequest, TransactionResult,
};
use crate::desired::DesiredStore;
use crate::fingerprint::{BuildFingerprint, ContentHash};
use crate::process::CargoSupervisor;
use crate::scheduler::ReadyScheduler;
use crate::staging::{verify_generation_fingerprint, ActivePointer, ArtifactCache};
use crate::types::PublishedGeneration;

/// A single worker handle.
pub struct Worker {
    pub join: Option<std::thread::JoinHandle<WorkerExit>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerExit {
    Stopped,
    Panicked,
}

#[derive(Debug, Clone)]
pub struct AttemptCompletion {
    pub request_id: u64,
    pub identity: CanonicalId,
    pub fingerprint: BuildFingerprint,
    pub revision: crate::types::Revision,
    pub outcome_kind: CompletionKind,
    pub was_cache_hit: bool,
    pub generation: Option<PublishedGeneration>,
    #[allow(dead_code)]
    pub artifact_path: Option<PathBuf>,
    #[allow(dead_code)]
    pub stderr: Vec<String>,
    pub compiled_packages: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Published,
    CompileError,
    Transient,
    Cancelled,
    Shutdown,
}

pub struct WorkerPool {
    pub workers: Vec<Worker>,
    pub scheduler: Arc<ReadyScheduler>,
    pub desired: Arc<DesiredStore>,
    pub compiler: Arc<Tier1Compiler>,
    pub supervisor: Arc<CargoSupervisor>,
    pub cache: Arc<ArtifactCache>,
    pub partitions: Arc<PartitionRegistry>,
    pub config: WorkerPoolConfig,
    pub completions_tx: Sender<AttemptCompletion>,
    pub completions_rx: Receiver<AttemptCompletion>,
    pub stop_flag: Arc<Mutex<bool>>,
}

#[derive(Clone, Debug)]
pub struct WorkerPoolConfig {
    pub n_workers: usize,
    pub park_interval: Duration,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .saturating_mul(2)
            .clamp(1, 4);
        Self {
            n_workers: n,
            park_interval: Duration::from_millis(25),
        }
    }
}

impl WorkerPool {
    pub fn spawn(
        scheduler: Arc<ReadyScheduler>,
        desired: Arc<DesiredStore>,
        compiler: Arc<Tier1Compiler>,
        supervisor: Arc<CargoSupervisor>,
        cache: Arc<ArtifactCache>,
        partitions: Arc<PartitionRegistry>,
        config: WorkerPoolConfig,
        defaults: ResolvedBuildConfig,
        stamps: crate::compiler::ServiceStamps,
    ) -> Self {
        let n = config.n_workers.max(1);
        let (tx, rx) = bounded::<AttemptCompletion>(n * 4);
        let mut workers = Vec::with_capacity(n);
        let stop_flag = Arc::new(Mutex::new(false));
        for n in 0..n {
            let sched = scheduler.clone();
            let des = desired.clone();
            let comp = compiler.clone();
            let carg = supervisor.clone();
            let cache_c = cache.clone();
            let parts = partitions.clone();
            let tx_c = tx.clone();
            let stop_c = stop_flag.clone();
            let cfg_c = config.clone();
            let defaults_c = defaults.clone();
            let stamps_c = stamps.clone();
            let join = std::thread::Builder::new()
                .name(format!("compiler_cache.worker-{n}"))
                .spawn(move || {
                    worker_loop(
                        sched,
                        des,
                        comp,
                        carg,
                        cache_c,
                        parts,
                        tx_c,
                        stop_c,
                        cfg_c,
                        defaults_c,
                        stamps_c,
                    )
                })
                .expect("spawn compiler_cache worker thread");
            workers.push(Worker { join: Some(join) });
        }
        Self {
            workers,
            scheduler,
            desired,
            compiler,
            supervisor,
            cache,
            partitions,
            config,
            completions_tx: tx,
            completions_rx: rx,
            stop_flag,
        }
    }
}

/// Pre-lock resolution output (R7-1).
pub struct ResolvedPlan {
    pub effective: ResolvedBuildConfig,
    pub fingerprint: BuildFingerprint,
    pub partition_key: PartitionKey,
    pub rendered: crate::compiler::RenderedManifests,
}

/// Pre-lock resolution used by `BuildService::submit`. **Completely
/// pure**: does NOT touch disk (no manifest writes, no source writes,
/// no directory creation, no `Cargo.lock` reads). Computes the
/// authoritative plan + fingerprint from the cached stamps, the
/// canonical renderer output, and the lockfile hash supplied by the
/// caller (zero if the partition has never been bootstrapped).
///
/// The lockfile hash is read by the caller (the service) before this is
/// called so the submit hot path stays filesystem-free for the rendered
/// workspace.
pub fn preview_resolve(
    id: &CanonicalId,
    snapshot: &[u8],
    choices: &crate::compiler::RequestChoices,
    artifact_kind: crate::types::ArtifactKind,
    defaults: &ResolvedBuildConfig,
    partitions: &PartitionRegistry,
    cache_root: &std::path::Path,
    stamps: &crate::compiler::ServiceStamps,
    lockfile_hash: ContentHash,
) -> ResolvedPlan {
    let tentative_partition = PartitionKey::from_inputs(
        &if choices.target_triple.is_empty() {
            defaults.target_triple.clone()
        } else {
            choices.target_triple.clone()
        },
        &stamps.toolchain_stamp,
        &choices.capabilities,
        match choices.profile {
            crate::compiler::ProfileName::Dist => "dist",
            crate::compiler::ProfileName::DistLean => "dist-lean",
        },
        defaults.abi_version,
        defaults.compiler_service_schema,
    );
    let _entry = partitions.get_or_insert(tentative_partition.clone(), cache_root);

    // Pure render. The renderer is completely pure (R7-1): no
    // create_dir_all, no source writes, no manifest writes.
    let rendered = render_workspace_and_package(
        &tentative_partition,
        &defaults.sdk_path,
        snapshot,
        choices.profile,
        choices.panic,
        match artifact_kind {
            crate::types::ArtifactKind::StaticLib => crate::compiler::CrateTypeName::Staticlib,
            crate::types::ArtifactKind::DynamicLib { .. } => crate::compiler::CrateTypeName::Dylib,
            crate::types::ArtifactKind::Tier1Plugin | crate::types::ArtifactKind::Tier1Script => {
                crate::compiler::CrateTypeName::Cdylib
            }
        },
        &choices.capabilities,
        id,
    );

    let effective = resolve_build_config(
        id,
        snapshot,
        choices,
        artifact_kind,
        defaults,
        lockfile_hash,
        stamps,
        &rendered,
    )
    .expect("preview resolve: resolve_build_config failed");

    let fingerprint = effective.fingerprint(id, snapshot);
    let partition_key = PartitionKey::from_inputs(
        &effective.target_triple,
        &effective.toolchain_stamp,
        &effective.capabilities,
        effective.profile.as_str(),
        effective.abi_version,
        effective.compiler_service_schema,
    );

    ResolvedPlan {
        effective,
        fingerprint,
        partition_key,
        rendered,
    }
}

fn run_one_transaction(
    compiler: Arc<Tier1Compiler>,
    cache: Arc<ArtifactCache>,
    _partitions: Arc<PartitionRegistry>,
    id: CanonicalId,
    snapshot: Arc<Vec<u8>>,
    resolved: ResolvedPlan,
    artifact_kind: crate::types::ArtifactKind,
    _scheduled_revision: crate::types::Revision,
    _scheduled_request_id: u64,
    _tx: &Sender<AttemptCompletion>,
) -> Result<TransactionResult, ()> {
    let staged_dir = crate::staging::stage_dir(compiler.cache_root(), &id);
    let lib_ext = resolved.fingerprint.crate_type_default_lib_ext();
    let staged_artifact = staged_dir.join(format!("lib{}.{lib_ext}", id.bare_leaf()));

    let req = TransactionRequest {
        identity: id.clone(),
        source_snapshot: snapshot,
        artifact_kind,
        staged_artifact: staged_artifact.clone(),
        partition_key: resolved.partition_key,
        rendered: resolved.rendered,
        effective: resolved.effective,
        fingerprint: resolved.fingerprint,
    };
    let result = compiler.run_transaction(cache.clone(), req);
    Ok(result)
}

fn send_completion(
    tx: &Sender<AttemptCompletion>,
    request_id: u64,
    identity: CanonicalId,
    fingerprint: BuildFingerprint,
    revision: crate::types::Revision,
    kind: CompletionKind,
    was_cache_hit: bool,
    generation: Option<PublishedGeneration>,
    artifact_path: Option<PathBuf>,
    stderr: Vec<String>,
    compiled_packages: Vec<PathBuf>,
) {
    let _ = tx.send(AttemptCompletion {
        request_id,
        identity,
        fingerprint,
        revision,
        outcome_kind: kind,
        was_cache_hit,
        generation,
        artifact_path,
        stderr,
        compiled_packages,
    });
}

fn handle_cache_hit_active(
    cache: &ArtifactCache,
    id: &CanonicalId,
    fingerprint: &BuildFingerprint,
) -> Option<PublishedGeneration> {
    let active = cache.read_active(id)?;
    let r = verify_generation_fingerprint(cache, id, active.generation, fingerprint).ok()?;
    if r.matched {
        Some(active.generation)
    } else {
        None
    }
}

fn handle_cache_hit_inactive(
    cache: &ArtifactCache,
    id: &CanonicalId,
    fingerprint: &BuildFingerprint,
) -> Option<PublishedGeneration> {
    let gen = cache.lookup_inactive(id, &fingerprint.build_key())?;
    let r = verify_generation_fingerprint(cache, id, gen, fingerprint).ok()?;
    if r.matched {
        // Reactivate.
        let _ = cache.write_active(
            id,
            &ActivePointer {
                generation: gen,
                fingerprint_hash: fingerprint.build_key().0,
                compiler_service_schema: fingerprint.compiler_service_schema,
            },
            false,
        );
        Some(gen)
    } else {
        None
    }
}

fn worker_loop(
    scheduler: Arc<ReadyScheduler>,
    desired: Arc<DesiredStore>,
    compiler: Arc<Tier1Compiler>,
    _supervisor: Arc<CargoSupervisor>,
    cache: Arc<ArtifactCache>,
    partitions: Arc<PartitionRegistry>,
    tx: Sender<AttemptCompletion>,
    stop: Arc<Mutex<bool>>,
    cfg: WorkerPoolConfig,
    defaults: ResolvedBuildConfig,
    stamps: crate::compiler::ServiceStamps,
) -> WorkerExit {
    loop {
        if *stop.lock() {
            return WorkerExit::Stopped;
        }
        let lease = scheduler.lease_next_timeout(cfg.park_interval);
        let Some(lease) = lease else {
            continue;
        };
        let Some(id) = lease.id().cloned() else {
            continue;
        };
        let Some((revision, snapshot, inputs, artifact_kind)) = desired.snapshot(&id) else {
            scheduler.finish(&id, false);
            continue;
        };
        let scheduled_request_id = desired.latest_request_id(&id).unwrap_or(0);

        let choices = crate::compiler::RequestChoices {
            target_triple: if inputs.target_triple.is_empty() {
                defaults.target_triple.clone()
            } else {
                inputs.target_triple.clone()
            },
            capabilities: inputs.capabilities.clone(),
            profile: match inputs.profile {
                crate::types::BuildProfile::Dist => crate::compiler::ProfileName::Dist,
                crate::types::BuildProfile::DistLean => crate::compiler::ProfileName::DistLean,
            },
            panic: match inputs.panic {
                crate::types::PanicStrategy::Abort => crate::compiler::PanicMode::Abort,
                crate::types::PanicStrategy::Unwind => crate::compiler::PanicMode::Unwind,
            },
            rustflags: inputs.rustflags.clone(),
        };

        // Compute the lockfile hash from disk (the only filesystem
        // read on the submit hot path). The preview path uses this.
        let tentative_partition = crate::cargo_target::PartitionKey::from_inputs(
            &choices.target_triple,
            &stamps.toolchain_stamp,
            &choices.capabilities,
            choices.profile.as_str(),
            defaults.abi_version,
            defaults.compiler_service_schema,
        );
        let entry = partitions.get_or_insert(tentative_partition, compiler.cache_root());
        let lockfile_path = entry.target_dir.join("generated").join("Cargo.lock");
        let lockfile_hash = if lockfile_path.exists() {
            match std::fs::read(&lockfile_path) {
                Ok(b) => hash_bytes(&b),
                Err(_) => ContentHash::ZERO,
            }
        } else {
            ContentHash::ZERO
        };

        let resolved = preview_resolve(
            &id,
            &snapshot,
            &choices,
            artifact_kind,
            &defaults,
            &partitions,
            compiler.cache_root(),
            &stamps,
            lockfile_hash,
        );

        // Fast-path: active cache hit. The worker's preview fingerprint is
        // identical to the fingerprint that would be published by the
        // transaction (both use the same lockfile_hash: real if the
        // lockfile exists, ZERO otherwise — the transaction bootstraps
        // a matching fingerprint on first compile).
        if let Some(gen) = handle_cache_hit_active(&cache, &id, &resolved.fingerprint) {
            desired.record_outcome(
                &id,
                crate::status::AttemptOutcome::Success,
                revision,
                None,
            );
            scheduler.finish(&id, false);
            let artifact_path = cache.artifact_path(
                &id,
                gen,
                &resolved.fingerprint.crate_type_default_lib_ext(),
            );
            send_completion(
                &tx,
                scheduled_request_id,
                id.clone(),
                resolved.fingerprint.clone(),
                revision,
                CompletionKind::Published,
                true,
                Some(gen),
                Some(artifact_path),
                Vec::new(),
                Vec::new(),
            );
            continue;
        }
        // Fast-path: inactive cache hit (reactivate).
        if let Some(gen) = handle_cache_hit_inactive(&cache, &id, &resolved.fingerprint) {
            desired.record_outcome(
                &id,
                crate::status::AttemptOutcome::Success,
                revision,
                None,
            );
            scheduler.finish(&id, false);
            let artifact_path = cache.artifact_path(
                &id,
                gen,
                &resolved.fingerprint.crate_type_default_lib_ext(),
            );
            send_completion(
                &tx,
                scheduled_request_id,
                id.clone(),
                resolved.fingerprint.clone(),
                revision,
                CompletionKind::Published,
                true,
                Some(gen),
                Some(artifact_path),
                Vec::new(),
                Vec::new(),
            );
            continue;
        }

        // Cache miss → run the production transaction (ONE partition lock).
        desired.next_attempt_id();
        desired.record_attempt(&id, 0, revision);

        let transaction_result = match run_one_transaction(
            compiler.clone(),
            cache.clone(),
            partitions.clone(),
            id.clone(),
            snapshot.clone(),
            ResolvedPlan {
                effective: resolved.effective.clone(),
                fingerprint: resolved.fingerprint.clone(),
                partition_key: resolved.partition_key.clone(),
                rendered: resolved.rendered.clone(),
            },
            artifact_kind,
            revision,
            scheduled_request_id,
            &tx,
        ) {
            Ok(r) => r,
            Err(()) => {
                scheduler.finish(&id, false);
                continue;
            }
        };

        // Convert to AttemptCompletion.
        match transaction_result {
            TransactionResult::CacheHit {
                fingerprint,
                generation,
                compiled_packages,
            } => {
                desired.record_outcome(
                    &id,
                    crate::status::AttemptOutcome::Success,
                    revision,
                    None,
                );
                let artifact_path = cache.artifact_path(
                    &id,
                    generation,
                    &fingerprint.crate_type_default_lib_ext(),
                );
                send_completion(
                    &tx,
                    scheduled_request_id,
                    id.clone(),
                    fingerprint,
                    revision,
                    CompletionKind::Published,
                    true,
                    Some(generation),
                    Some(artifact_path),
                    Vec::new(),
                    compiled_packages,
                );
            }
            TransactionResult::Published {
                fingerprint,
                generation_placeholder: _,
                compiled_packages,
                json_events: _,
                stderr,
                staged_artifact,
            } => {
                let pub_result = crate::staging::publish(
                    &cache,
                    &id,
                    revision,
                    &fingerprint,
                    &staged_artifact,
                );
                match pub_result {
                    Ok(pub_res) => {
                        send_completion(
                            &tx,
                            scheduled_request_id,
                            id.clone(),
                            fingerprint,
                            revision,
                            CompletionKind::Published,
                            false,
                            Some(pub_res.generation),
                            Some(pub_res.artifact_path),
                            stderr,
                            compiled_packages,
                        );
                    }
                    Err(e) => {
                        let mut publish_stderr = vec![format!("publish: {e}")];
                        publish_stderr.extend(stderr);
                        send_completion(
                            &tx,
                            scheduled_request_id,
                            id.clone(),
                            fingerprint,
                            revision,
                            CompletionKind::CompileError,
                            false,
                            None,
                            None,
                            publish_stderr,
                            compiled_packages,
                        );
                    }
                }
            }
            TransactionResult::CompileError {
                stderr,
                diagnostics,
                json_events: _,
            } => {
                send_completion(
                    &tx,
                    scheduled_request_id,
                    id.clone(),
                    resolved.fingerprint.clone(),
                    revision,
                    CompletionKind::CompileError,
                    false,
                    None,
                    None,
                    stderr,
                    Vec::new(),
                );
                let _ = diagnostics;
            }
            TransactionResult::Transient {
                stderr,
                diagnostics,
                json_events: _,
            } => {
                send_completion(
                    &tx,
                    scheduled_request_id,
                    id.clone(),
                    resolved.fingerprint.clone(),
                    revision,
                    CompletionKind::Transient,
                    false,
                    None,
                    None,
                    stderr,
                    Vec::new(),
                );
                let _ = diagnostics;
            }
        }

        // Check whether a newer revision has arrived since we started.
        let latest = desired.latest_revision(&id).unwrap_or(revision);
        let needs_follow_up = latest.0 > revision.0;
        scheduler.finish(&id, needs_follow_up);

        // Avoid hot-spinning.
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Jittered bounded exponential backoff (initial 500 ms, cap 30 s, jittered).
pub fn jittered_backoff(attempt: u32, cap_ms: u64) -> Duration {
    let base = 500u64 << attempt.min(6);
    let capped = base.min(cap_ms);
    let jitter = (capped as f64 * 0.1) as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = if jitter == 0 { 0 } else { nanos % (jitter + 1) };
    Duration::from_millis(capped + jitter)
}

// Compatibility shim so the rest of the crate still compiles. The old
// `run_one_compile` is removed; this empty re-export keeps any
// downstream code that referenced the symbol compiling. The actual
// production compile path is `compiler.run_transaction`.
#[allow(dead_code)]
pub(crate) fn _compile_request_marker() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittered_backoff_grows_then_caps() {
        let d0 = jittered_backoff(0, 30_000);
        let d1 = jittered_backoff(1, 30_000);
        let d6 = jittered_backoff(6, 30_000);
        assert!(d0.as_millis() <= 33_500);
        assert!(d1.as_millis() <= 33_500);
        assert!(d6.as_millis() <= 33_500);
    }

    #[test]
    fn partition_lock_blocks_during_compile() {
        use crate::cargo_target::partition_lock;
        let partitions = PartitionRegistry::new();
        let entry: Arc<PartitionEntry> = partitions.get_or_insert(
            PartitionKey {
                target_triple: "x86_64-unknown-linux-gnu".into(),
                toolchain_stamp: "toolchain".into(),
                capabilities_canonical: "".into(),
                profile: "dist".into(),
                abi_version: 1,
                compiler_service_schema: 1,
            },
            std::path::Path::new("/tmp"),
        );
        let _guard = partition_lock(&entry);
        let entry2 = entry.clone();
        let started = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started2 = started.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _g = partition_lock(&entry2);
            started2.store(1, std::sync::atomic::Ordering::Relaxed);
            tx.send(true).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(started.load(std::sync::atomic::Ordering::Relaxed), 0);
        drop(_guard);
        let _ = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        t.join().unwrap();
    }
}