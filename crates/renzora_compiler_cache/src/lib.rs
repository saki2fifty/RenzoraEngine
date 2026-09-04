//! `compiler_cache` crate root.
//!
//! Bevy-independent cached compiler service for the A27 tiered Rust extension
//! migration. Phase 2. See `docs/r1-alpha7/editor-dev/build-cache.md` for the
//! user-facing description and
//! `AgentFiles/Documentation/phase2-cached-compiler-design.md` for the
//! authoritative architecture this implementation follows.
//!
//! The crate exposes:
//!
//! - [`BuildService`]: the single entry point for submitting a [`BuildRequest`]
//!   and querying the published artifact cache.
//! - [`types`]: the boundary types (`BuildRequest`, `BuildResult`,
//!   `BuildOutcome`, [`ArtifactKind`], [`Diagnostic`]).
//! - [`fingerprint`]: the versioned, length-delimited [`fingerprint::BuildFingerprint`].
//! - [`scheduler`]: the single-lock [`scheduler::ReadyScheduler`] that owns
//!   ready-queue + in-flight membership.
//! - [`desired`]: the per-id desired-revision store that the scheduler drives.
//! - [`worker`]: the bounded [`worker::WorkerPool`] that drains the scheduler
//!   into cargo invocations.
//! - [`cargo_target`]: the shared Cargo target partition registry and the
//!   `PartitionLock` mutex.
//! - [`process`]: the [`process::CargoSupervisor`] with process-tree ownership
//!   and bounded shutdown.
//! - [`staging`]: the immutable staging and publication protocol with
//!   `replace_active_pointer` (POSIX rename / Windows `ReplaceFileW`).
//! - [`retention`]: the two retention budgets and the
//!   active/mapped/pinned-never-evicted set.
//! - [`recovery`]: startup reconciliation over staged/abandoned generations.
//!
//! Every public type is `Send + Sync` where it is shared across the worker
//! pool. The crate compiles without `bevy`, without `renzora_rust_script`,
//! and without the old native plugin / dylib loaders — Phase 2 invariant.

#![deny(unsafe_op_in_unsafe_fn)]
// `#![warn(missing_docs)]` is intentionally not set globally. The
// crate is in active development; missing-doc warnings are treated
// as lint, not errors, and are addressed per-PR rather than
// blocking compilation.

pub mod cargo_target;
pub mod compiler;
pub mod desired;
pub mod fingerprint;
pub mod loader;
pub mod process;
pub mod recovery;
pub mod retention;
pub mod scheduler;
pub mod service;
pub mod staging;
pub mod status;
pub mod types;
pub mod worker;

pub use fingerprint::{BuildFingerprint, BuildKey};
pub use loader::{LoadError, LoadedLibrary, Loader};
pub use staging::ArtifactCache;
pub use scheduler::{ReadyScheduler, SchedulerConfig};
pub use service::{BuildService, BuildServiceConfig, BuildServiceError, ShutdownReport};
pub use shared::SharedBuildService;
pub use staging::{from_safe_id_dir_name, safe_id_dir_name};
pub use types::{
    ArtifactKind, BuildOutcome, BuildRequest, BuildResult, CancelCause, Diagnostic, FingerprintInputs,
    Revision, Severity,
};

pub mod shared;
