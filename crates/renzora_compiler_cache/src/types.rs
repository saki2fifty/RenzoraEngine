//! Boundary types for the cached compiler service.
//!
//! These types are the only public surface Phases 3 and 4 (and the Bevy
//! adapter) consume. They do NOT depend on Bevy, on `renzora_rust_script`, on
//! the old native plugin loaders, or on the Rust trait-object dynamic plugin
//! ABI. See `phase2-cached-compiler-design.md` §10 for the design contract.

use std::path::PathBuf;
use std::sync::Arc;

use renzora_identity::CanonicalId;

use crate::fingerprint::BuildFingerprint;

/// Monotonic per-`CanonicalId` revision counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Revision(pub u64);

impl Revision {
    /// Revision 0 — the "no edit yet" sentinel.
    pub const ZERO: Self = Self(0);

    /// Bump the revision by one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// True when `self` is strictly greater than `other`.
    pub fn is_greater_than(self, other: Self) -> bool {
        self.0 > other.0
    }
}

/// What kind of artifact a `BuildRequest` asks for. The cache key includes
/// `crate_type` (Tier 1 SDK ships a single `cdylib` per surface), so changing
/// the kind of artifact requires a different cache partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// A regular static library — `.a` on POSIX, `.lib` on Windows.
    StaticLib,
    /// A regular dynamic library — `.so` / `.dll` / `.dylib` with an
    /// optional platform-specific load prefix.
    DynamicLib {
        /// Load prefix (`lib` on Linux/macOS, empty on Windows).
        load_prefix: &'static str,
    },
    /// A Tier 1 C-ABI plugin (`renzora_plugin` surface). Phase 3 consumer.
    Tier1Plugin,
    /// A Tier 1 C-ABI script (`renzora_script` surface). Phase 4 consumer.
    Tier1Script,
}

impl ArtifactKind {
    /// Crate-type tag for fingerprint inputs.
    pub fn crate_type_tag(&self) -> &'static str {
        match self {
            ArtifactKind::StaticLib => "staticlib",
            ArtifactKind::DynamicLib { .. } => "dylib",
            ArtifactKind::Tier1Plugin => "cdylib-plugin",
            ArtifactKind::Tier1Script => "cdylib-script",
        }
    }

    /// Default library extension for this kind on the host platform.
    pub fn default_lib_ext(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            match self {
                ArtifactKind::StaticLib => "lib",
                ArtifactKind::DynamicLib { .. } | ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script => "dll",
            }
        }
        #[cfg(target_os = "macos")]
        {
            match self {
                ArtifactKind::StaticLib => "a",
                ArtifactKind::DynamicLib { .. } | ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script => "dylib",
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match self {
                ArtifactKind::StaticLib => "a",
                ArtifactKind::DynamicLib { .. } | ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script => "so",
            }
        }
    }
}

/// Inputs that are not the source bytes but DO influence the cache key.
/// Filled by the Bevy adapter; consumed by `BuildFingerprint::with_inputs`.
#[derive(Clone, Debug)]
pub struct FingerprintInputs {
    /// Target triple the artifact is being built for (e.g. `x86_64-unknown-linux-gnu`).
    pub target_triple: String,
    /// Full `rustc -Vv` output captured at SDK build time.
    pub toolchain_stamp: String,
    /// 32-byte content hash of the Tier 1 SDK package.
    pub sdk_content_hash: [u8; 32],
    /// Tier 1 ABI version + sorted `INTERFACE_PREFIX_HASHES` (each u32 LE).
    pub abi_version: u32,
    /// Interface-prefix hashes — one u32 per C-ABI symbol.
    pub interface_prefix_hashes: Vec<u32>,
    /// Wrapper template schema version.
    pub wrapper_schema: u32,
    /// Manifest schema version (the on-disk layout of `active.bin` /
    /// `fingerprint.bin` / `status.bin`).
    pub manifest_schema: u32,
    /// SHA-256 of the canonical dep lock emitted by the cache.
    pub lock_resolution: [u8; 32],
    /// Enabled capabilities / features — `runtime`, `static_plugins`, …
    pub capabilities: std::collections::BTreeSet<String>,
    /// Build profile.
    pub profile: BuildProfile,
    /// Cargo `--config` profile overrides.
    pub rustflags: Vec<String>,
    /// Panic strategy.
    pub panic: PanicStrategy,
    /// Version of `compiler_cache` itself.
    pub compiler_service_schema: u32,
}

impl Default for FingerprintInputs {
    fn default() -> Self {
        Self {
            target_triple: default_target_triple(),
            toolchain_stamp: String::new(),
            sdk_content_hash: [0; 32],
            abi_version: 1,
            interface_prefix_hashes: Vec::new(),
            wrapper_schema: 1,
            manifest_schema: 1,
            lock_resolution: [0; 32],
            capabilities: std::collections::BTreeSet::new(),
            profile: BuildProfile::Dist,
            rustflags: Vec::new(),
            panic: PanicStrategy::Abort,
            compiler_service_schema: COMPILER_SERVICE_SCHEMA,
        }
    }
}

/// Build profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildProfile {
    /// `dist` profile (fast link, opt-level 2).
    Dist,
    /// `dist-lean` profile (size-optimised).
    DistLean,
}

impl BuildProfile {
    /// Stable string tag for fingerprint inputs.
    pub fn as_tag(&self) -> &'static str {
        match self {
            BuildProfile::Dist => "dist",
            BuildProfile::DistLean => "dist-lean",
        }
    }
}

/// Panic strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanicStrategy {
    /// `panic = "abort"`.
    Abort,
    /// `panic = "unwind"`.
    Unwind,
}

impl PanicStrategy {
    /// Single-byte tag for fingerprint binary encoding.
    pub fn byte(&self) -> u8 {
        match self {
            PanicStrategy::Abort => 0,
            PanicStrategy::Unwind => 1,
        }
    }

    /// Parse a single-byte tag.
    pub fn from_byte(b: u8) -> Result<Self, String> {
        match b {
            0 => Ok(PanicStrategy::Abort),
            1 => Ok(PanicStrategy::Unwind),
            _ => Err(format!("unknown panic byte {b}")),
        }
    }
}

/// Current `compiler_cache` public-schema version. Bumped when the on-disk
/// format or `BuildFingerprint` field table changes; existing caches must be
/// rebuilt after a bump. See `fingerprint::BuildFingerprint::schema_version`.
pub const COMPILER_SERVICE_SCHEMA: u32 = 1;

/// Default target triple for the host this crate is compiled on. Tests
/// substitute explicit values; production replaces via the SDK. Returns a
/// triple cargo recognises (e.g. `x86_64-unknown-linux-gnu`).
pub fn default_target_triple() -> String {
    match std::env::consts::OS {
        "linux" => format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
        "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
        "windows" => format!("{}-pc-windows-msvc", std::env::consts::ARCH),
        other => format!("{}-unknown-{}", std::env::consts::ARCH, other),
    }
}

/// Default Tier-1 script entry-point symbol name. The Bevy adapter
/// overrides this with the project's chosen entry-point.
pub fn default_script_symbol() -> &'static [u8] {
    b"renzora_script_update\0"
}

/// A request to compile `identity` at the supplied source snapshot. The
/// `Arc<Vec<u8>>` keeps the source bytes alive for the lifetime of the
/// future returned by `BuildService::submit`, regardless of how many worker
/// ticks pass between dequeuing and re-serializing.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// Canonical identity (already covers scheme + path).
    pub identity: CanonicalId,
    /// Source bytes — `Arc<Vec<u8>>` so the snapshot is reference-counted and
    /// cheaply cloneable to a fingerprint or a future.
    pub source_snapshot: Arc<Vec<u8>>,
    /// Non-source fingerprint inputs.
    pub fingerprint_inputs: FingerprintInputs,
    /// Target triple (overrides `fingerprint_inputs.target_triple` for
    /// convenience).
    pub target: String,
    /// What kind of artifact is being requested.
    pub artifact_kind: ArtifactKind,
}

impl BuildRequest {
    /// **Preview-only** fingerprint. Computed from caller-supplied
    /// `FingerprintInputs` and is NOT the production cache key. The
    /// production cache key is derived exclusively from the authoritative
    /// `ResolvedBuildConfig::fingerprint()` (R5B-1). This method exists
    /// only for unit tests that exercise fingerprint-sensitivity without
    /// resolving a real plan; production callers must not use it.
    #[doc(hidden)]
    pub fn preview_fingerprint(&self) -> BuildFingerprint {
        let mut fp = BuildFingerprint::new(self.identity.clone(), (*self.source_snapshot).clone());
        let mut inputs = self.fingerprint_inputs.clone();
        if inputs.target_triple.is_empty() {
            inputs.target_triple = self.target.clone();
        }
        fp.target_triple = inputs.target_triple.clone();
        fp.toolchain_stamp = inputs.toolchain_stamp.clone();
        fp.sdk_content_hash = crate::fingerprint::ContentHash(inputs.sdk_content_hash);
        fp.abi = crate::fingerprint::AbiStamp {
            version: inputs.abi_version,
            interface_prefix_hashes: inputs.interface_prefix_hashes.clone(),
        };
        fp.wrapper_schema = inputs.wrapper_schema;
        fp.wrapper_content_hash = crate::fingerprint::ContentHash::ZERO;
        fp.manifest_schema = inputs.manifest_schema;
        fp.manifest_content_hash = crate::fingerprint::ContentHash::ZERO;
        fp.lock_resolution = crate::fingerprint::ContentHash(inputs.lock_resolution);
        fp.capabilities = inputs.capabilities.clone();
        fp.profile = match inputs.profile {
            BuildProfile::Dist => crate::fingerprint::ProfileTag::Dist,
            BuildProfile::DistLean => crate::fingerprint::ProfileTag::DistLean,
        };
        fp.rustflags = inputs.rustflags.clone();
        fp.panic = match inputs.panic {
            PanicStrategy::Abort => crate::fingerprint::PanicTag::Abort,
            PanicStrategy::Unwind => crate::fingerprint::PanicTag::Unwind,
        };
        fp.compiler_service_schema = inputs.compiler_service_schema;
        fp.crate_type = match self.artifact_kind {
            ArtifactKind::StaticLib => crate::fingerprint::CrateTypeTag::Staticlib,
            ArtifactKind::DynamicLib { .. } => crate::fingerprint::CrateTypeTag::Dylib,
            ArtifactKind::Tier1Plugin | ArtifactKind::Tier1Script => crate::fingerprint::CrateTypeTag::Cdylib,
        };
        fp
    }

    /// Return the request's revision, if known. The `DesiredStore` sets the
    /// authoritative revision; the request itself carries the source snapshot
    /// it was discovered with.
    pub fn request_revision(&self) -> Revision {
        // The request does not own the revision (the scheduler + desired
        // store do); it owns the source snapshot. Tests that want a
        // request with an explicit revision build one via
        // `BuildService::submit_revisioned`.
        Revision(0)
    }
}

/// A successful build's result. The artifact is immutable on disk under
/// `<cache_root>/<id>/gen-<N>/lib<id>.<ext>`.
#[derive(Clone, Debug)]
pub struct BuildResult {
    /// Revision that was compiled.
    pub request_revision: Revision,
    /// The fingerprint of the request that produced this result.
    pub fingerprint: BuildFingerprint,
    /// Absolute path to the immutable artifact.
    pub immutable_artifact_path: PathBuf,
    /// Generation number (monotonic per `CanonicalId`).
    pub generation: PublishedGeneration,
    /// Diagnostics emitted during the build (warnings, notes).
    pub diagnostics: Vec<Diagnostic>,
}

/// Generation counter — monotonic per `CanonicalId`, only on success.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublishedGeneration(pub u64);

/// A single compiler diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Severity.
    pub severity: Severity,
    /// 1-based line number, if known.
    pub line: Option<u32>,
    /// 1-based column number, if known.
    pub column: Option<u32>,
    /// Human-readable message.
    pub message: String,
}

impl Diagnostic {
    /// Construct an error diagnostic.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            line: None,
            column: None,
            message: message.into(),
        }
    }
}

/// Severity of a diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// An error — blocks successful publication.
    Error,
    /// A warning.
    Warning,
    /// A note (informational).
    Note,
}

/// Outcome of a [`BuildService::submit`] future. Every future resolves
/// EXACTLY ONCE with exactly one variant.
#[derive(Clone, Debug)]
pub enum BuildOutcome {
    /// A new generation was published successfully. Caller should call
    /// `BuildService::load_published(id, &fingerprint)`.
    Published {
        /// Revision that was compiled.
        request_revision: Revision,
        /// Fingerprint of the request.
        fingerprint: BuildFingerprint,
        /// Published generation number.
        generation: PublishedGeneration,
        /// R6-5: packages Cargo actually compiled for this build. The
        /// package list is direct, machine-readable evidence of which
        /// crates were rebuilt vs reused.
        compiled_packages: Vec<String>,
    },
    /// A previously-published generation matched the request fingerprint.
    /// Caller should call `BuildService::load_published(id, &fingerprint)`.
    CacheHit {
        /// Revision that was matched.
        request_revision: Revision,
        /// Fingerprint of the request.
        fingerprint: BuildFingerprint,
        /// Generation that matched.
        generation: PublishedGeneration,
        /// R6-5: empty for cache hits (no compilation occurred this
        /// build).
        compiled_packages: Vec<String>,
    },
    /// The request was superseded by a newer revision before it could
    /// start. Caller may re-submit at the newer revision.
    Superseded {
        /// The revision that was superseded.
        superseded_revision: Revision,
        /// The newer revision that won.
        by_revision: Revision,
    },
    /// The cargo invocation failed with a non-zero exit code attributable
    /// to the source. Caller MUST NOT auto-retry the same revision.
    CompileFailed {
        /// Revision that was compiled.
        request_revision: Revision,
        /// Diagnostics from the failed attempt.
        diagnostics: Vec<Diagnostic>,
    },
    /// Cancelled by project close, user retry, or shutdown.
    Cancelled {
        /// Revision that was cancelled.
        request_revision: Revision,
        /// Why.
        by: CancelCause,
    },
    /// Editor shutdown before the request completed.
    Shutdown {
        /// Revision that was in flight.
        request_revision: Revision,
    },
}

/// Cause of cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelCause {
    /// Project was closed.
    ProjectClose,
    /// User clicked "retry" while a build was in flight.
    UserRetry,
    /// Editor is shutting down (use [`BuildOutcome::Shutdown`] in preference).
    Shutdown,
}

/// A loaded, mapped artifact. The handle is the OS-loaded library; the
/// service retains the loader handle inside `MappedSet` until the wrapper is
/// dropped. See [`crate::loader::Loader`] for the real `dlopen` /
/// `LoadLibraryW` implementation.
pub struct LoadedMappedArtifact {
    /// Path the artifact was loaded from.
    pub path: PathBuf,
    /// Generation number.
    pub generation: PublishedGeneration,
}

// Make the inner `Revision` importable via this module too.
// (Revision is defined at the top of this file.)

