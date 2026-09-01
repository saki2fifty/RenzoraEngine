//! Startup reconciliation. See `phase2-cached-compiler-design.md` §6.4 / §8.4.

use renzora_identity::CanonicalId;

use crate::staging::{ActivePointer, ArtifactCache};

/// Summary of a startup-reconciliation pass.
#[derive(Clone, Debug, Default)]
pub struct ReconciliationReport {
    /// Ids whose `stage-<uuid>/` was moved to `abandoned/<uuid>/`.
    pub stages_abandoned: usize,
    /// Ids whose `abandoned/<uuid>/` directories were removed.
    pub abandoned_cleaned: usize,
    /// Ids whose `active.bin` pointed to a missing generation; fallback
    /// generation selected (None = no fallback available).
    pub active_fallbacks: Vec<(CanonicalId, Option<crate::types::PublishedGeneration>)>,
    /// Removed incomplete `gen-<N>/` directories (no `fingerprint.bin`).
    pub incomplete_gens_removed: usize,
}

/// Run the startup reconciliation pass. Idempotent.
pub fn startup_reconcile(cache: &ArtifactCache) -> ReconciliationReport {
    let mut report = ReconciliationReport::default();
    let root = cache.root();
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name == "_cargo_target" {
            continue;
        }
        let Some(id) = parse_id_dir(&name) else {
            continue;
        };

        // Move stage-<uuid>/ to abandoned/<uuid>/ when no worker for that
        // id is running (at startup, no worker is).
        if let Ok(sub) = std::fs::read_dir(&path) {
            for sub_entry in sub.flatten() {
                let sub_name = sub_entry.file_name().to_string_lossy().to_string();
                if let Some(uuid) = sub_name.strip_prefix("stage-") {
                    let from = sub_entry.path();
                    let to = path.join("abandoned").join(uuid);
                    let _ = std::fs::create_dir_all(path.join("abandoned"));
                    let _ = std::fs::rename(&from, &to);
                    report.stages_abandoned += 1;
                }
            }
        }

        // Remove abandoned/<uuid>/ directories (best-effort).
        let abandoned = path.join("abandoned");
        if abandoned.exists() {
            if let Ok(sub) = std::fs::read_dir(&abandoned) {
                for sub in sub.flatten() {
                    let _ = std::fs::remove_dir_all(sub.path());
                    report.abandoned_cleaned += 1;
                }
            }
        }

        // Remove `active.bin.tmp.*` partial writes (T2.35).
        if let Ok(sub) = std::fs::read_dir(&path) {
            for sub in sub.flatten() {
                let Some(name) = sub.file_name().to_str().map(|s| s.to_string()) else { continue };
                if name.starts_with("active.bin.tmp.") {
                    let _ = std::fs::remove_file(sub.path());
                }
            }
        }

        // Verify active.bin → gen-<N>.
        let active = cache.read_active(&id);
        if let Some(active) = active {
            let gen_dir = cache.gen_dir(&id, active.generation);
            if !gen_dir.join("fingerprint.bin").exists() || !gen_dir.join("status.bin").exists() {
                report.active_fallbacks.push((id.clone(), find_fallback_generation(cache, &id, &active)));
            }
        }

        // Drop incomplete gen-<N>/ directories (no fingerprint.bin).
        if let Ok(sub) = std::fs::read_dir(&path) {
            for sub_entry in sub.flatten() {
                let sub_name = sub_entry.file_name().to_string_lossy().to_string();
                if let Some(rest) = sub_name.strip_prefix("gen-") {
                    if rest.parse::<u64>().is_ok() {
                        let gen_path = sub_entry.path();
                        if !gen_path.join("fingerprint.bin").exists() {
                            let _ = std::fs::remove_dir_all(&gen_path);
                            report.incomplete_gens_removed += 1;
                        }
                    }
                }
            }
        }
    }
    report
}

fn parse_id_dir(name: &str) -> Option<CanonicalId> {
    let scheme_path = crate::staging::from_safe_id_dir_name(name)?;
    CanonicalId::parse(&scheme_path).ok()
}

fn find_fallback_generation(
    cache: &ArtifactCache,
    id: &CanonicalId,
    _active: &ActivePointer,
) -> Option<crate::types::PublishedGeneration> {
    let id_dir = crate::staging::id_dir(cache.root(), id);
    let Ok(read_dir) = std::fs::read_dir(&id_dir) else {
        return None;
    };
    let mut candidates: Vec<u64> = Vec::new();
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
        if entry.path().join("fingerprint.bin").exists() && entry.path().join("status.bin").exists() {
            candidates.push(n);
        }
    }
    candidates.sort_by_key(|n| std::cmp::Reverse(*n));
    candidates.first().copied().map(crate::types::PublishedGeneration)
}
