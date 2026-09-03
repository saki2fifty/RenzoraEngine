//! Stable staging of a successfully published loose-plugin generation into a
//! flat, loader-visible directory.
//!
//! Phase 3 places stable files directly in a dedicated flat directory using
//! collision-proof canonical-id-derived names (see design §"Staging and stable
//! loader identity"). The chosen directory is the one the existing
//! `renzora_plugin::host::loader::load_dir` extension scan already knows how
//! to read; the loose-plugin host integration calls a new
//! `load_one_from_stable_path` entry point with the per-identity stable path
//! rather than going through `load_dir`'s stem-based identity.
//!
//! Atomicity on every supported platform:
//!
//! - POSIX: `rename(2)` overwrites the destination atomically when both
//!   source and destination are on the same filesystem.
//! - Windows: `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` performs the
//!   same atomic overwrite semantics, and is the only way to replace a file
//!   that the loader has not opened yet. (`MoveFileExW` requires the target
//!   to NOT be open; the staged file is always unmapped at the moment we
//!   swap — the host's transactional activation shadow-copies on Windows
//!   before opening, so the staged file is never mapped.)

use std::path::{Path, PathBuf};
use renzora_identity::CanonicalId;

/// Where loose plugins stage their stable, loader-visible cdylib.
#[derive(Debug, Clone)]
pub struct StableStaging {
    root: PathBuf,
}

impl StableStaging {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure the staging directory exists. Idempotent; safe to call once
    /// at install time.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }

    /// Collision-proof flat filename for `id` using the current platform's
    /// dynamic-library extension. The name encodes the scheme and the path
    /// the identity carries, with `:` and `/` replaced so the result is a
    /// valid filename on every OS. The `.` separator in the leaf is kept.
    ///
    /// Example: `engine://spin.rs` becomes `engine___spin.rs.dll` on Windows,
    /// `engine___spin.rs.so` on Linux, `engine___spin.rs.dylib` on macOS
    /// (the `:` and both `/` of the `://` separator each become `_`).
    pub fn stable_name_for(&self, id: &CanonicalId) -> String {
        let ext = std::env::consts::DLL_EXTENSION;
        let safe = id.to_scheme_path().replace([':', '/'], "_");
        format!("{safe}.{ext}")
    }

    /// Collision-proof directory-safe name for `id`. The full canonical
    /// id (e.g. `engine://spin.rs`) contains `:` and `/`, which are
    /// invalid Windows directory name characters and would split the id
    /// into unintended subdirectories on every OS. Replaces both with
    /// `_`, leaving the scheme prefix readable in a folder listing.
    ///
    /// Used by the export pipeline to compute `<output>/plugins/<safe>/`
    /// without leaking the colon-and-slash delimiter into the path.
    /// Same encoding as `stable_name_for`'s body, minus the extension.
    pub fn safe_dir_name_for(&self, id: &CanonicalId) -> String {
        id.to_scheme_path().replace([':', '/'], "_")
    }

    pub fn stable_path_for(&self, id: &CanonicalId) -> PathBuf {
        self.root.join(self.stable_name_for(id))
    }

    /// Atomically replace the stable staged library for `id` with the
    /// contents of `immutable_source`.
    ///
    /// `immutable_source` is the path Phase 2 published — an immutable
    /// generation under `<cache_root>/<id>/gen-<N>/...`. The destination is
    /// the flat stable file under `self.root`. On success, the destination
    /// contains exactly the bytes of the source, and any prior generation
    /// the OS could have left at the destination has been overwritten
    /// atomically.
    ///
    /// Returns the previous stable path (if any) and the new stable path.
    /// The previous path is informational — the caller typically does not
    /// need to do anything with it. A failed swap leaves the previous
    /// stable file untouched, which is what "last-good preserved on
    /// compile failure" requires.
    pub fn place(
        &self,
        id: &CanonicalId,
        immutable_source: &Path,
    ) -> Result<StableStagingPlacement, StableStagingError> {
        if !immutable_source.is_file() {
            return Err(StableStagingError::SourceMissing(
                immutable_source.to_path_buf(),
            ));
        }
        let dst = self.stable_path_for(id);
        let previous = if dst.exists() { Some(dst.clone()) } else { None };

        // Stage next to the destination so the atomic-replace is on the
        // same filesystem (POSIX rename) or so MoveFileExW can target the
        // exact destination.
        let tmp = self.root.join(format!(
            ".swap-{}.{}",
            uuid::Uuid::new_v4(),
            std::env::consts::DLL_EXTENSION
        ));
        std::fs::copy(immutable_source, &tmp).map_err(StableStagingError::CopyFailed)?;
        atomic_replace(&tmp, &dst).map_err(StableStagingError::AtomicReplaceFailed)?;
        // The swap tmp is gone on success (rename consumes it on POSIX;
        // MoveFileExW consumes it on Windows). Best-effort cleanup if it
        // somehow survived.
        let _ = std::fs::remove_file(&tmp);

        Ok(StableStagingPlacement {
            stable_path: dst,
            previous_path: previous,
        })
    }
}

/// Where one successful staging landed.
#[derive(Debug, Clone)]
pub struct StableStagingPlacement {
    pub stable_path: PathBuf,
    pub previous_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum StableStagingError {
    SourceMissing(PathBuf),
    CopyFailed(std::io::Error),
    AtomicReplaceFailed(std::io::Error),
}

impl std::fmt::Display for StableStagingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StableStagingError::SourceMissing(p) => {
                write!(f, "staging source missing: {}", p.display())
            }
            StableStagingError::CopyFailed(e) => write!(f, "staging copy failed: {e}"),
            StableStagingError::AtomicReplaceFailed(e) => {
                write!(f, "staging atomic replace failed: {e}")
            }
        }
    }
}

impl std::error::Error for StableStagingError {}

#[cfg(unix)]
fn atomic_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::rename(src, dst)
}

#[cfg(windows)]
fn atomic_replace(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x00000001;
    let src_w: Vec<u16> = src.as_os_str().encode_wide().chain([0]).collect();
    let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: both buffers are null-terminated wide strings; MoveFileExW
    // reads them up to the null terminator.
    let ok = unsafe { MoveFileExW(src_w.as_ptr(), dst_w.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> CanonicalId {
        CanonicalId::parse(s).expect("valid id")
    }

    #[test]
    fn stable_name_is_collision_proof_and_platform_specific() {
        let staging = StableStaging::new(std::env::temp_dir());
        let n = staging.stable_name_for(&id("engine://spin.rs"));
        assert!(n.starts_with("engine___spin.rs."));
        assert!(n.ends_with(std::env::consts::DLL_EXTENSION));
    }

    #[test]
    fn place_copies_bytes_to_stable_path() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StableStaging::new(dir.path().to_path_buf());
        staging.ensure().unwrap();
        let src = dir.path().join("gen-N.bin");
        std::fs::write(&src, b"hello").unwrap();
        let p = staging.place(&id("engine://spin.rs"), &src).unwrap();
        assert_eq!(
            std::fs::read(&p.stable_path).unwrap(),
            b"hello".to_vec(),
        );
    }

    #[test]
    fn place_overwrites_existing_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StableStaging::new(dir.path().to_path_buf());
        staging.ensure().unwrap();
        let id = id("engine://spin.rs");
        let src1 = dir.path().join("g1.bin");
        let src2 = dir.path().join("g2.bin");
        std::fs::write(&src1, b"one").unwrap();
        std::fs::write(&src2, b"two").unwrap();
        staging.place(&id, &src1).unwrap();
        let p = staging.place(&id, &src2).unwrap();
        assert_eq!(std::fs::read(&p.stable_path).unwrap(), b"two".to_vec());
        assert!(p.previous_path.is_some());
    }

    #[test]
    fn place_missing_source_returns_error_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StableStaging::new(dir.path().to_path_buf());
        staging.ensure().unwrap();
        let id = id("engine://spin.rs");
        let src1 = dir.path().join("g1.bin");
        std::fs::write(&src1, b"keep-me").unwrap();
        staging.place(&id, &src1).unwrap();
        let bogus = dir.path().join("does-not-exist");
        let err = staging.place(&id, &bogus).unwrap_err();
        assert!(matches!(err, StableStagingError::SourceMissing(_)));
        // Stable file untouched.
        assert_eq!(
            std::fs::read(staging.stable_path_for(&id)).unwrap(),
            b"keep-me".to_vec(),
        );
    }
}
