//! Tier-1 library loader. Real `dlopen` / `LoadLibraryW` via libloading.
//!
//! See `phase2-cached-compiler-design.md` §6.6 / §10.2. The loader:
//! 1. Centralized fingerprint byte-for-byte verification (§2.3 / P2-7).
//! 2. Reserves the generation against retention BEFORE `open_library` so
//!    the retention sweep cannot race the loader.
//! 3. Rolls back the reservation if `open_library` fails.
//! 4. Owns the real `Library` handle in RAII (unregister + unload on drop).
//! 5. Never reports a mapping that did not occur.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use parking_lot::Mutex;
use renzora_identity::CanonicalId;

use crate::fingerprint::BuildFingerprint;
use crate::staging::{verify_generation_fingerprint, ArtifactCache};
use crate::types::{PublishedGeneration, Revision};

/// Errors from [`Loader::load`].
#[derive(Debug)]
pub enum LoadError {
    /// No active pointer for the id.
    NoActive,
    /// Stored `fingerprint.bin` could not be read or decoded.
    FingerprintCorrupt(String),
    /// The stored fingerprint does not match the request fingerprint.
    FingerprintMismatch,
    /// Neither active nor indexed inactive generation matches.
    Miss,
    /// `Library::new` failed.
    OpenLibraryFailed(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NoActive => write!(f, "no active pointer"),
            LoadError::FingerprintCorrupt(s) => write!(f, "fingerprint corrupt: {s}"),
            LoadError::FingerprintMismatch => write!(f, "fingerprint mismatch"),
            LoadError::Miss => write!(f, "no matching generation"),
            LoadError::OpenLibraryFailed(s) => write!(f, "open library failed: {s}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// A successfully-loaded library. The handle is owned; dropping it
/// unregisters from `MappedSet` and unloads the library (via RAII).
pub struct LoadedLibrary {
    pub(crate) identity: CanonicalId,
    pub(crate) generation: PublishedGeneration,
    pub(crate) path: PathBuf,
    pub(crate) library: Option<Library>,
    pub(crate) cache: Arc<ArtifactCache>,
}

impl LoadedLibrary {
    pub fn identity(&self) -> &CanonicalId {
        &self.identity
    }
    pub fn generation(&self) -> PublishedGeneration {
        self.generation
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up a symbol by raw name (any `&[u8]`). Returns the raw pointer
    /// to the symbol. The caller is responsible for casting it to the
    /// expected `fn` signature and for upholding the symbol's contract.
    ///
    /// # Safety
    /// The caller must ensure `name` matches the symbol actually exported
    /// by the library, and the resulting pointer must be cast to a `fn`
    /// signature whose ABI matches the symbol's. The pointer is valid
    /// for as long as the `LoadedLibrary` is kept alive.
    pub unsafe fn symbol(&self, name: &[u8]) -> Result<*mut std::ffi::c_void, String> {
        let lib = self.library.as_ref().ok_or("library already consumed")?;
        // SAFETY: `Library::get` returns a `Symbol<T>` whose pointer is
        // valid for the library's lifetime. We coerce to `*mut c_void`.
        let sym = unsafe { lib.get::<*mut std::ffi::c_void>(name) }
            .map_err(|e| e.to_string())?;
        // The `Symbol<T>` is a `Drop` type on Linux (calls `dlclose`).
        // We do NOT want to drop it; the `LoadedLibrary` owns the
        // library. We `ManuallyDrop` the wrapper to avoid the drop call
        // and read the pointer through `Deref`.
        let p: *mut std::ffi::c_void = *sym;
        #[allow(clippy::forget_non_drop)]
        std::mem::forget(sym);
        Ok(p)
    }

    /// Borrow the inner `libloading::Library` without consuming it. The
    /// mapping registration remains active as long as `self` is alive;
    /// the returned reference is valid for that same lifetime. Phase 4
    /// will use this to hand function pointers to `LoadedScripts::insert`
    /// while keeping the mapping guard on the cache.
    pub fn library(&self) -> &libloading::Library {
        self.library.as_ref().expect("library already consumed")
    }
}

impl Drop for LoadedLibrary {
    fn drop(&mut self) {
        // Unregister from MappedSet BEFORE closing the library so a
        // concurrent retention sweep cannot observe a closed handle.
        self.cache.unregister(&self.identity, self.generation);
        // Drop the Library: libloading's Drop runs `dlclose` /
        // `FreeLibrary` which runs static destructors inside the
        // image. Per Phase 1 documented behaviour, the engine tolerates
        // leaks here — but we still attempt the close.
        drop(self.library.take());
    }
}

/// The cache's Tier-1 library loader. Stateless beyond `Arc<ArtifactCache>`.
pub struct Loader {
    cache: Arc<ArtifactCache>,
    /// Symbol name lookup registry. The Bevy adapter registers the
    /// project-defined symbols (`renzora_script_update` etc.) at startup
    /// so the loader can validate that the library exports at least one
    /// known entry point. The loader does NOT dereference the symbol;
    /// the Bevy adapter does.
    required_symbols: Mutex<Vec<Vec<u8>>>,
}

impl Loader {
    pub fn new(cache: Arc<ArtifactCache>) -> Self {
        Self {
            cache,
            required_symbols: Mutex::new(Vec::new()),
        }
    }

    /// Register a C-string symbol name that every loaded library MUST
    /// export. The loader verifies presence at `load` time but does not
    /// call the symbol.
    pub fn require_symbol(&self, name: &[u8]) {
        self.required_symbols.lock().push(name.to_vec());
    }

    /// Real library load. Steps:
    /// 1. Read `active.bin` for the id.
    /// 2. Verify active generation's stored fingerprint byte-for-byte
    ///    against `request_fingerprint`. On match, proceed.
    /// 3. Else consult `index.bin` for an indexed inactive generation.
    ///    For each candidate, verify byte-for-byte.
    /// 4. On match, reactivate `active.bin`.
    /// 5. Register in `MappedSet` BEFORE `Library::new`.
    /// 6. `Library::new`. On failure, unregister and return error.
    /// 7. Verify the library exports every required symbol.
    /// 8. Return the `LoadedLibrary` RAII handle.
    pub fn load(
        &self,
        id: &CanonicalId,
        request_fingerprint: &BuildFingerprint,
    ) -> Result<LoadedLibrary, LoadError> {
        let active = self
            .cache
            .read_active(id)
            .ok_or(LoadError::NoActive)?;

        // Step 2: active.
        let active_match = verify_generation_fingerprint(
            &self.cache,
            id,
            active.generation,
            request_fingerprint,
        )
        .map_err(LoadError::FingerprintCorrupt)?;
        let (generation, needs_reactivate) = if active_match.matched {
            (active.generation, false)
        } else {
            // Step 3: inactive.
            let key = request_fingerprint.build_key();
            let Some(candidate) = self.cache.lookup_inactive(id, &key) else {
                return Err(LoadError::Miss);
            };
            let cand_match = verify_generation_fingerprint(
                &self.cache,
                id,
                candidate,
                request_fingerprint,
            )
            .map_err(LoadError::FingerprintCorrupt)?;
            if !cand_match.matched {
                return Err(LoadError::FingerprintMismatch);
            }
            (candidate, true)
        };

        let lib_ext = request_fingerprint.crate_type_default_lib_ext();
        let path = self.cache.artifact_path(id, generation, &lib_ext);

        // Step 4: reactivate if needed.
        if needs_reactivate {
            let ptr = crate::staging::ActivePointer {
                generation,
                fingerprint_hash: request_fingerprint.build_key().0,
                compiler_service_schema: request_fingerprint.compiler_service_schema,
            };
            self.cache
                .write_active(id, &ptr, false)
                .map_err(|e| LoadError::OpenLibraryFailed(format!("reactivate active.bin: {e}")))?;
        }

        // Step 5: register BEFORE open_library.
        self.cache.register(id, generation);

        // Step 6: open the library.
        // SAFETY: code is loaded from `<cache_root>/<id>/gen-<N>/lib<id>.<ext>`,
        // a path the cache owns and writes under an atomic protocol. The
        // loader registers in MappedSet before this call so the retention
        // sweep cannot evict the generation. The library is assumed
        // trustworthy in the same way `renzora_plugin_build`'s loader
        // trusts compiled plugins: it is project-authored code on the
        // user's machine.
        let library = unsafe { Library::new(&path) }
            .map_err(|e| {
                // Step 6 rollback: unregister so the generation is
                // evictable.
                self.cache.unregister(id, generation);
                LoadError::OpenLibraryFailed(format!("{}: {e}", path.display()))
            })?;

        // Step 7: verify required symbols are present.
        let required = self.required_symbols.lock().clone();
        for name in &required {
            // SAFETY: we are not dereferencing; we only check the
            // library's exported-symbol table.
            let lookup_result = unsafe {
                let sym: Result<libloading::Symbol<*mut std::ffi::c_void>, _> =
                    library.get(name);
                sym.is_ok()
            };
            if !lookup_result {
                self.cache.unregister(id, generation);
                return Err(LoadError::OpenLibraryFailed(format!(
                    "library is missing required symbol {}",
                    String::from_utf8_lossy(name)
                )));
            }
        }

        Ok(LoadedLibrary {
            identity: id.clone(),
            generation,
            path,
            library: Some(library),
            cache: self.cache.clone(),
        })
    }

    /// True iff `id` has a published generation whose fingerprint matches.
    pub fn has_match(
        &self,
        id: &CanonicalId,
        request_fingerprint: &BuildFingerprint,
    ) -> bool {
        let Some(active) = self.cache.read_active(id) else {
            return false;
        };
        if let Ok(r) = verify_generation_fingerprint(
            &self.cache,
            id,
            active.generation,
            request_fingerprint,
        ) {
            if r.matched {
                return true;
            }
        }
        let key = request_fingerprint.build_key();
        if let Some(gen) = self.cache.lookup_inactive(id, &key) {
            if let Ok(r) = verify_generation_fingerprint(
                &self.cache,
                id,
                gen,
                request_fingerprint,
            ) {
                return r.matched;
            }
        }
        false
    }

    /// Return the generation number currently active for `id` (no
    /// fingerprint verification — caller responsibility).
    pub fn active_generation(&self, id: &CanonicalId) -> Option<PublishedGeneration> {
        self.cache.read_active(id).map(|p| p.generation)
    }

    /// Return the published artifact path for `id`'s active generation.
    pub fn active_artifact_path(
        &self,
        id: &CanonicalId,
        fingerprint: &BuildFingerprint,
    ) -> Option<PathBuf> {
        let active = self.cache.read_active(id)?;
        let lib_ext = fingerprint.crate_type_default_lib_ext();
        Some(self.cache.artifact_path(id, active.generation, &lib_ext))
    }
}

/// The revision associated with a load. Surfaced for the Bevy adapter.
pub fn _build_revision_anchor(_id: &CanonicalId, _rev: Revision) {}
