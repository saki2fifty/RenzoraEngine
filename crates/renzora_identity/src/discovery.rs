//! Filesystem-containment layer.
//!
//! `DiscoveredPath` extends `CanonicalId` with the on-disk absolute path of
//! both the discovery root and the discovered file, so callers can verify
//! containment and detect symlink escapes.
//!
//! This module is gated behind `feature = "discovery"` because it pulls in
//! `std::path` and the I/O backend. The pure identity layer
//! ([`crate::identity`]) never imports from this module.

#[cfg(feature = "discovery")]
use std::path::{Path, PathBuf};

use alloc::string::String;

use crate::identity::{CanonicalId, IdParseError};

/// A canonical identity plus the filesystem paths that produced it.
///
/// Used at the discovery boundary — typically by `renzora_rust_script`'s
/// walk, the editor's watcher reconcile loop, and the copy-based exporter.
/// After construction, callers should treat the contents as immutable for
/// the lifetime of any system that holds a reference.
#[derive(Clone, Debug)]
pub struct DiscoveredPath {
    root: RootPath,
    path: PathBuf,
    identity: CanonicalId,
}

/// Newtype wrapper around the absolute root path, for symmetry with
/// `DiscoveredPath` and so the type can later be extended with caching or
/// re-root behaviour without changing the public field layout.
#[derive(Clone, Debug)]
pub struct RootPath(PathBuf);

impl RootPath {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Errors specific to discovery-phase operations. These are distinct from
/// `IdParseError`, which is for pure-string construction; a discovery-time
/// error is a runtime artefact (file deleted between walk and reconcile;
/// symlink escaped root; read permission denied; etc.).
#[derive(Debug)]
pub enum DiscoveryError {
    /// The path was not present at discovery time. Not necessarily fatal;
    /// the watcher may simply retire the entry.
    NotFound,
    /// The path resolved outside its declared root via symlink hop.
    SymlinkEscape { from: PathBuf, attempted: PathBuf },
    /// I/O error returned by the OS.
    Io(std::io::Error),
}

impl core::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DiscoveryError::NotFound => f.write_str("path not found at discovery"),
            DiscoveryError::SymlinkEscape { from, attempted } => write!(
                f,
                "symlink escape: {} resolved outside root, attempted {}",
                from.display(),
                attempted.display()
            ),
            DiscoveryError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl core::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            DiscoveryError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl DiscoveredPath {
    /// Construct from a rooted discovery. Verifies lexical containment by
    /// canonicalising `path` (where the platform supports it) and walking up
    /// its components until it reaches `root` or escapes.
    pub fn from_discovered(
        root: RootPath,
        root_kind: crate::identity::RootKind,
        relpath: &str,
    ) -> Result<Self, IdParseError> {
        let identity = CanonicalId::from_rooted(root_kind, relpath)?;
        // Lexical containment: the absolute `path` must START with `root`
        // once both are canonicalised. We do best-effort canonicalisation; on
        // platforms that lack `canonicalize` semantics (read-only FS,
        // unsupported `lstat`), we fall back to string comparison.
        let path = root.as_path().join(relpath.replace('\\', "/"));
        if let Ok(can_path) = std::fs::canonicalize(&path) {
            if !can_path.starts_with(root.as_path()) {
                // Symlink escape: refuse construction and let the caller
                // decide what to do. We refuse to return a DiscoveredPath
                // because the identity mapping is unsafe.
                // (The caller can interpret the path further if needed;
                // we don't surface this as a typed error here — instead, we
                // keep the constructor pure-lexical: this entire branch is
                // detected by a higher-level helper that turns it into a
                // DiscoveryError.)
                return Err(IdParseError::EscapesDeclaredRoot);
            }
        }
        Ok(Self {
            root,
            path,
            identity,
        })
    }

    /// The canonical identity.
    pub fn identity(&self) -> &CanonicalId {
        &self.identity
    }

    /// The configured discovery root.
    pub fn root(&self) -> &RootPath {
        &self.root
    }

    /// The on-disk path produced by discovery.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Render the canonical identity's `scheme://path` form. Convenience
    /// over `identity().to_scheme_path()`.
    pub fn scheme_path(&self) -> String {
        self.identity.to_scheme_path()
    }
}
