//! Stable compiler paths backed by verified, immutable input snapshots.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::EngineBuildJob;

/// Called only while the cache-wide build lock is held through compilation.
pub(crate) fn prepare(job: &EngineBuildJob) -> io::Result<PathBuf> {
    let destination = directory(job);
    let source = fs::canonicalize(&job.workspace)?;
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working copy and snapshot must not overlap",
        ));
    }
    refresh_snapshot(&source, &destination, &job.stamp.build_kit_hash)?;
    Ok(destination)
}

fn refresh_snapshot(source: &Path, destination: &Path, kit_hash: &str) -> io::Result<()> {
    let complete = destination.join(".renzora-working-copy-complete");
    // A stable compiler path may now serve a newer kit. Reuse vendor bytes
    // only for the exact kit that completed this copy; failed refreshes carry
    // no marker and must revalidate the complete snapshot on the next attempt.
    let reusable = fs::read(&complete).ok().as_deref() == Some(kit_hash.as_bytes());
    if complete.exists() {
        fs::remove_file(&complete)?;
    }
    // A failed refresh leaves no completion marker. The next attempt refreshes
    // vendor bytes as well rather than trusting a partially populated tree.
    synchronize(source, destination, reusable, true)?;
    fs::write(complete, kit_hash)?;
    Ok(())
}

pub(crate) fn directory(job: &EngineBuildJob) -> PathBuf {
    job.cache_root.join("workspaces").join(workspace_key(
        &job.stamp.toolchain_hash,
        &job.target,
        &job.profile,
    ))
}

fn workspace_key(toolchain: &str, target: &str, profile: &str) -> String {
    blake3::hash(
        format!(
            "{}:{toolchain}:{target}:{profile}",
            crate::native_overlay::NATIVE_OVERLAY_SCHEMA,
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn synchronize(
    source: &Path,
    destination: &Path,
    reuse_vendor: bool,
    root: bool,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "snapshot contains a non-regular entry",
        ));
    }
    if let Ok(existing) = fs::symlink_metadata(destination) {
        if existing.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "working copy contains a symlink",
            ));
        }
        if existing.is_dir() != metadata.is_dir() {
            remove(destination)?;
        }
    }
    if metadata.is_file() {
        // Preserving mtime is essential: touching every crate on each request
        // makes Cargo rerun otherwise unchanged compilation/build-script work.
        if fs::read(destination).ok().as_deref() != Some(fs::read(source)?.as_slice()) {
            if destination.exists() {
                crate::overlay::make_owner_writable(destination).map_err(io::Error::other)?;
            }
            fs::copy(source, destination)?;
        }
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(destination)? {
        let entry = entry?;
        if !source.join(entry.file_name()).exists() {
            remove(&entry.path())?;
        }
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if root
            && reuse_vendor
            && entry.file_name() == "vendor"
            && destination.join("vendor").is_dir()
        {
            continue;
        }
        synchronize(
            &entry.path(),
            &destination.join(entry.file_name()),
            false,
            false,
        )?;
    }
    Ok(())
}

fn remove(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_refresh_preserves_paths_but_refreshes_changed_vendor_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let output = temp.path().join("output");
        fs::create_dir_all(source.join("vendor")).expect("vendor");
        fs::write(source.join("vendor/dep.rs"), "same").expect("dependency");
        fs::write(source.join("engine.rs"), "engine").expect("engine");
        refresh_snapshot(&source, &output, "kit-a").expect("first kit");
        let before = fs::metadata(output.join("engine.rs"))
            .expect("metadata")
            .modified()
            .expect("mtime");
        fs::write(source.join("vendor/dep.rs"), "updated").expect("updated dependency");
        refresh_snapshot(&source, &output, "kit-b").expect("new kit");
        assert_eq!(
            fs::read(output.join("vendor/dep.rs")).expect("updated vendor"),
            b"updated"
        );
        assert_eq!(
            fs::metadata(output.join("engine.rs"))
                .expect("metadata")
                .modified()
                .expect("mtime"),
            before
        );
        assert_eq!(
            fs::read(output.join(".renzora-working-copy-complete")).expect("provenance"),
            b"kit-b"
        );
        fs::remove_file(output.join(".renzora-working-copy-complete")).expect("incomplete refresh");
        fs::write(output.join("vendor/dep.rs"), "partial").expect("partial copy");
        refresh_snapshot(&source, &output, "kit-b").expect("recover incomplete refresh");
        assert_eq!(
            fs::read(output.join("vendor/dep.rs")).expect("recovered vendor"),
            b"updated"
        );
        assert_ne!(
            workspace_key("toolchain-a", "target", "dist"),
            workspace_key("toolchain-b", "target", "dist")
        );
        assert_ne!(
            workspace_key("toolchain-a", "target", "dist"),
            workspace_key("toolchain-a", "other", "dist")
        );
        assert_ne!(
            workspace_key("toolchain-a", "target", "dist"),
            workspace_key("toolchain-a", "target", "release")
        );
    }

    #[test]
    fn refresh_preserves_unchanged_files_and_retires_removed_sources() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let output = temp.path().join("output");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("engine.rs"), "engine").expect("engine");
        fs::write(source.join("plugin.rs"), "v1").expect("plugin");
        synchronize(&source, &output, false, true).expect("first copy");
        let before = fs::metadata(output.join("engine.rs"))
            .expect("metadata")
            .modified()
            .expect("mtime");
        fs::write(source.join("plugin.rs"), "v2").expect("save");
        fs::write(output.join("retired.rs"), "old").expect("retired");
        synchronize(&source, &output, true, true).expect("refresh");
        assert_eq!(
            fs::metadata(output.join("engine.rs"))
                .expect("metadata")
                .modified()
                .expect("mtime"),
            before
        );
        assert_eq!(fs::read(output.join("plugin.rs")).expect("plugin"), b"v2");
        assert!(!output.join("retired.rs").exists());
        assert_eq!(
            fs::read(source.join("engine.rs")).expect("source unchanged"),
            b"engine"
        );
    }
}
