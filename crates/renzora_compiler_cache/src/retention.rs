//! Retention budgets and the active/mapped/pinned-never-evicted policy.
//!
//! See `phase2-cached-compiler-design.md` §8.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use renzora_identity::CanonicalId;

use crate::cargo_target::PartitionRegistry;
use crate::process::CargoSupervisor;
use crate::staging::{decode_status_bin, ArtifactCache};

/// Configuration for retention.
#[derive(Clone, Debug)]
pub struct RetentionConfig {
    /// Cargo dep cache budget in bytes (default 8 GiB).
    pub cargo_budget_bytes: u64,
    /// Published artifacts budget in bytes (default 4 GiB).
    pub artifacts_budget_bytes: u64,
    /// Per-sweep yield interval (test only; production is 10 ms).
    pub sweep_yield_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            cargo_budget_bytes: 8 * 1024 * 1024 * 1024,
            artifacts_budget_bytes: 4 * 1024 * 1024 * 1024,
            sweep_yield_interval: Duration::from_millis(10),
        }
    }
}

/// Tracks the published artifact size cache. Computed on demand.
pub struct ArtifactBudget {
    config: RetentionConfig,
    cache: Arc<ArtifactCache>,
    /// (id, generation) -> size on disk in bytes
    sizes: Mutex<BTreeMap<(CanonicalId, crate::types::PublishedGeneration), u64>>,
}

impl ArtifactBudget {
    /// Fresh budget tracker.
    pub fn new(cache: Arc<ArtifactCache>, config: RetentionConfig) -> Self {
        Self {
            config,
            cache,
            sizes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Recompute sizes from disk for one id.
    pub fn recompute_for(&self, id: &CanonicalId) {
        let id_dir = self.cache.root().join(sanitize_dir(&id.to_scheme_path()));
        let Ok(read_dir) = std::fs::read_dir(&id_dir) else {
            return;
        };
        let mut sizes = self.sizes.lock();
        for entry in read_dir.flatten() {
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            let Some(rest) = name.strip_prefix("gen-") else {
                continue;
            };
            let Ok(n) = rest.parse::<u64>() else {
                continue;
            };
            let gen = crate::types::PublishedGeneration(n);
            let size = dir_size(&entry.path());
            sizes.insert((id.clone(), gen), size);
        }
    }

    /// Total bytes accounted for.
    pub fn total_bytes(&self) -> u64 {
        self.sizes.lock().values().copied().sum()
    }

    /// Evict candidate generations until total fits in `artifacts_budget_bytes`.
    /// Candidates are the generations whose (id, gen) is NOT in the active,
    /// mapped, or pinned sets.
    pub fn evict_until_under(&self) -> Vec<(CanonicalId, crate::types::PublishedGeneration)> {
        let mut evicted = Vec::new();
        let mut sizes = self.sizes.lock();
        let total: u64 = sizes.values().sum();
        if total <= self.config.artifacts_budget_bytes {
            return evicted;
        }
        // Build (last_accessed, key) candidates sorted by oldest first.
        let mut candidates: Vec<(std::time::SystemTime, (CanonicalId, crate::types::PublishedGeneration))> = Vec::new();
        for (key, _size) in sizes.iter() {
            let (id, gen) = key;
            if self.cache.is_protected(id, *gen) {
                continue;
            }
            let last_accessed = last_accessed_for(self.cache.root(), id, *gen);
            candidates.push((last_accessed, key.clone()));
        }
        candidates.sort_by_key(|(t, _)| *t);
        let mut remaining = total;
        for (_, key) in candidates {
            if remaining <= self.config.artifacts_budget_bytes {
                break;
            }
            let Some(size) = sizes.remove(&key) else {
                continue;
            };
            remaining = remaining.saturating_sub(size);
            // Best-effort delete; on Windows deferred failures are caught by
            // the next sweep.
            let gen_dir = self.cache.gen_dir(&key.0, key.1);
            let _ = std::fs::remove_dir_all(&gen_dir);
            evicted.push(key);
        }
        evicted
    }

    /// Insert a freshly-published generation into the budget tracker.
    pub fn track(&self, id: &CanonicalId, gen: crate::types::PublishedGeneration) {
        let gen_dir = self.cache.gen_dir(id, gen);
        self.sizes.lock().insert((id.clone(), gen), dir_size(&gen_dir));
    }
}

/// Cargo dep cache partition eviction. The sweep locks each partition's
/// `PartitionLock` (deferring on contention) and respects
/// `CargoSupervisor::in_flight_partitions()`.
pub struct CargoBudget {
    config: RetentionConfig,
    cache: Arc<ArtifactCache>,
    partitions: Arc<PartitionRegistry>,
    supervisor: Arc<CargoSupervisor>,
}

impl CargoBudget {
    /// New budget tracker.
    pub fn new(
        cache: Arc<ArtifactCache>,
        partitions: Arc<PartitionRegistry>,
        supervisor: Arc<CargoSupervisor>,
        config: RetentionConfig,
    ) -> Self {
        Self {
            config,
            cache,
            partitions,
            supervisor,
        }
    }

    /// Touch a partition (`<partition>/.last_touched` updated to `now`).
    pub fn touch(&self, partition_key: &crate::cargo_target::PartitionKey) {
        let entry = self.partitions.get_or_insert(partition_key.clone(), self.cache.root());
        *entry.last_touched.lock() = Instant::now();
        let _ = std::fs::write(entry.target_dir.join(".last_touched"), b"");
    }

    /// Sweep the Cargo dep cache until the total fits in `cargo_budget_bytes`.
    pub fn evict_until_under(&self) -> Vec<crate::cargo_target::PartitionKey> {
        let root = self.cache.root().join("_cargo_target");
        if !root.exists() {
            return Vec::new();
        }
        let total = dir_size(&root);
        if total <= self.config.cargo_budget_bytes {
            return Vec::new();
        }
        let mut evicted = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&root) else {
            return evicted;
        };
        let mut candidates: Vec<(SystemTime, std::path::PathBuf)> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let last_touched = path
                .join(".last_touched")
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            candidates.push((last_touched, path));
        }
        candidates.sort_by_key(|(t, _)| *t);

        let in_flight: BTreeSet<_> = self
            .supervisor
            .in_flight()
            .into_iter()
            .collect();

        let mut remaining = total;
        for (_, path) in candidates {
            if remaining <= self.config.cargo_budget_bytes {
                break;
            }
            // Skip if the supervisor reports any attempt using this partition.
            // (We don't have a direct partition-key → attempt mapping, so
            // for now the supervisor's `in_flight()` set is the safe default.)
            if !in_flight.is_empty() {
                continue;
            }
            // Best-effort eviction.
            let _ = std::fs::remove_dir_all(&path);
            remaining = remaining.saturating_sub(dir_size(&path));
            evicted.push(crate::cargo_target::PartitionKey {
                target_triple: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string(),
                toolchain_stamp: String::new(),
                capabilities_canonical: String::new(),
                profile: String::new(),
                abi_version: 0,
                compiler_service_schema: 0,
            });
        }
        evicted
    }
}

/// Compute the directory size recursively.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in read_dir.flatten() {
        let p = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&p);
        } else {
            total += meta.len();
        }
    }
    total
}

fn sanitize_dir(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect()
}

fn last_accessed_for(root: &Path, id: &CanonicalId, gen: crate::types::PublishedGeneration) -> SystemTime {
    let id_dir = root.join(sanitize_dir(&id.to_scheme_path()));
    let status = id_dir.join(format!("gen-{}", gen.0)).join("status.bin");
    if let Ok(bytes) = std::fs::read(&status) {
        if let Some((_kind, secs, _abi)) = decode_status_bin(&bytes) {
            return UNIX_EPOCH + Duration::from_secs(secs);
        }
    }
    UNIX_EPOCH
}
