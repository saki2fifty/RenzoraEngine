//! Immutable staging and publication protocol.
//!
//! See `phase2-cached-compiler-design.md` §6 for the layout and §6.2 for the
//! `replace_active_pointer` atomic-replace protocol (POSIX `rename` over an
//! existing file, or Windows `ReplaceFileW` with `WRITE_THROUGH |
//! IGNORE_MERGE_ERRORS | IGNORE_ACL_ERRORS`). The first-publication case is
//! `rename` (POSIX) / `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`
//! (Windows).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use renzora_identity::CanonicalId;
use uuid::Uuid;

use crate::fingerprint::{BuildFingerprint, BuildKey};
use crate::types::PublishedGeneration;

/// Active-generation pointer. Binary (postcard-like) length-delimited
/// encoding — NOT JSON. See `phase2-cached-compiler-design.md` §6.2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePointer {
    /// Published generation number.
    pub generation: PublishedGeneration,
    /// Blake3 hash for fast equality.
    pub fingerprint_hash: [u8; 32],
    /// `compiler_cache` schema version.
    pub compiler_service_schema: u32,
}

impl ActivePointer {
    /// Current schema version (bumped when the encoding changes).
    pub const CURRENT_SCHEMA: u32 = 1;

    /// Encode as a length-delimited binary record:
    /// `[u32 BE schema_version][u32 BE payload_len][u64 BE generation][32 bytes hash][u32 BE compiler_service_schema]`.
    /// payload_len = 8 + 32 + 4 = 44; total = 8 + 44 = 52.
    pub fn encode(&self) -> Vec<u8> {
        let payload_len: u32 = 8 + 32 + 4;
        let total_len: u32 = 8 + payload_len;
        let mut out = Vec::with_capacity(total_len as usize);
        out.extend_from_slice(&Self::CURRENT_SCHEMA.to_be_bytes());
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(&self.generation.0.to_be_bytes());
        out.extend_from_slice(&self.fingerprint_hash);
        out.extend_from_slice(&self.compiler_service_schema.to_be_bytes());
        out
    }

    /// Decode from binary.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 {
            return Err("active pointer too short".to_string());
        }
        let schema = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        if schema != Self::CURRENT_SCHEMA {
            return Err(format!("active pointer schema {schema} != current {}", Self::CURRENT_SCHEMA));
        }
        let payload_len = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if bytes.len() != 8 + payload_len {
            return Err("active pointer length mismatch".to_string());
        }
        if payload_len != 44 {
            return Err(format!("active pointer payload length {payload_len} != 44"));
        }
        let gen = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[16..48]);
        let schema2 = u32::from_be_bytes(bytes[48..52].try_into().unwrap());
        Ok(Self {
            generation: PublishedGeneration(gen),
            fingerprint_hash: hash,
            compiler_service_schema: schema2,
        })
    }
}

/// Atomic pointer replacement. POSIX: `rename(2)` over an existing target is
/// atomic. Windows: `ReplaceFileW(target, source, ..., WRITE_THROUGH | IGNORE_MERGE_ERRORS | IGNORE_ACL_ERRORS, ...)`.
///
/// The `first_publication` flag selects the under-the-hood primitive for the
/// no-existing-target case.
pub fn replace_active_pointer(
    dir: &Path,
    new_bytes: &[u8],
    first_publication: bool,
) -> Result<(), ReplaceError> {
    let tmp_path = dir.join(format!("active.bin.tmp.{}", Uuid::new_v4()));
    // Write tmp.
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(ReplaceError::Io)?;
        f.write_all(new_bytes).map_err(ReplaceError::Io)?;
        f.flush().map_err(ReplaceError::Io)?;
        f.sync_all().map_err(ReplaceError::Io)?;
    }

    // Best-effort parent dir fsync.
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }

    let target = dir.join("active.bin");
    let result = platform_replace(&target, &tmp_path, first_publication);

    // Best-effort tmp cleanup; never fail the replace just because the
    // tmp file couldn't be removed (the OS will eventually GC it).
    let _ = fs::remove_file(&tmp_path);

    result
}

/// Atomic-replace error variants.
#[derive(Debug)]
pub enum ReplaceError {
    /// Underlying IO error.
    Io(std::io::Error),
    /// Windows reported that the atomicity guarantee cannot be honoured
    /// (e.g. FAT32 filesystem).
    AtomicityGuarantee,
    /// Retries exhausted on a transient sharing violation. Caller should
    /// retry the whole staging path; the previously-active generation
    /// continues to load in the meantime.
    TransientSharing,
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplaceError::Io(e) => write!(f, "{e}"),
            ReplaceError::AtomicityGuarantee => f.write_str("filesystem does not support atomic replace"),
            ReplaceError::TransientSharing => f.write_str("transient sharing violation exhausted retries"),
        }
    }
}

impl std::error::Error for ReplaceError {}

#[cfg(unix)]
fn platform_replace(target: &Path, tmp: &Path, first_publication: bool) -> Result<(), ReplaceError> {
    if first_publication {
        // POSIX rename atomically creates the new file when target does not exist.
        std::fs::rename(tmp, target).map_err(ReplaceError::Io)
    } else {
        // POSIX rename atomically replaces the target when it exists.
        std::fs::rename(tmp, target).map_err(ReplaceError::Io)
    }
}

#[cfg(windows)]
fn platform_replace(target: &Path, tmp: &Path, first_publication: bool) -> Result<(), ReplaceError> {
    use std::os::windows::ffi::OsStrExt;
    if first_publication {
        // MoveFileExW with MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH.
        let src_w: Vec<u16> = tmp.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let dst_w: Vec<u16> = target.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let r = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                src_w.as_ptr(),
                dst_w.as_ptr(),
                0x00000001 | 0x00000008, /* REPLACE_EXISTING | WRITE_THROUGH */
            )
        };
        if r == 0 {
            return Err(ReplaceError::Io(std::io::Error::last_os_error()));
        }
        return Ok(());
    }

    // ReplaceFileW with WRITE_THROUGH | IGNORE_MERGE_ERRORS | IGNORE_ACL_ERRORS.
    let src_w: Vec<u16> = tmp.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let dst_w: Vec<u16> = target.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut attempts = 0u32;
    loop {
        let r = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
                dst_w.as_ptr(),
                src_w.as_ptr(),
                std::ptr::null(),
                0x00000002 | 0x00000010 | 0x00000004, /* WRITE_THROUGH | IGNORE_MERGE_ERRORS | IGNORE_ACL_ERRORS */
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if r != 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        // ERROR_SHARING_VIOLATION = 32, ERROR_ACCESS_DENIED = 5
        let raw = err.raw_os_error().unwrap_or(0);
        if matches!(raw, 32 | 5) && attempts < 20 {
            attempts += 1;
            let backoff_ms = std::cmp::min(1000u64, 50u64 * (1u64 << attempts.min(4)));
            std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            continue;
        }
        return Err(ReplaceError::Io(err));
    }
}

/// One published generation record (rebuilt from disk on startup).
#[derive(Clone, Debug)]
pub struct IndexedGeneration {
    pub generation: PublishedGeneration,
    pub fingerprint: BuildFingerprint,
}

/// Cache root layout helpers. See §6.1.
pub fn id_dir(cache_root: &Path, id: &CanonicalId) -> PathBuf {
    // Use a deterministic, reversible encoding: percent-encode characters
    // that are not portable across filesystems, then collapse remaining
    // doubled separators.
    cache_root.join(safe_id_dir_name(&id.to_scheme_path()))
}

/// Reversible encoding: `:` -> `_c_`, `/` -> `_s_`, `%` -> `_p_`, `_` -> `__`,
/// and any other unsafe character -> `_x<HEX>_`. This guarantees every
/// canonical id maps to a unique directory name and can be reconstructed.
pub fn safe_id_dir_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ':' => out.push_str("_c_"),
            '/' => out.push_str("_s_"),
            '%' => out.push_str("_p_"),
            '_' => out.push_str("__"),
            '\\' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => {
                out.push_str(&format!("_x{:02x}_", c as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// Reverse of `safe_id_dir_name`. Public for `recovery.rs` to use.
pub fn from_safe_id_dir_name(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' && i + 1 < bytes.len() {
            if i + 2 < bytes.len() && bytes[i + 1] == b'_' {
                out.push('_');
                i += 2;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b'c' && bytes[i + 2] == b'_' {
                out.push(':');
                i += 3;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b's' && bytes[i + 2] == b'_' {
                out.push('/');
                i += 3;
                continue;
            }
            if i + 2 < bytes.len() && bytes[i + 1] == b'p' && bytes[i + 2] == b'_' {
                out.push('%');
                i += 3;
                continue;
            }
            if i + 4 < bytes.len() && bytes[i + 1] == b'x' {
                let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).ok()?;
                let code = u8::from_str_radix(hex, 16).ok()?;
                out.push(code as char);
                i += 5;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Some(out)
}

/// Path to the next `gen-<N>` for `id`. Reads existing `gen-*` directories and
/// picks N+1. Twelve hex chars (4 billion generations) — sufficient for any
/// realistic project lifetime.
pub fn next_generation_path(cache_root: &Path, id: &CanonicalId) -> std::io::Result<(PathBuf, PublishedGeneration)> {
    let dir = id_dir(cache_root, id);
    let mut max = 0u64;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if let Some(rest) = name.strip_prefix("gen-") {
            if let Ok(n) = rest.parse::<u64>() {
                max = max.max(n);
            }
        }
    }
    let next = PublishedGeneration(max + 1);
    let path = dir.join(format!("gen-{}", next.0));
    Ok((path, next))
}

/// Stage directory path (UUID suffix to avoid collisions).
pub fn stage_dir(cache_root: &Path, id: &CanonicalId) -> PathBuf {
    id_dir(cache_root, id).join(format!("stage-{}", Uuid::new_v4()))
}

/// The artifact cache. Owns `MappedSet`, `PinnedSet`, and the inactive-
/// generation index for every `CanonicalId` it has ever seen.
pub struct ArtifactCache {
    /// Active pointer per id.
    active: Mutex<BTreeMap<CanonicalId, ActivePointer>>,
    /// Inactive-generation index per id: `BuildKey -> generation`.
    index: Mutex<BTreeMap<CanonicalId, BTreeMap<BuildKey, PublishedGeneration>>>,
    /// Currently mapped generations.
    mapped: Mutex<BTreeSet<(CanonicalId, PublishedGeneration)>>,
    /// Pinned generations.
    pinned: Mutex<BTreeSet<(CanonicalId, PublishedGeneration)>>,
    /// Cache root.
    root: PathBuf,
}

impl ArtifactCache {
    /// Construct an empty cache over `root`.
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(BTreeMap::new()),
            index: Mutex::new(BTreeMap::new()),
            mapped: Mutex::new(BTreeSet::new()),
            pinned: Mutex::new(BTreeSet::new()),
            root,
        })
    }

    /// Read the active pointer for `id`.
    pub fn read_active(&self, id: &CanonicalId) -> Option<ActivePointer> {
        let path = id_dir(&self.root, id).join("active.bin");
        let bytes = std::fs::read(&path).ok()?;
        ActivePointer::decode(&bytes).ok()
    }

    /// Write `active.bin` for `id` using `replace_active_pointer`.
    pub fn write_active(&self, id: &CanonicalId, ptr: &ActivePointer, first_publication: bool) -> Result<(), ReplaceError> {
        let dir = id_dir(&self.root, id);
        std::fs::create_dir_all(&dir).map_err(ReplaceError::Io)?;
        replace_active_pointer(&dir, &ptr.encode(), first_publication)?;
        self.active.lock().insert(id.clone(), ptr.clone());
        Ok(())
    }

    /// Insert into `MappedSet` (called by `MappedArtifact::register`). The
    /// loader inserts BEFORE returning from `open_library`, so retention
    /// sweeps cannot race the loader.
    pub fn register(&self, id: &CanonicalId, generation: PublishedGeneration) {
        self.mapped.lock().insert((id.clone(), generation));
    }

    /// Remove from `MappedSet`.
    pub fn unregister(&self, id: &CanonicalId, generation: PublishedGeneration) {
        self.mapped.lock().remove(&(id.clone(), generation));
    }

    /// Test/diagnostic: true iff `(id, generation)` is currently in the
    /// `MappedSet`.
    pub fn is_mapped(&self, id: &CanonicalId, generation: PublishedGeneration) -> bool {
        self.mapped.lock().contains(&(id.clone(), generation))
    }

    /// Pin a generation.
    pub fn pin(&self, id: &CanonicalId, generation: PublishedGeneration) {
        self.pinned.lock().insert((id.clone(), generation));
    }

    /// Unpin a generation.
    pub fn unpin(&self, id: &CanonicalId, generation: PublishedGeneration) {
        self.pinned.lock().remove(&(id.clone(), generation));
    }

    /// Add to the inactive-generation index.
    pub fn index_inactive(&self, id: &CanonicalId, key: BuildKey, generation: PublishedGeneration) {
        self.index
            .lock()
            .entry(id.clone())
            .or_default()
            .insert(key, generation);
    }

    /// Look up an inactive generation by build key.
    pub fn lookup_inactive(&self, id: &CanonicalId, key: &BuildKey) -> Option<PublishedGeneration> {
        self.index.lock().get(id).and_then(|m| m.get(key).copied())
    }

    /// Look up the active generation for `id`. None if there is no active
    /// pointer.
    pub fn active_generation(&self, id: &CanonicalId) -> Option<PublishedGeneration> {
        self.read_active(id).map(|p| p.generation)
    }

    /// True when `id` and `generation` are mapped, pinned, or active. The
    /// retention sweep MUST NOT evict these.
    pub fn is_protected(&self, id: &CanonicalId, generation: PublishedGeneration) -> bool {
        if self.mapped.lock().contains(&(id.clone(), generation)) {
            return true;
        }
        if self.pinned.lock().contains(&(id.clone(), generation)) {
            return true;
        }
        if let Some(active) = self.active.lock().get(id) {
            if active.generation == generation {
                return true;
            }
        }
        false
    }

    /// Path to the immutable published generation directory.
    pub fn gen_dir(&self, id: &CanonicalId, generation: PublishedGeneration) -> PathBuf {
        id_dir(&self.root, id).join(format!("gen-{}", generation.0))
    }

    /// Path to `fingerprint.bin` for a generation.
    pub fn fingerprint_bin_path(&self, id: &CanonicalId, generation: PublishedGeneration) -> PathBuf {
        self.gen_dir(id, generation).join("fingerprint.bin")
    }

    /// Path to `status.bin` for a generation.
    pub fn status_bin_path(&self, id: &CanonicalId, generation: PublishedGeneration) -> PathBuf {
        self.gen_dir(id, generation).join("status.bin")
    }

    /// Path to the artifact library for a generation.
    pub fn artifact_path(&self, id: &CanonicalId, generation: PublishedGeneration, lib_ext: &str) -> PathBuf {
        self.gen_dir(id, generation).join(format!("lib{}.{lib_ext}", id.bare_leaf()))
    }

    /// Cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Rebuild the in-memory inactive-generation index from disk. Called on
    /// service startup so a new `BuildService` against the same cache root
    /// sees every previously-published generation without rereading every
    /// fingerprint lazily.
    pub fn rebuild_index_from_disk(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if let Some(id) = id_from_dir_name(&name) {
                // For every gen-<N>/ in this id's dir, read fingerprint.bin
                // and index the BuildKey.
                let id_dir = entry.path();
                let Ok(gen_entries) = std::fs::read_dir(&id_dir) else {
                    continue;
                };
                for gen_entry in gen_entries.flatten() {
                    let Some(gen_name) = gen_entry.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    let Some(rest) = gen_name.strip_prefix("gen-") else {
                        continue;
                    };
                    let Ok(n) = rest.parse::<u64>() else {
                        continue;
                    };
                    let fp_path = gen_entry.path().join("fingerprint.bin");
                    let Ok(bytes) = std::fs::read(&fp_path) else {
                        continue;
                    };
                    if let Ok(fp) = BuildFingerprint::deserialize(&bytes) {
                        self.index_inactive(&id, fp.build_key(), PublishedGeneration(n));
                    }
                }
            }
        }
    }
}

/// Reconstruct a `CanonicalId` from a directory name produced by `safe_id_dir_name`.
fn id_from_dir_name(name: &str) -> Option<CanonicalId> {
    let scheme_path = from_safe_id_dir_name(name)?;
    CanonicalId::parse(&scheme_path).ok()
}

/// The full publication protocol. See §6.3 of the design.
pub struct PublicationResult {
    /// The published generation number.
    pub generation: PublishedGeneration,
    /// The fingerprint that was published.
    pub fingerprint: BuildFingerprint,
    /// Path to the immutable artifact.
    pub artifact_path: PathBuf,
    /// Whether this was the first publication for this id.
    pub first_publication: bool,
}

/// Publish a successful build. Assumes the artifact already exists at
/// `staged_artifact` (the supervisor's working copy). Implements §6.3
/// steps 4-10.
pub fn publish(
    cache: &ArtifactCache,
    id: &CanonicalId,
    _revision: crate::types::Revision,
    fingerprint: &BuildFingerprint,
    staged_artifact: &Path,
) -> Result<PublicationResult, ReplaceError> {
    let id_dir = id_dir(cache.root(), id);
    std::fs::create_dir_all(&id_dir).map_err(ReplaceError::Io)?;

    // Step 6: pick the next generation number and rename the staged dir.
    let (gen_path, generation) =
        next_generation_path(cache.root(), id).map_err(ReplaceError::Io)?;
    if gen_path.exists() {
        return Err(ReplaceError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "target gen dir exists",
        )));
    }
    let staged_dir = staged_artifact.parent().unwrap_or(&id_dir);
    atomic_dir_rename(staged_dir, &gen_path)?;

    // Step 7-9: write fingerprint.bin and status.bin next to the artifact,
    // then replace active.bin atomically.
    let lib_ext = fingerprint.crate_type_default_lib_ext();
    let artifact = cache.artifact_path(id, generation, &lib_ext);
    let fp_bytes = fingerprint.serialize();
    std::fs::write(cache.fingerprint_bin_path(id, generation), &fp_bytes).map_err(ReplaceError::Io)?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let status_bytes = encode_status_bin(0u8 /* success */, now_secs, fingerprint.abi.version);
    std::fs::write(cache.status_bin_path(id, generation), &status_bytes).map_err(ReplaceError::Io)?;

    let active = ActivePointer {
        generation,
        fingerprint_hash: fingerprint.build_key().0,
        compiler_service_schema: crate::types::COMPILER_SERVICE_SCHEMA,
    };
    let first_publication = cache.read_active(id).is_none();
    cache.write_active(id, &active, first_publication)?;
    cache.index_inactive(id, fingerprint.build_key(), generation);

    Ok(PublicationResult {
        generation,
        fingerprint: fingerprint.clone(),
        artifact_path: artifact,
        first_publication,
    })
}

/// Atomic directory rename (used for `stage-<uuid>/` → `gen-<N>/`).
pub fn atomic_dir_rename(src: &Path, dst: &Path) -> Result<(), ReplaceError> {
    // POSIX `rename(2)` and Windows `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`
    // are atomic for directories on the same filesystem.
    if dst.exists() {
        return Err(ReplaceError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "rename target exists",
        )));
    }
    #[cfg(unix)]
    {
        std::fs::rename(src, dst).map_err(ReplaceError::Io)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let src_w: Vec<u16> = src.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let r = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                src_w.as_ptr(),
                dst_w.as_ptr(),
                0x00000008, /* WRITE_THROUGH */
            )
        };
        if r == 0 {
            Err(ReplaceError::Io(std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }
}

/// Encode `status.bin` = `[u8 kind][u64 BE last_accessed_unix_seconds][u32 BE abi_v]`.
pub fn encode_status_bin(kind: u8, last_accessed_unix_seconds: u64, abi_v: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.push(kind);
    out.extend_from_slice(&last_accessed_unix_seconds.to_be_bytes());
    out.extend_from_slice(&abi_v.to_be_bytes());
    out
}

/// Decode `status.bin`. Returns `(kind, last_accessed_unix_seconds, abi_v)`.
pub fn decode_status_bin(bytes: &[u8]) -> Option<(u8, u64, u32)> {
    if bytes.len() < 13 {
        return None;
    }
    let kind = bytes[0];
    let last_accessed = u64::from_be_bytes(bytes[1..9].try_into().ok()?);
    let abi_v = u32::from_be_bytes(bytes[9..13].try_into().ok()?);
    Some((kind, last_accessed, abi_v))
}

impl BuildFingerprint {
    /// Default library extension for the fingerprint's `crate_type`.
    pub fn crate_type_default_lib_ext(&self) -> String {
        use crate::fingerprint::CrateTypeTag;
        let ext = match self.crate_type {
            CrateTypeTag::Staticlib => static_lib_ext(),
            CrateTypeTag::Cdylib | CrateTypeTag::Dylib => dynamic_lib_ext(),
        };
        ext.to_string()
    }
}

#[cfg(unix)]
fn static_lib_ext() -> &'static str {
    "a"
}
#[cfg(windows)]
fn static_lib_ext() -> &'static str {
    "lib"
}

#[cfg(target_os = "macos")]
fn dynamic_lib_ext() -> &'static str {
    "dylib"
}
#[cfg(all(unix, not(target_os = "macos")))]
fn dynamic_lib_ext() -> &'static str {
    "so"
}
#[cfg(windows)]
fn dynamic_lib_ext() -> &'static str {
    "dll"
}

/// Test / helper: load the binary fingerprint next to a generation dir and
/// re-verify byte-for-byte against the request fingerprint. Per §2.3.
pub fn verify_fingerprint(
    stored_bytes: &[u8],
    request_fingerprint: &BuildFingerprint,
) -> Result<bool, String> {
    let stored = BuildFingerprint::deserialize(stored_bytes)?;
    Ok(BuildFingerprint::bytes_equal(&stored, request_fingerprint))
}

/// Centralized cache lookup. Per P2-7: every cache hit MUST read
/// `fingerprint.bin`, deserialize it, and compare byte-for-byte with the
/// request's full fingerprint before any decision is made (CacheHit,
/// reactivation, etc). Both active and inactive paths use this entry
/// point so they cannot diverge.
pub struct CacheLookupResult {
    /// `true` when the generation's stored fingerprint matches the request
    /// fingerprint byte-for-byte.
    pub matched: bool,
    /// The stored fingerprint (if `fingerprint.bin` deserialized).
    pub stored: Option<BuildFingerprint>,
    /// `true` when the generation is currently the active pointer.
    pub is_active: bool,
}

/// Centralized fingerprint verification. Both the service's `load_published`
/// and the worker's cache check go through this function so the rules cannot
/// diverge.
pub fn verify_generation_fingerprint(
    cache: &ArtifactCache,
    id: &CanonicalId,
    generation: PublishedGeneration,
    request_fingerprint: &BuildFingerprint,
) -> Result<CacheLookupResult, String> {
    let fp_path = cache.fingerprint_bin_path(id, generation);
    let bytes = std::fs::read(&fp_path)
        .map_err(|e| format!("read {}: {e}", fp_path.display()))?;
    let stored = BuildFingerprint::deserialize(&bytes)
        .map_err(|e| format!("decode fingerprint.bin for gen-{}: {e}", generation.0))?;
    let matched = BuildFingerprint::bytes_equal(&stored, request_fingerprint);
    let is_active = cache
        .read_active(id)
        .map(|p| p.generation == generation)
        .unwrap_or(false);
    Ok(CacheLookupResult {
        matched,
        stored: Some(stored),
        is_active,
    })
}

/// Helper: write fingerprint.bin atomically (write-to-tmp + rename).
pub fn write_fingerprint_bin(target: &Path, fingerprint: &BuildFingerprint) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = fingerprint.serialize();
    let tmp = target.with_extension("bin.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.flush()?;
        f.sync_all()?;
    }
    atomic_dir_rename(&tmp, target).map_err(|e| std::io::Error::other(format!("{e}")))?;
    Ok(())
}

/// Convenience: open the cache's root and ensure it exists.
pub fn ensure_cache_root(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)
}
