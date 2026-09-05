//! Real Tier-1 production compiler.
//!
//! See `phase2-cached-compiler-design.md` §5 / §6.3 / §10.2. The
//! authoritative design requires ONE immutable `ResolvedBuildConfig` to
//! drive both the fingerprint and the actual Cargo invocation — fingerprint
//! and command cannot diverge because they share the same struct.
//!
//! ## R7-1 (rev-7)
//!
//! - `render_workspace_and_package` is **completely pure**: no
//!   `create_dir_all`, no source writes, no manifest writes, no
//!   filesystem mutations of any kind. The preview / submission hot path
//!   performs NO generated-workspace writes.
//! - One stable wrapper package per partition. The package name is
//!   derived from the partition key, NOT from the script identity. When
//!   the worker writes the manifest, it overwrites this single stable
//!   wrapper's `src/lib.rs` with the selected source under the partition
//!   lock. The script's canonical identity still drives the cache key
//!   and the staged artifact path; it does NOT need a unique Cargo
//!   package.
//! - One partition-lock acquisition owns the complete cache-miss
//!   transaction: write stable wrapper manifest + selected source,
//!   bootstrap-or-reuse the lockfile, read & hash the existing lockfile,
//!   resolve the authoritative plan & fingerprint, perform the
//!   authoritative cache lookup/reactivation, run Cargo with `--locked`
//!   on a miss, stage & publish the artifact. The lock is released only
//!   after the artifact is staged.
//!
//! ## R5B-3 / R6-2 / R6-6 retained
//!
//! - `--locked` always passed once the lockfile exists on the partition.
//!   Drift rejects without regenerating.
//! - `ServiceStamps` captured once per `BuildService` generation.
//!   `compile` does NOT re-run `rustc -Vv` or re-hash the SDK.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use renzora_identity::CanonicalId;
use sha2::{Digest, Sha256};

use crate::cargo_target::{partition_lock, PartitionKey};
use crate::fingerprint::{BuildFingerprint, ContentHash};
use crate::process::CargoSupervisor;
use crate::staging::{verify_generation_fingerprint, ArtifactCache};
use crate::types::{ArtifactKind, Diagnostic, PublishedGeneration};

/// One set of bytes the canonical renderer produces. The output of a
/// render pass is two pieces: the literal workspace `Cargo.toml` bytes
/// and the literal per-package `Cargo.toml` bytes.
///
/// The renderer is **completely pure**: it returns these bytes from its
/// arguments without touching the filesystem. The caller (worker, inside
/// the partition lock) is responsible for writing them to disk.
#[derive(Clone, Debug)]
pub struct RenderedManifests {
    /// Stable package name derived from `stable_partition_package_name(pk)`.
    /// Every script that compiles inside this partition overwrites THIS
    /// package's `src/lib.rs`; the canonical script identity is preserved
    /// in the cache key and the staged-artifact path, but the Cargo
    /// package is shared.
    pub package_name: String,
    /// Identity-derived library crate name. This is the `[lib] name` in
    /// the per-package `Cargo.toml`, the prefix that `module_path!()`
    /// emits at the start of every plugin-supplied type, and the
    /// filename cargo produces for the produced artefact. Derived from
    /// the canonical identity's BLAKE3 so it is the SAME across every
    /// partition (target, profile, capabilities, toolchain, ABI,
    /// compiler-service schema) for a given plugin — and so the
    /// `module_path!()`-derived plugin-local type paths are partition-
    /// independent. A valid Rust identifier; no truncation.
    pub lib_name: String,
    /// Bytes of the workspace `Cargo.toml`. Includes the (single)
    /// `resolver = "2"` entry, the `[profile.<name>]` block, and the
    /// `[workspace.dependencies] renzora_plugin = { path = "..." }` line.
    /// `members` is exactly one entry: `package_name`.
    pub workspace_toml: Vec<u8>,
    /// Bytes of the per-package `Cargo.toml`. The `renzora_plugin`
    /// dependency has `workspace = true, default-features = false,
    /// features = [...]` all inside ONE dep entry — never as standalone
    /// `[dependencies]` table entries.
    pub package_toml: Vec<u8>,
    /// Workspace members. Always exactly `[package_name]` — the
    /// stable per-partition wrapper package. There is no per-script
    /// workspace membership growth.
    pub members: Vec<String>,
}

impl RenderedManifests {
    /// The exact bytes that go into `wrapper_content_hash`. Includes the
    /// workspace bytes, package bytes, source bytes, and a trailer
    /// capturing per-request choices (target + rustflags).
    pub fn wrapper_hash_bytes(
        &self,
        source: &[u8],
        target_triple: &str,
        rustflags: &[String],
    ) -> Vec<u8> {
        let trailer = format!(
            "\n# effective_build.target_triple = {}\n# effective_build.rustflags = {:?}\n",
            target_triple, rustflags
        );
        let mut buf = Vec::with_capacity(
            self.workspace_toml.len() + self.package_toml.len()
                + source.len() + trailer.len() + 4,
        );
        buf.extend_from_slice(&self.workspace_toml);
        buf.push(0);
        buf.extend_from_slice(&self.package_toml);
        buf.push(0);
        buf.extend_from_slice(source);
        buf.extend_from_slice(trailer.as_bytes());
        buf
    }
}

/// Cargo profile name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileName {
    Dist,
    DistLean,
}

impl ProfileName {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileName::Dist => "dist",
            ProfileName::DistLean => "dist-lean",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicMode {
    Abort,
    Unwind,
}

impl PanicMode {
    pub fn as_cargo_str(&self) -> &'static str {
        match self {
            PanicMode::Abort => "abort",
            PanicMode::Unwind => "unwind",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrateTypeName {
    Cdylib,
    Staticlib,
    Dylib,
}

impl CrateTypeName {
    pub fn as_cargo_str(&self) -> &'static str {
        match self {
            CrateTypeName::Cdylib => "cdylib",
            CrateTypeName::Staticlib => "staticlib",
            CrateTypeName::Dylib => "dylib",
        }
    }
}

/// Allow-listed capabilities the editor may request.
pub const ALLOWED_CAPABILITIES: &[&str] = &[
    "runtime",
    "static_plugins",
    "static_scripts",
    // F4-1: scripts target the small `script` feature of
    // `renzora_plugin` so `renzora_plugin::script::*` is in the SDK.
    "script",
];

/// The authoritative, fully-resolved build configuration.
#[derive(Clone, Debug)]
pub struct ResolvedBuildConfig {
    pub target_triple: String,
    /// Toolchain stamp from the cached `ServiceStamps` (R6-6).
    pub toolchain_stamp: String,
    pub sdk_path: PathBuf,
    /// SDK content hash from the cached `ServiceStamps` (R6-6).
    pub sdk_content_hash: ContentHash,
    pub abi_version: u32,
    pub interface_prefix_hashes: Vec<u32>,
    pub wrapper_schema: u32,
    /// SHA-256 of the actual generated wrapper content (workspace
    /// `Cargo.toml` + per-package `Cargo.toml` + `src/lib.rs` bytes).
    pub wrapper_content_hash: ContentHash,
    pub manifest_schema: u32,
    pub manifest_content_hash: ContentHash,
    /// SHA-256 of the canonical `Cargo.lock` at resolve time. Authoritative.
    pub lock_resolution: ContentHash,
    /// Path to the resolved `Cargo.lock`. `cargo build --locked` reads it.
    pub lockfile_path: Option<PathBuf>,
    pub capabilities: BTreeSet<String>,
    pub cargo_features: Vec<String>,
    pub profile: ProfileName,
    pub rustflags: Vec<String>,
    pub panic: PanicMode,
    pub crate_type: CrateTypeName,
    pub compiler_service_schema: u32,
}

impl ResolvedBuildConfig {
    pub fn profile_env_prefix(&self) -> &'static str {
        match self.profile {
            ProfileName::Dist => "CARGO_PROFILE_DIST",
            ProfileName::DistLean => "CARGO_PROFILE_DIST_LEAN",
        }
    }

    pub fn fingerprint(&self, identity: &CanonicalId, source: &[u8]) -> BuildFingerprint {
        let mut fp = BuildFingerprint::new(identity.clone(), source.to_vec());
        fp.target_triple = self.target_triple.clone();
        fp.toolchain_stamp = self.toolchain_stamp.clone();
        fp.sdk_content_hash = self.sdk_content_hash;
        fp.abi = crate::fingerprint::AbiStamp {
            version: self.abi_version,
            interface_prefix_hashes: self.interface_prefix_hashes.clone(),
        };
        fp.wrapper_schema = self.wrapper_schema;
        fp.wrapper_content_hash = self.wrapper_content_hash;
        fp.manifest_schema = self.manifest_schema;
        fp.manifest_content_hash = self.manifest_content_hash;
        fp.lock_resolution = self.lock_resolution;
        fp.capabilities = self.capabilities.clone();
        fp.profile = match self.profile {
            ProfileName::Dist => crate::fingerprint::ProfileTag::Dist,
            ProfileName::DistLean => crate::fingerprint::ProfileTag::DistLean,
        };
        fp.rustflags = self.rustflags.clone();
        fp.panic = match self.panic {
            PanicMode::Abort => crate::fingerprint::PanicTag::Abort,
            PanicMode::Unwind => crate::fingerprint::PanicTag::Unwind,
        };
        fp.crate_type = match self.crate_type {
            CrateTypeName::Cdylib => crate::fingerprint::CrateTypeTag::Cdylib,
            CrateTypeName::Staticlib => crate::fingerprint::CrateTypeTag::Staticlib,
            CrateTypeName::Dylib => crate::fingerprint::CrateTypeTag::Dylib,
        };
        fp.compiler_service_schema = self.compiler_service_schema;
        fp
    }
}

/// Compiler-level configuration. Each `BuildService` carries its own.
#[derive(Clone, Debug)]
pub struct CompilerConfig {
    pub cache_root: PathBuf,
    pub defaults: ResolvedBuildConfig,
    pub stamps: ServiceStamps,
    pub cargo_timeout: Duration,
}

/// One cache-miss transaction request.
///
/// The renderer has already produced `rendered` (pure) and the resolver
/// has already produced `effective` + `fingerprint` from those same
/// bytes. The transaction method does the rest under a single
/// partition lock.
pub struct TransactionRequest {
    pub identity: CanonicalId,
    pub source_snapshot: Arc<Vec<u8>>,
    pub artifact_kind: ArtifactKind,
    /// Where the compiled artifact must be staged so the caller can
    /// publish it after the lock is released.
    pub staged_artifact: PathBuf,
    pub partition_key: PartitionKey,
    pub rendered: RenderedManifests,
    pub effective: ResolvedBuildConfig,
    pub fingerprint: BuildFingerprint,
}

/// Result of the cache-miss transaction. The lock has been released by
/// the time this is returned to the caller.
#[derive(Debug)]
pub enum TransactionResult {
    /// A previously-published generation matched. `staged_artifact` is
    /// NOT populated — the caller uses `cache.artifact_path(...)` for
    /// the existing artifact.
    CacheHit {
        fingerprint: BuildFingerprint,
        generation: PublishedGeneration,
        compiled_packages: Vec<PathBuf>,
    },
    /// A fresh build succeeded. `staged_artifact` carries the exact
    /// path the transaction wrote the compiled binary to. The caller
    /// publishes from this path (do NOT recompute the path: each
    /// `staging::stage_dir` call generates a fresh UUID and would miss
    /// the just-written artifact).
    Published {
        fingerprint: BuildFingerprint,
        generation_placeholder: PublishedGeneration,
        compiled_packages: Vec<PathBuf>,
        json_events: Vec<String>,
        stderr: Vec<String>,
        staged_artifact: PathBuf,
    },
    /// Cargo exited non-zero. Stderr captured.
    CompileError {
        stderr: Vec<String>,
        diagnostics: Vec<Diagnostic>,
        json_events: Vec<String>,
    },
    /// Transient infrastructure failure. Stderr captured if any.
    Transient {
        stderr: Vec<String>,
        diagnostics: Vec<Diagnostic>,
        json_events: Vec<String>,
    },
}

pub struct CompileOutcome {
    pub kind: CompileKind,
    pub stderr: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub compiled_packages: Vec<PathBuf>,
    pub json_events: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileKind {
    Success,
    CompileError,
    Transient,
}

/// The production compiler.
pub struct Tier1Compiler {
    pub(crate) config: CompilerConfig,
    pub(crate) supervisor: Arc<CargoSupervisor>,
    pub(crate) partitions: Arc<crate::cargo_target::PartitionRegistry>,
    pub(crate) _attempt_counter: AtomicU64,
}

impl Tier1Compiler {
    pub fn new(
        config: CompilerConfig,
        supervisor: Arc<CargoSupervisor>,
        partitions: Arc<crate::cargo_target::PartitionRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            supervisor,
            partitions,
            _attempt_counter: AtomicU64::new(1),
        })
    }

    pub fn partitions(&self) -> &Arc<crate::cargo_target::PartitionRegistry> {
        &self.partitions
    }

    pub fn cache_root(&self) -> &Path {
        &self.config.cache_root
    }

    pub fn allocate_attempt_id(&self) -> u64 {
        self._attempt_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// ONE per-partition transaction covering the entire cache-miss path
    /// (R7-1). Steps:
    ///
    /// 1. Acquire the partition lock.
    /// 2. Acquire a bounded child permit (N_CHILDREN).
    /// 3. Write the stable wrapper manifest, package manifest, and
    ///    overwrite the wrapper's `src/lib.rs` with the selected source.
    /// 4. Bootstrap the lockfile ONLY when absent.
    /// 5. Read the existing lockfile without regenerating it.
    /// 6. Verify the on-disk lockfile hash matches `req.effective.lock_resolution`
    ///    (drift → Transient, no regeneration).
    /// 7. Perform the authoritative cache lookup (active / inactive +
    ///    reactivation). Cache hit → release lock and return `CacheHit`.
    /// 8. On cache miss, run Cargo with `--locked` and
    ///    `--message-format=json-render-diagnostics`.
    /// 9. Reap, parse the JSON messages, locate the artifact, and copy
    ///    it to `staged_artifact`.
    /// 10. Release the partition lock.
    ///
    /// Different partitions remain parallel.
    pub fn run_transaction(
        &self,
        cache: Arc<ArtifactCache>,
        req: TransactionRequest,
    ) -> TransactionResult {
        if let Err(e) = validate_capabilities(&req.effective.capabilities) {
            return TransactionResult::Transient {
                stderr: vec![format!("capabilities: {e}")],
                diagnostics: vec![Diagnostic::error(format!("capabilities: {e}"))],
                json_events: Vec::new(),
            };
        }
        if !matches!(
            req.artifact_kind,
            ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script
        ) {
            return TransactionResult::Transient {
                stderr: vec![format!("unsupported artifact_kind: {:?}", req.artifact_kind)],
                diagnostics: vec![Diagnostic::error(format!(
                    "unsupported artifact_kind: {:?}",
                    req.artifact_kind
                ))],
                json_events: Vec::new(),
            };
        }

        let partition_key = req.partition_key.clone();
        let entry = self.partitions.get_or_insert(partition_key.clone(), self.cache_root());

        // ── ONE per-partition critical section ──
        let _partition_guard = partition_lock(&entry);
        let _permit = self.supervisor.acquire_child_permit();
        let generated_root = entry.target_dir.join("generated");

        // Step 3: write the stable wrapper manifest + package manifest
        // + overwrite the wrapper's src/lib.rs with the selected source.
        if let Err(e) = write_rendered_workspace_and_package(&generated_root, &req.rendered) {
            return TransactionResult::Transient {
                stderr: vec![format!("write manifests: {e}")],
                diagnostics: vec![Diagnostic::error(format!("write manifests: {e}"))],
                json_events: Vec::new(),
            };
        }
        // Step 3b: write the user's source verbatim to the wrapper
        // package's `src/lib.rs`. The compiler does NOT transform
        // user source — `crate::` references, crate-level attributes
        // and macros must work as if the file had been written by the
        // author. Durable identity is constructed later at the host's
        // registration boundary from the canonical identity; see
        // `renzora_plugin::host::durable_type_path`.
        if let Err(e) = write_partition_source(
            &generated_root,
            &req.rendered.package_name,
            &req.source_snapshot,
        ) {
            return TransactionResult::Transient {
                stderr: vec![format!("write source: {e}")],
                diagnostics: vec![Diagnostic::error(format!("write source: {e}"))],
                json_events: Vec::new(),
            };
        }

        // Step 4: bootstrap lockfile ONLY when absent.
        if let Err(e) = ensure_lockfile(&generated_root) {
            return TransactionResult::Transient {
                stderr: vec![format!("ensure_lockfile: {e}")],
                diagnostics: vec![Diagnostic::error(format!("ensure_lockfile: {e}"))],
                json_events: Vec::new(),
            };
        }

        // Step 5: read the existing lockfile without regenerating it.
        let lockfile_path = generated_root.join("Cargo.lock");
        let lockfile_hash = match std::fs::read(&lockfile_path) {
            Ok(b) => hash_bytes(&b),
            Err(e) => {
                return TransactionResult::Transient {
                    stderr: vec![format!("read lockfile: {e}")],
                    diagnostics: vec![Diagnostic::error(format!("read lockfile: {e}"))],
                    json_events: Vec::new(),
                };
            }
        };

        // Step 6: drift detection. The preview's lockfile hash was
        // ZERO when no lockfile existed yet. The transaction just
        // bootstrapped (or reused) the lockfile; the on-disk hash
        // is now authoritative. Drift = preview had a real hash AND
        // it does not match the on-disk bytes.
        if req.effective.lock_resolution != ContentHash::ZERO
            && req.effective.lock_resolution != lockfile_hash
        {
            return TransactionResult::Transient {
                stderr: vec![format!(
                    "lockfile hash drift: resolved={} actual={}",
                    req.effective.lock_resolution.to_hex(),
                    lockfile_hash.to_hex()
                )],
                diagnostics: vec![Diagnostic::error(format!(
                    "lockfile hash drift: resolved={} actual={}",
                    req.effective.lock_resolution.to_hex(),
                    lockfile_hash.to_hex()
                ))],
                json_events: Vec::new(),
            };
        }

        // Step 6b: rebuild `effective` and the fingerprint using the
        // actual on-disk lockfile hash. The preview may have used
        // ZERO (bootstrap case); the published fingerprint must use
        // the real hash so subsequent submits of the same source
        // match this generation's fingerprint.
        let effective = ResolvedBuildConfig {
            lock_resolution: lockfile_hash,
            lockfile_path: Some(lockfile_path.clone()),
            ..req.effective.clone()
        };
        let fingerprint = effective.fingerprint(&req.identity, &req.source_snapshot);
        let build_key = fingerprint.build_key();

        // Step 7: authoritative cache lookup under the lock.
        if let Some(active) = cache.read_active(&req.identity) {
            if let Ok(r) =
                verify_generation_fingerprint(&cache, &req.identity, active.generation, &fingerprint)
            {
                if r.matched {
                    return TransactionResult::CacheHit {
                        fingerprint,
                        generation: active.generation,
                        compiled_packages: Vec::new(),
                    };
                }
            }
        }
        if let Some(gen) = cache.lookup_inactive(&req.identity, &build_key) {
            if let Ok(r) =
                verify_generation_fingerprint(&cache, &req.identity, gen, &fingerprint)
            {
                if r.matched {
                    let _ = cache.write_active(
                        &req.identity,
                        &crate::staging::ActivePointer {
                            generation: gen,
                            fingerprint_hash: build_key.0,
                            compiler_service_schema: fingerprint.compiler_service_schema,
                        },
                        false,
                    );
                    return TransactionResult::CacheHit {
                        fingerprint,
                        generation: gen,
                        compiled_packages: Vec::new(),
                    };
                }
            }
        }

        // Step 8: spawn cargo with `--locked` and `--message-format=json`.
// `ensure_lockfile` already created `Cargo.lock` if it was absent,
// and the lockfile's hash is folded into the authoritative
// `effective.lock_resolution` used to derive the fingerprint. Every
// cargo invocation MUST respect that lockfile — including the first
// build of a fresh partition — so the produced artifact's
// dependency graph is exactly the one recorded in the cache key.
        let mut cmd = build_cargo_command_with_json(
            &effective,
            &generated_root,
            &req.rendered.package_name,
            &entry.target_dir,
        );
        cmd.current_dir(&generated_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let spawn_result = self
            .supervisor
            .spawn(req.identity.clone(), partition_key.clone(), cmd);
        let attempt_id = match spawn_result {
            Ok(id) => id,
            Err(e) => {
                return TransactionResult::Transient {
                    stderr: vec![format!("spawn cargo: {e}")],
                    diagnostics: vec![Diagnostic::error(format!("spawn cargo: {e}"))],
                    json_events: Vec::new(),
                };
            }
        };

        // Step 9: reap, parse, locate, stage.
        let deadline = Instant::now() + self.config.cargo_timeout;
        loop {
            if let Some(rec) = self.supervisor.try_reap(attempt_id) {
                let stderr = rec.stderr.clone();
                let (compiled, json_events) = parse_cargo_json_messages(&rec.stdout);
                match rec.exit_status {
                    Some(0) => {
                        // Locate the actual artefact using the cargo JSON
                        // `filenames` field — the cargo-emitted, target-
                        // relative paths are authoritative, so the
                        // per-identity `[lib] name` never has to be
                        // guessed. The selection is platform-aware
                        // (`lib<name>.so` / `lib<name>.dylib` /
                        // `<name>.dll`) so the same code path handles
                        // Linux, macOS, and Windows targets. A miss
                        // produces a diagnostic naming the target
                        // triple, the expected filename, and the full
                        // cargo-emitted filename list.
                        let actual_artifact = locate_artifact(
                            &compiled,
                            &req.rendered.lib_name,
                            &req.artifact_kind,
                            &effective.target_triple,
                        );
                        let real_path = match actual_artifact {
                            Ok(p) => p,
                            Err(miss) => {
                                return TransactionResult::CompileError {
                                    stderr: vec![miss.to_string()],
                                    diagnostics: vec![Diagnostic::error(miss.to_string())],
                                    json_events,
                                };
                            }
                        };
                        if let Err(e) = copy_to_staged(&real_path, &req.staged_artifact) {
                            return TransactionResult::Transient {
                                stderr: vec![format!("stage artifact: {e}")],
                                diagnostics: vec![Diagnostic::error(format!(
                                    "stage artifact: {e}"
                                ))],
                                json_events,
                            };
                        }
                        // _partition_guard drops here, releasing the
                        // partition lock.
                        drop(_partition_guard);
                        return TransactionResult::Published {
                            fingerprint,
                            generation_placeholder: PublishedGeneration(0),
                            compiled_packages: compiled,
                            json_events,
                            stderr,
                            staged_artifact: req.staged_artifact.clone(),
                        };
                    }
                    Some(_code) => {
                        let diagnostics = parse_cargo_diagnostics(&stderr);
                        return TransactionResult::CompileError {
                            stderr,
                            diagnostics,
                            json_events,
                        };
                    }
                    None => {
                        return TransactionResult::Transient {
                            stderr,
                            diagnostics: vec![Diagnostic::error("cargo wait error")],
                            json_events,
                        };
                    }
                }
            }
            if Instant::now() >= deadline {
                self.supervisor.cancel(attempt_id);
                let _ = self.supervisor.try_reap(attempt_id);
                return TransactionResult::Transient {
                    stderr: Vec::new(),
                    diagnostics: vec![Diagnostic::error(format!(
                        "cargo timeout after {:?}",
                        self.config.cargo_timeout
                    ))],
                    json_events: Vec::new(),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Validate the request's capabilities against `ALLOWED_CAPABILITIES`.
pub fn validate_capabilities(caps: &BTreeSet<String>) -> Result<(), String> {
    for c in caps {
        if !ALLOWED_CAPABILITIES.contains(&c.as_str()) {
            return Err(format!("unsupported capability: {c}"));
        }
    }
    Ok(())
}

/// Bootstrap the partition's `Cargo.lock` ONLY when absent.
///
/// Rules:
/// - If `<generated_root>/Cargo.lock` does not exist, run
///   `cargo generate-lockfile --manifest-path <generated_root>/Cargo.toml`
///   to create the authoritative lockfile.
/// - If the lockfile already exists, do NOT regenerate or overwrite it
///   — read the existing bytes and return its path. The compile step
///   will pass `--locked` to `cargo build`, which refuses to run when
///   the resolved graph has drifted from the on-disk lockfile.
pub fn ensure_lockfile(generated_root: &Path) -> Result<PathBuf, String> {
    let lockfile_path = generated_root.join("Cargo.lock");
    let manifest = generated_root.join("Cargo.toml");
    if lockfile_path.exists() {
        return Ok(lockfile_path);
    }
    let output = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .map_err(|e| format!("cargo generate-lockfile spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo generate-lockfile exited {:?}: {stderr}",
            output.status.code()
        ));
    }
    if !lockfile_path.exists() {
        return Err(format!(
            "lockfile not present at {} after generate-lockfile",
            lockfile_path.display()
        ));
    }
    Ok(lockfile_path)
}

/// Explicit dependency refresh. Drops the existing `Cargo.lock` (if any)
/// and re-runs `cargo generate-lockfile`. NOT called on the per-edit
/// hot path.
pub fn refresh_lockfile(generated_root: &Path) -> Result<PathBuf, String> {
    let lockfile_path = generated_root.join("Cargo.lock");
    if lockfile_path.exists() {
        std::fs::remove_file(&lockfile_path).map_err(|e| {
            format!("remove existing lockfile {}: {e}", lockfile_path.display())
        })?;
    }
    ensure_lockfile(generated_root)
}

/// One stable package name per partition (R7-1). Every script that
/// compiles inside this partition overwrites this single package's
/// `src/lib.rs` under the partition lock. The Cargo workspace therefore
/// has exactly one member regardless of how many script identities
/// share the partition, so adding a new script identity does not grow
/// workspace membership and does not require regenerating `Cargo.lock`.
pub fn stable_partition_package_name(pk: &PartitionKey) -> String {
    let canonical = format!(
        "{}-{}-{}-{}-abi{}-cs{}",
        pk.target_triple,
        pk.toolchain_stamp,
        pk.capabilities_canonical,
        pk.profile,
        pk.abi_version,
        pk.compiler_service_schema
    );
    let h = blake3::hash(canonical.as_bytes());
    let mut hex = String::with_capacity(12);
    for b in &h.as_bytes()[..6] {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("script_partition_{hex}")
}

/// Canonical workspace + package renderer (R7-1). **Completely pure**:
/// no `create_dir_all`, no source writes, no manifest writes, no
/// filesystem mutations of any kind. Returns the exact bytes the worker
/// will later write to disk under the partition lock.
///
/// Properties enforced here:
/// * Exactly ONE `resolver = "2"` line in the workspace.
/// * `renzora_plugin = { workspace = true, default-features = false,
///   features = [...] }` is ONE dependency table entry.
/// * `[profile.<name>]` carries `inherits = "release"` + `panic`.
/// * Workspace `members` is exactly ONE entry: the stable per-partition
///   wrapper package. There is no per-script workspace membership
///   growth.
pub fn render_workspace_and_package(
    partition_key: &PartitionKey,
    sdk_path: &Path,
    _source: &[u8],
    profile: ProfileName,
    panic: PanicMode,
    crate_type: CrateTypeName,
    capabilities: &BTreeSet<String>,
    identity: &CanonicalId,
) -> RenderedManifests {
    let package_name = stable_partition_package_name(partition_key);
    let lib_name = lib_name_for_identity(identity);
    let lib_ext = crate_type.as_cargo_str();
    let cargo_features: Vec<&str> = capabilities.iter().map(|s| s.as_str()).collect();

    // Workspace members: exactly ONE entry. All scripts in this
    // partition share the stable wrapper package; only its
    // `src/lib.rs` is overwritten per compile.
    let members = vec![package_name.clone()];

    let members_csv = format!("\"{}\"", package_name);

    // ONE single workspace Cargo.toml: ONE resolver, the SDK path dep in
    // workspace.dependencies, the [profile.<name>] block. The workspace
    // dep declares `default-features = false` so per-package
    // `[features]` overrides can safely enable specific capabilities.
    let sdk_path_str = sdk_path.to_string_lossy().into_owned();
    let workspace_toml = format!(
        "[workspace]\n\
         members = [{members_csv}]\n\
         resolver = \"2\"\n\
         \n\
         [workspace.dependencies]\n\
         renzora_plugin = {{ path = {sdk:?}, default-features = false, features = [] }}\n\
         \n\
         [profile.{profile}]\n\
         inherits = \"release\"\n\
         panic = \"{panic}\"\n",
        members_csv = members_csv,
        sdk = sdk_path_str,
        profile = profile.as_str(),
        panic = panic.as_cargo_str(),
    );

    // ONE package Cargo.toml: SDK features as FIELDS of the
    // renzora_plugin dependency, never as standalone entries.
    //
    // The workspace dep declares `default-features = false`. The
    // `renzora_plugin` crate requires `std` for a guest plugin (the
    // `add!` macro, the Bevy integration glue, `libm` fallback when
    // `std` is off). Always enable `std` on the per-package side; the
    // empty-features path previously relied on `workspace = true`
    // picking up `default-features = false` and produced plugins that
    // could not compile.
    let pkg_dep = if cargo_features.is_empty() {
        "renzora_plugin = { workspace = true, default-features = false, features = [\"std\"] }"
            .to_string()
    } else {
        let mut feats: Vec<String> = vec!["\"std\"".to_string()];
        for f in cargo_features {
            feats.push(format!("\"{}\"", f));
        }
        let feats_csv = feats.join(", ");
        format!(
            "renzora_plugin = {{ workspace = true, default-features = false, features = [{feats_csv}] }}",
        )
    };
    // The `[lib] name` is the per-canonical-identity crate name.
    // It is what `module_path!()` returns at the start of every
    // plugin-supplied type, and what cargo uses to name the produced
    // artefact. It is derived from the canonical identity (NOT from
    // `partition_key`) so the same canonical plugin produces the
    // SAME prefix in every partition — the saved-scene durability
    // invariant. The Cargo package name above stays per-partition
    // so workspace membership does not grow when a new identity
    // appears.
    let package_toml = format!(
        "[package]\n\
         name = \"{pkg}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         \n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"{lib_ext}\"]\n\
         \n\
         [dependencies]\n\
         {pkg_dep}\n",
        pkg = package_name,
        lib_name = lib_name,
        lib_ext = lib_ext,
        pkg_dep = pkg_dep,
    );

    RenderedManifests {
        package_name,
        lib_name,
        workspace_toml: workspace_toml.into_bytes(),
        package_toml: package_toml.into_bytes(),
        members,
    }
}

/// Compute the per-canonical-identity `[lib] name` used in the
/// generated wrapper's `Cargo.toml`. The output is a valid Rust
/// identifier: `renzora_plugin_<full-blake3>`. The full 256-bit
/// BLAKE3 (rendered as 64 hex characters) is used so collisions
/// with other identities have a cryptographic likelihood. The
/// identifier does not depend on `PartitionKey`, so a plugin
/// compiled for one target / profile / capability set
/// produces the SAME `module_path!()` prefix as the same plugin
/// compiled for another.
pub fn lib_name_for_identity(identity: &CanonicalId) -> String {
    use std::fmt::Write as _;
    let canonical = identity.to_scheme_path();
    let mut hex = String::with_capacity(64);
    for b in blake3::hash(canonical.as_bytes()).as_bytes() {
        let _ = write!(hex, "{b:02x}");
    }
    format!("renzora_plugin_{hex}")
}

/// Write the canonical workspace + package `Cargo.toml` bytes to disk.
/// Does NOT write the source — call `write_partition_source` for that.
/// Writes are idempotent: existing files whose content equals the
/// rendered bytes are left untouched (preserves mtime).
pub fn write_rendered_workspace_and_package(
    generated_root: &Path,
    rendered: &RenderedManifests,
) -> Result<(), String> {
    std::fs::create_dir_all(generated_root)
        .map_err(|e| format!("create generated root: {e}"))?;
    std::fs::create_dir_all(generated_root.join(&rendered.package_name))
        .map_err(|e| format!("create package dir: {e}"))?;
    std::fs::create_dir_all(generated_root.join(&rendered.package_name).join("src"))
        .map_err(|e| format!("create package src dir: {e}"))?;
    write_if_changed(&generated_root.join("Cargo.toml"), &rendered.workspace_toml)
        .map_err(|e| format!("write workspace Cargo.toml: {e}"))?;
    write_if_changed(
        &generated_root.join(&rendered.package_name).join("Cargo.toml"),
        &rendered.package_toml,
    )
    .map_err(|e| format!("write package Cargo.toml: {e}"))?;
    Ok(())
}

/// Overwrite the stable wrapper package's `src/lib.rs` with the
/// selected source bytes. Called under the partition lock so concurrent
/// scripts targeting the same partition do not race.
pub fn write_partition_source(
    generated_root: &Path,
    package_name: &str,
    source: &[u8],
) -> Result<(), String> {
    let path = generated_root.join(package_name).join("src").join("lib.rs");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create src dir {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, source).map_err(|e| format!("write src/lib.rs: {e}"))
}

/// Build the actual Cargo command from the validated
/// `ResolvedBuildConfig`.
pub fn build_cargo_command(
    effective: &ResolvedBuildConfig,
    generated_root: &Path,
    package_name: &str,
    target_dir: &Path,
) -> std::process::Command {
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build")
        .arg("--profile")
        .arg(effective.profile.as_str())
        .arg("--target")
        .arg(&effective.target_triple)
        .arg("--manifest-path")
        .arg(generated_root.join("Cargo.toml"))
        .arg("--package")
        .arg(package_name)
        .arg("--target-dir")
        .arg(target_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env(
            format!("{}_OPT_LEVEL", effective.profile_env_prefix()),
            "3",
        )
        .env(
            format!("{}_PANIC", effective.profile_env_prefix()),
            effective.panic.as_cargo_str(),
        )
        .env(
            format!("{}_CODEGEN_UNITS", effective.profile_env_prefix()),
            "1",
        );

    // Capabilities are forwarded to the `renzora_plugin` dependency
    // via the per-package `Cargo.toml` the renderer emits
    // (`renzora_plugin = { ..., features = ["static_plugins"] }`).
    // Do NOT pass `--features` on the cargo CLI: that flag enables
    // features on the package being built, not on the dependency.
    // Passing capability names here makes cargo look for them on the
    // WRAPPER package (`script_partition_*`), which doesn't declare
    // them — see `prod_non_empty_capability_real_cargo_build`.
    let cargo_features = &effective.cargo_features;
    if cargo_features.is_empty() {
        // nothing — keep the no-op explicit so the intent is visible.
    }

    if !effective.rustflags.is_empty() {
        cmd.env("CARGO_ENCODED_RUSTFLAGS", effective.rustflags.join("\x1f"));
    }

    if effective.lockfile_path.is_some() {
        cmd.arg("--locked");
    }

    cmd
}

/// Build the production Cargo command with machine-readable
/// `--message-format=json-render-diagnostics` (R6-5).
pub fn build_cargo_command_with_json(
    effective: &ResolvedBuildConfig,
    generated_root: &Path,
    package_name: &str,
    target_dir: &Path,
) -> std::process::Command {
    let mut cmd = build_cargo_command(effective, generated_root, package_name, target_dir);
    cmd.arg("--message-format").arg("json-render-diagnostics");
    cmd
}

/// Internal write helper. Idempotent.
pub fn write_if_changed_pub(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_if_changed(path, bytes)
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match std::fs::read(path) {
        Ok(existing) if existing == bytes => Ok(()),
        _ => std::fs::write(path, bytes),
    }
}

/// Platform-specific filename conventions for Rust cdylib / staticlib
/// outputs. The cross-platform selection table below is the
/// authoritative reference — see also
/// `docs/r1-alpha7/extending/standalone-plugins.md` for the user-
/// facing description.
///
/// Dynamic libraries:
///
/// | target triple         | filename         |
/// | ---                   | ---              |
/// | `*-linux-*`           | `lib<name>.so`   |
/// | `*-apple-darwin`      | `lib<name>.dylib`|
/// | `*-windows-msvc`      | `<name>.dll`     |
/// | `*-windows-gnu`       | `<name>.dll`     |
///
/// Static libraries:
///
/// | target triple         | filename         |
/// | ---                   | ---              |
/// | `*-linux-*`           | `lib<name>.a`    |
/// | `*-apple-darwin`      | `lib<name>.a`    |
/// | `*-windows-msvc`      | `<name>.lib`     |
/// | `*-windows-gnu`       | `lib<name>.a`    |  (rustc's GNU target emits the Unix `lib` prefix)
///
/// Distractor filenames we MUST NOT pick:
///
///   - the SDK's own cdylib (`librenzora_plugin.so`,
///     `librenzora_plugin.dylib`, `renzora_plugin.dll`);
///   - Windows MSVC import libraries (`<name>.dll.lib`);
///   - Windows export sidecars (`<name>.exp`, `<name>.pdb`);
///   - rustc intermediates (`*.d`, `*.rlib`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactPlatform {
    LinuxGnu,
    LinuxMusl,
    MacOS,
    WindowsMsvc,
    WindowsGnu,
    /// Anything else (e.g. wasm32, illumos). Used as a last-resort
    /// branch that picks the most-likely correct filename for the
    /// target's documented Rust output.
    Other,
}

impl ArtifactPlatform {
    /// Derive the platform from a target triple. The token right
    /// before the vendor (e.g. `linux-gnu`, `apple-darwin`,
    /// `windows-msvc`) drives the choice.
    pub fn from_target_triple(target: &str) -> Self {
        if target.contains("apple-darwin") {
            ArtifactPlatform::MacOS
        } else if target.contains("windows-msvc") {
            ArtifactPlatform::WindowsMsvc
        } else if target.contains("windows-gnu") {
            ArtifactPlatform::WindowsGnu
        } else if target.contains("linux-gnu") {
            ArtifactPlatform::LinuxGnu
        } else if target.contains("linux-musl") {
            ArtifactPlatform::LinuxMusl
        } else {
            ArtifactPlatform::Other
        }
    }

    /// Expected on-disk filename for the supplied `kind` and
    /// `lib_name`. Used to drive the artefact selection. Returns
    /// `None` for combinations the platform does not produce (only
    /// happens for the `Other` fallback).
    pub fn expected_filename(&self, kind: &ArtifactKind, lib_name: &str) -> Option<String> {
        match kind {
            ArtifactKind::StaticLib => match self {
                ArtifactPlatform::LinuxGnu
                | ArtifactPlatform::LinuxMusl
                | ArtifactPlatform::MacOS
                | ArtifactPlatform::WindowsGnu => Some(format!("lib{lib_name}.a")),
                ArtifactPlatform::WindowsMsvc => Some(format!("{lib_name}.lib")),
                ArtifactPlatform::Other => None,
            },
            ArtifactKind::DynamicLib { .. }
            | ArtifactKind::Tier1Plugin
            | ArtifactKind::Tier1Script => match self {
                ArtifactPlatform::LinuxGnu | ArtifactPlatform::LinuxMusl => {
                    Some(format!("lib{lib_name}.so"))
                }
                ArtifactPlatform::MacOS => Some(format!("lib{lib_name}.dylib")),
                ArtifactPlatform::WindowsMsvc | ArtifactPlatform::WindowsGnu => {
                    Some(format!("{lib_name}.dll"))
                }
                ArtifactPlatform::Other => None,
            },
        }
    }

    /// Suffixes the cargo `filenames` array might list for this
    /// kind and platform. Used by the distractor / fallback scan:
    /// we ignore any entry whose extension is not in this set, so
    /// `*.pdb`, `*.exp`, `*.d`, `*.dll.lib`, `*.rlib` and
    /// `*.rmeta` never reach the equality check.
    pub fn allowed_extensions(&self, kind: &ArtifactKind) -> &'static [&'static str] {
        match kind {
            ArtifactKind::StaticLib => match self {
                ArtifactPlatform::WindowsMsvc => &["lib"],
                _ => &["a"],
            },
            ArtifactKind::DynamicLib { .. }
            | ArtifactKind::Tier1Plugin
            | ArtifactKind::Tier1Script => match self {
                ArtifactPlatform::MacOS => &["dylib"],
                ArtifactPlatform::LinuxGnu | ArtifactPlatform::LinuxMusl => &["so"],
                ArtifactPlatform::WindowsMsvc | ArtifactPlatform::WindowsGnu => &["dll"],
                ArtifactPlatform::Other => &["so", "dll", "dylib", "a", "lib"],
            },
        }
    }
}

/// Diagnostic returned to the caller when no candidate matched.
/// The full Cargo-emitted filename list is included so a human
/// (or a higher-level caller) can see WHY the matcher picked
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocateArtifactMiss {
    pub lib_name: String,
    pub target_triple: String,
    pub platform: ArtifactPlatform,
    pub kind: ArtifactKind,
    pub expected_filename: Option<String>,
    /// Boxed so the struct stays small (the miss is otherwise
    /// bounded by Cargo's full filenames array, which can be
    /// hundreds of entries on a real build).
    pub filenames: Box<Vec<String>>,
}

impl std::fmt::Display for LocateArtifactMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no cargo artifact matches lib_name=`{}` for target triple `{}` \
             (platform={:?}, kind={:?}); expected `{}`; cargo emitted: [{}]",
            self.lib_name,
            self.target_triple,
            self.platform,
            self.kind,
            self.expected_filename.as_deref().unwrap_or("<no expected filename>"),
            self.filenames.join(", ")
        )
    }
}

impl std::error::Error for LocateArtifactMiss {}

/// Locate the actual compiled artifact under `<target_dir>/<profile>/`
/// or `<target_dir>/<target_triple>/<profile>/`.
///
/// Selection is platform-aware: the on-disk filename convention is
/// derived from `target_triple`. The wrapper's cdylib is
/// `lib<lib_name>.so` / `lib<lib_name>.dylib` / `<lib_name>.dll`
/// depending on the target; this function picks the one matching
/// the platform. Selection is exact — `lib_name` is matched against
/// the wrapper's own `[lib] name` (per-canonical-identity), so the
/// SDK's `librenzora_plugin.so` cannot be picked when `lib_name` is
/// anything else.
///
/// Distractors ignored:
///
///   - the SDK's `renzora_plugin` artifacts (cdylib or import lib);
///   - Windows MSVC `<name>.dll.lib` import libraries (these are
///     paired with `<name>.dll` but are themselves link inputs, not
///     load targets);
///   - Windows `<name>.exp` export tables and `<name>.pdb` debug
///     symbols;
///   - rustc intermediates (`*.d`, `*.rlib`, `*.rmeta`);
///   - another identity's cdylib (`lib_other_<hash>.so` etc.).
///
/// On a miss the function returns `Err(LocateArtifactMiss)` so the
/// caller can surface a diagnostic that names the platform, the
/// expected filename, and the full Cargo-emitted list.
pub fn locate_artifact(
    cargo_filenames: &[PathBuf],
    lib_name: &str,
    kind: &ArtifactKind,
    target_triple: &str,
) -> Result<PathBuf, LocateArtifactMiss> {
    let platform = ArtifactPlatform::from_target_triple(target_triple);
    let expected = platform.expected_filename(kind, lib_name);
    let allowed_exts = platform.allowed_extensions(kind);
    let filename_list: Vec<String> = cargo_filenames
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()))
        .collect();
    let candidates: Vec<&PathBuf> = cargo_filenames
        .iter()
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            // The SDK's own artifact must NEVER be picked. Both the
            // `lib<lib_name>.so` form and the no-prefix
            // `<lib_name>.dll` form are excluded.
            if name.starts_with("librenzora_plugin.") || name.starts_with("renzora_plugin.") {
                return false;
            }
            // Extension gate: only the platform-allowed set.
            let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            if !allowed_exts.contains(&ext) {
                return false;
            }
            // The Windows MSVC import-library sidecar
            // `<name>.dll.lib` ends in `lib` (allowed for static
            // libs) but its stem contains `<name>.dll`, not
            // `<name>`. Reject anything where the stem is itself a
            // library with an extension.
            if name.ends_with(".dll.lib") {
                return false;
            }
            true
        })
        .collect();
    // Exact-match primary path: pick the candidate whose file name
    // equals the platform-expected name verbatim. This is the
    // production-typical case.
    if let Some(expected_name) = &expected {
        if let Some(p) = candidates
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(expected_name.as_str()))
        {
            return Ok((*p).clone());
        }
    }
    // No exact match. The platform's strict extension filter has
    // already removed the wrong-platform artefacts; the only
    // remaining candidates are target-appropriate and don't carry
    // the expected name. Report a miss so the operator can see
    // the platform-expected filename and the actual Cargo-emitted
    // list rather than silently picking an oddly-named but
    // extension-compatible artifact.
    Err(LocateArtifactMiss {
        lib_name: lib_name.to_string(),
        target_triple: target_triple.to_string(),
        platform,
        kind: *kind,
        expected_filename: expected,
        filenames: Box::new(filename_list),
    })
}

fn copy_to_staged(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

fn parse_cargo_diagnostics(stderr: &[String]) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut current: Option<(String, Option<u32>, Option<u32>)> = None;
    for line in stderr {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Finished")
            || trimmed.starts_with("Locking")
            || trimmed.starts_with("Updating")
            || trimmed.starts_with("Downloaded")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Installing")
            || trimmed.starts_with("Build")
            || trimmed.starts_with("Running")
            || trimmed.starts_with("Fresh")
            || trimmed.starts_with("Checking")
        {
            current = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("error") {
            if let Some((msg, line_, col)) = current.take() {
                diags.push(Diagnostic {
                    severity: crate::types::Severity::Error,
                    line: line_,
                    column: col,
                    message: msg,
                });
            }
            let msg = rest.trim_start_matches(':').trim().to_string();
            current = Some((msg, None, None));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("warning") {
            if let Some((msg, line_, col)) = current.take() {
                diags.push(Diagnostic {
                    severity: crate::types::Severity::Warning,
                    line: line_,
                    column: col,
                    message: msg,
                });
            }
            let msg = rest.trim_start_matches(':').trim().to_string();
            current = Some((msg, None, None));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("-->") {
            let cleaned = rest.trim().trim_start_matches("src/lib.rs:");
            if let Some((line_str, col_str)) = cleaned.split_once(':') {
                if let (Ok(l), Ok(c)) = (line_str.parse::<u32>(), col_str.parse::<u32>()) {
                    if let Some(ref mut cur) = current {
                        cur.1 = Some(l);
                        cur.2 = Some(c);
                    }
                }
            }
            continue;
        }
        if let Some(ref mut c) = current {
            if !trimmed.is_empty() {
                if !c.0.is_empty() {
                    c.0.push(' ');
                }
                c.0.push_str(trimmed);
            }
        }
    }
    if let Some((msg, line_, col)) = current.take() {
        diags.push(Diagnostic {
            severity: crate::types::Severity::Error,
            line: line_,
            column: col,
            message: msg,
        });
    }
    diags
}

/// Parse cargo's machine-readable JSON messages from stdout (R6-5).
/// Extracts `compiler-artifact` events with `fresh: false` (cargo
/// actually recompiled the package). Cached artefacts re-emitted as
/// `compiler-artifact` with `fresh: true` are SKIPPED.
///
/// Returns a list of produced artefact file paths (read from
/// `compiler-artifact.filenames` — the cargo-emitted, target-
/// relative `filenames` field, which is the authoritative source
/// for what the build produced). The list is the union across all
/// non-cached artefacts; a `compiler-artifact.fresh: true` is
/// skipped. The caller locates the freshly-built plugin cdylib
/// from this list rather than guessing a filename derived from
/// the package or lib name.
pub fn parse_cargo_json_messages(stdout_lines: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut compiled = Vec::new();
    let mut events = Vec::new();
    for line in stdout_lines {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        events.push(line.to_string());
        let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("");
        let fresh = v.get("fresh").and_then(|f| f.as_bool()).unwrap_or(false);
        if reason == "compiler-artifact" && !fresh {
            if let Some(filenames) = v.get("filenames").and_then(|f| f.as_array()) {
                for f in filenames {
                    if let Some(s) = f.as_str() {
                        let p = std::path::PathBuf::from(s);
                        if !compiled.iter().any(|c| c == &p) {
                            compiled.push(p);
                        }
                    }
                }
            }
        }
    }
    (compiled, events)
}

/// Capture the actual `rustc -Vv` output. Used to authoritatively set
/// `ResolvedBuildConfig::toolchain_stamp`.
pub fn capture_toolchain_stamp() -> String {
    std::process::Command::new("rustc")
        .arg("-Vv")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Hash a directory's contents deterministically (file path + bytes,
/// sorted). SHA-256 of `path|bytes` lines concatenated.
pub fn hash_directory(path: &Path) -> ContentHash {
    let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    walk_dir(path, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (p, b) in &entries {
        h.update(p.to_string_lossy().as_bytes());
        h.update(b"\0");
        h.update(b);
    }
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    ContentHash(arr)
}

fn walk_dir(path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(path) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk_dir(&p, out);
        } else if meta.is_file() {
            let bytes = std::fs::read(&p).unwrap_or_default();
            out.push((p, bytes));
        }
    }
}

/// Hash arbitrary bytes.
pub fn hash_bytes(b: &[u8]) -> ContentHash {
    let mut h = Sha256::new();
    h.update(b);
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    ContentHash(arr)
}

/// Inputs the caller may legitimately supply per request.
#[derive(Clone, Debug)]
pub struct RequestChoices {
    pub target_triple: String,
    pub capabilities: BTreeSet<String>,
    pub profile: ProfileName,
    pub panic: PanicMode,
    pub rustflags: Vec<String>,
}

/// Resolve the authoritative `ResolvedBuildConfig` from the caller-
/// supplied `RequestChoices` + the engine-owned `defaults` + the cached
/// service stamps + the canonical render output.
///
/// `lockfile_hash` is the SHA-256 of the on-disk lockfile when one
/// exists, or `ContentHash::ZERO` when the partition has never been
/// bootstrapped. The caller (preview_resolve or the worker) computes it
/// from a read of `<generated_root>/Cargo.lock` before this function is
/// invoked. The resolver does not touch the filesystem for the lockfile.
pub fn resolve_build_config(
    _identity: &CanonicalId,
    source: &[u8],
    choices: &RequestChoices,
    artifact_kind: ArtifactKind,
    defaults: &ResolvedBuildConfig,
    lockfile_hash: ContentHash,
    stamps: &ServiceStamps,
    rendered: &RenderedManifests,
) -> Result<ResolvedBuildConfig, String> {
    if stamps.toolchain_stamp.is_empty() {
        return Err("toolchain stamp unavailable (stamps not captured)".into());
    }
    let resolved_toolchain = stamps.toolchain_stamp.clone();

    let resolved_target = if choices.target_triple.is_empty() {
        defaults.target_triple.clone()
    } else {
        choices.target_triple.clone()
    };
    if resolved_target.is_empty() {
        return Err("target_triple not provided and engine default is empty".into());
    }

    let resolved_sdk_hash = stamps.sdk_content_hash;

    let resolved_abi_version = defaults.abi_version;
    let mut resolved_iface_hashes = defaults.interface_prefix_hashes.clone();
    resolved_iface_hashes.sort_unstable();

    validate_capabilities(&choices.capabilities)?;
    let resolved_capabilities = choices.capabilities.clone();
    let resolved_cargo_features: Vec<String> = resolved_capabilities.iter().cloned().collect();

    let wrapper_hash_bytes =
        rendered.wrapper_hash_bytes(source, &resolved_target, &choices.rustflags);
    let resolved_wrapper_hash = hash_bytes(&wrapper_hash_bytes);
    let resolved_wrapper_schema = defaults.wrapper_schema;

    let manifest_descriptor = format!(
        "renzora_compiler_cache manifest schema {}: (active.bin, fingerprint.bin, status.bin)",
        defaults.manifest_schema
    );
    let resolved_manifest_hash = hash_bytes(manifest_descriptor.as_bytes());
    let resolved_manifest_schema = defaults.manifest_schema;

    let resolved_lock_path = if lockfile_hash != ContentHash::ZERO {
        // Caller did not pass the path; we cannot reconstruct the
        // partition here. The transaction path reads the lockfile
        // again under its own lock; the lockfile_path field is set
        // to None at resolve time and re-derived in the worker.
        None
    } else {
        None
    };

    let lib_ext = match artifact_kind {
        ArtifactKind::StaticLib => CrateTypeName::Staticlib,
        ArtifactKind::DynamicLib { .. } => CrateTypeName::Dylib,
        ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script => CrateTypeName::Cdylib,
    };

    Ok(ResolvedBuildConfig {
        target_triple: resolved_target,
        toolchain_stamp: resolved_toolchain,
        sdk_path: defaults.sdk_path.clone(),
        sdk_content_hash: resolved_sdk_hash,
        abi_version: resolved_abi_version,
        interface_prefix_hashes: resolved_iface_hashes,
        wrapper_schema: resolved_wrapper_schema,
        wrapper_content_hash: resolved_wrapper_hash,
        manifest_schema: resolved_manifest_schema,
        manifest_content_hash: resolved_manifest_hash,
        lock_resolution: lockfile_hash,
        lockfile_path: resolved_lock_path,
        capabilities: resolved_capabilities,
        cargo_features: resolved_cargo_features,
        profile: choices.profile,
        rustflags: choices.rustflags.clone(),
        panic: choices.panic,
        crate_type: lib_ext,
        compiler_service_schema: defaults.compiler_service_schema,
    })
}

/// Cached service-wide authoritative stamps (R6-6).
#[derive(Clone, Debug)]
pub struct ServiceStamps {
    pub toolchain_stamp: String,
    pub sdk_content_hash: ContentHash,
    pub generation: u64,
}

impl ServiceStamps {
    pub fn capture(sdk_path: &Path, generation: u64) -> Self {
        let toolchain_stamp = capture_toolchain_stamp();
        let sdk_content_hash = if sdk_path.is_dir() {
            hash_directory(&crate::sdk::content_root(sdk_path))
        } else {
            ContentHash::ZERO
        };
        Self {
            toolchain_stamp,
            sdk_content_hash,
            generation,
        }
    }
}
