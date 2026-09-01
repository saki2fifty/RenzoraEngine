//! Shared Cargo target partition registry + `PartitionLock` mutex.
//!
//! See `phase2-cached-compiler-design.md` §5 and §8.3 (NB-4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

/// The full set of dimensions that distinguish one Cargo partition from
/// another. A new dimension set is a new partition; a partition is a separate
/// target directory AND a separate `PartitionLock`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PartitionKey {
    pub target_triple: String,
    pub toolchain_stamp: String,
    pub capabilities_canonical: String,
    pub profile: String,
    pub abi_version: u32,
    pub compiler_service_schema: u32,
}

impl PartitionKey {
    pub fn from_inputs(
        target_triple: &str,
        toolchain_stamp: &str,
        capabilities: &std::collections::BTreeSet<String>,
        profile: &str,
        abi_version: u32,
        compiler_service_schema: u32,
    ) -> Self {
        Self {
            target_triple: target_triple.to_string(),
            toolchain_stamp: toolchain_stamp.to_string(),
            capabilities_canonical: capabilities.iter().cloned().collect::<Vec<_>>().join(":"),
            profile: profile.to_string(),
            abi_version,
            compiler_service_schema,
        }
    }

    /// Path to the partition's `_cargo_target` directory.
    pub fn target_dir(&self, root: &Path) -> PathBuf {
        let cap = if self.capabilities_canonical.is_empty() {
            "default".to_string()
        } else {
            self.capabilities_canonical.clone()
        };
        root.join("_cargo_target")
            .join(sanitize(&self.target_triple))
            .join(sanitize(&self.toolchain_stamp))
            .join(format!("abi-{}-{:x}", self.abi_version, hash_str(&self.capabilities_canonical)))
            .join(sanitize(&self.profile))
            .join(sanitize(&cap))
            .join(format!("cs-{}", self.compiler_service_schema))
    }
}

/// Mutex protecting one partition's serial cargo invocations AND its deletion.
/// Holds an actual `parking_lot::Mutex<()>`. Acquired through
/// [`partition_lock`] which returns a [`PartitionGuard`] that retains the
/// mutex guard for the lifetime of the protected operation.
pub struct PartitionLock {
    mutex: Mutex<()>,
}

impl Default for PartitionLock {
    fn default() -> Self {
        Self { mutex: Mutex::new(()) }
    }
}

/// One partition's metadata: directory + lock + `last_touched` time.
pub struct PartitionEntry {
    pub key: PartitionKey,
    pub target_dir: PathBuf,
    pub lock: Arc<PartitionLock>,
    pub last_touched: Mutex<std::time::Instant>,
}

#[derive(Default)]
pub struct PartitionRegistry {
    partitions: Mutex<HashMap<PartitionKey, Arc<PartitionEntry>>>,
}

impl PartitionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up or insert the partition for `key`.
    pub fn get_or_insert(&self, key: PartitionKey, cache_root: &Path) -> Arc<PartitionEntry> {
        let mut partitions = self.partitions.lock();
        if let Some(entry) = partitions.get(&key) {
            return entry.clone();
        }
        let entry = Arc::new(PartitionEntry {
            target_dir: key.target_dir(cache_root),
            key: key.clone(),
            lock: Arc::new(PartitionLock::default()),
            last_touched: Mutex::new(std::time::Instant::now()),
        });
        partitions.insert(key, entry.clone());
        entry
    }

    pub fn snapshot(&self) -> Vec<(PartitionKey, PathBuf)> {
        let partitions = self.partitions.lock();
        partitions
            .values()
            .map(|e| (e.key.clone(), e.target_dir.clone()))
            .collect()
    }
}

/// Acquire the partition's real mutex. The returned [`PartitionGuard`]
/// retains the lock until drop — holds for workspace manifest updates,
/// Cargo invocation, AND eviction.
pub fn partition_lock(entry: &Arc<PartitionEntry>) -> PartitionGuard<'_> {
    PartitionGuard {
        _guard: Some(entry.lock.mutex.lock()),
    }
}

/// RAII guard for a partition's mutex.
pub struct PartitionGuard<'a> {
    _guard: Option<parking_lot::MutexGuard<'a, ()>>,
}

impl<'a> PartitionGuard<'a> {
    /// Return the inner mutex guard (for callers that need to call
    /// additional APIs that take a guard).
    pub fn into_inner(mut self) -> parking_lot::MutexGuard<'a, ()> {
        self._guard
            .take()
            .expect("PartitionGuard consumed exactly once")
    }
}

impl Drop for PartitionGuard<'_> {
    fn drop(&mut self) {
        // The guard's drop releases the lock.
        self._guard.take();
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | ' ' | '\0' | '\n' | '\r' | '\t' | '(' | ')' => '_',
            other => other,
        })
        .collect()
}

fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
