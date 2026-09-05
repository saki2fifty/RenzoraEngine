//! Immutable reload images with process-leased cleanup ownership.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const MARKER: &[u8] = b"renzora-shadow-session-v1\n";

struct Session {
    dir: PathBuf,
    // Held for the process lifetime, like the mapped images themselves. The OS
    // releases this lock on process exit, including an abnormal termination.
    _lease: File,
}

static SESSIONS: LazyLock<Mutex<HashMap<PathBuf, Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn session_dir(plugin_dir: &Path) -> io::Result<PathBuf> {
    let root = plugin_dir.join(".reload");
    fs::create_dir_all(&root)?;
    let root = fs::canonicalize(root)?;
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| io::Error::other("shadow session lock poisoned"))?;
    if let Some(session) = sessions.get(&root) {
        return Ok(session.dir.clone());
    }
    let session = create_session(&root)?;
    let dir = session.dir.clone();
    sessions.insert(root, session);
    Ok(dir)
}

fn create_session(root: &Path) -> io::Result<Session> {
    // Serializes creation/cleanup between processes. Never remove this lock file:
    // replacing its inode would give different editors different locks.
    let root_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("sessions.lock"))?;
    root_lock.lock()?;
    reclaim_stale(root)?;
    let dir = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(root)?;
    let mut lease = File::create_new(dir.path().join("lease"))?;
    lease.lock()?;
    lease.write_all(MARKER)?;
    lease.sync_all()?;
    // A TempDir must not remove a session at shutdown while libraries are still
    // mapped. Only a later process holding the cleanup lock may reclaim it.
    Ok(Session {
        dir: dir.keep(),
        _lease: lease,
    })
}

fn reclaim_stale(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("session-")
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        let Ok(mut lease) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("lease"))
        else {
            continue;
        };
        if lease.try_lock().is_err() {
            continue;
        }
        let mut marker = Vec::new();
        (&mut lease)
            .take(MARKER.len() as u64 + 1)
            .read_to_end(&mut marker)?;
        if marker != MARKER {
            continue;
        }
        // root_lock excludes creators/reclaimers. No supported process reopens
        // an old session for writing. Close the lease before deletion on Windows.
        drop(lease);
        if let Err(error) = fs::remove_dir_all(&path) {
            bevy::log::warn!(
                "[plugin] could not reclaim stale shadow session {}: {error}",
                path.display()
            );
        }
    }
    // Flat legacy copies have no ownership lease. They might belong to an older
    // running editor, so they are deliberately not deleted automatically.
    Ok(())
}

pub(super) fn prepare(plugin_dir: &Path) -> io::Result<()> {
    session_dir(plugin_dir).map(|_| ())
}

pub(super) fn copy(path: &Path, generation: u32) -> io::Result<PathBuf> {
    let dir = session_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;
    copy_into(path, generation, &dir)
}

fn copy_into(path: &Path, generation: u32, dir: &Path) -> io::Result<PathBuf> {
    let mut source = File::open(path)?;
    let before = source.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("plugin image is not a regular file"));
    }
    // Generation is only a diagnostic hint. A failed generation can be retried,
    // and no attempt may truncate a previous attempt's mapped image.
    let mut output = tempfile::Builder::new()
        .prefix(&format!("image-{generation}-"))
        .suffix(&format!(".{}", std::env::consts::DLL_EXTENSION))
        .tempfile_in(dir)?;
    let copied = io::copy(&mut source, &mut output)?;
    let after = source.metadata()?;
    if copied != before.len()
        || after.len() != before.len()
        || after.modified()? != before.modified()?
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "plugin image changed while staging",
        ));
    }
    output.as_file().sync_all()?;
    // The path reaches the loader only after the complete copy is durable and
    // the write handle is closed. Failure drops just this unexposed temp file.
    output.into_temp_path().keep().map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_generation_never_overwrites_prior_image() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("plugin.bin");
        fs::write(&source, b"first image").unwrap();
        let first = copy_into(&source, 7, root.path()).unwrap();
        fs::write(&source, b"second image").unwrap();
        let second = copy_into(&source, 7, root.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), b"first image");
        assert_eq!(fs::read(second).unwrap(), b"second image");
    }

    #[test]
    fn cleanup_preserves_live_and_unknown_sessions_and_reclaims_only_stale_owned_files() {
        let root = tempfile::tempdir().unwrap();
        let live = create_session(root.path()).unwrap();
        let stale = create_session(root.path()).unwrap();
        let stale_path = stale.dir.clone();
        fs::write(stale.dir.join("image.dll"), b"old image").unwrap();
        let unknown = root.path().join("session-unknown");
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("lease"), b"not ours").unwrap();
        let legacy = root.path().join("old-1.dll");
        fs::write(&legacy, b"legacy image").unwrap();
        drop(stale);
        let next = create_session(root.path()).unwrap();
        assert!(live.dir.exists());
        assert!(!stale_path.exists());
        assert!(unknown.exists());
        assert!(legacy.exists());
        drop(next);
        drop(live);
    }

    #[test]
    fn missing_source_does_not_leave_a_partial_image() {
        let root = tempfile::tempdir().unwrap();
        assert!(copy_into(&root.path().join("missing"), 1, root.path()).is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn child_session_holder() {
        let Some(root) = std::env::var_os("RENZORA_SHADOW_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let session = create_session(&root).unwrap();
        fs::write(
            root.join("ready"),
            session.dir.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.join("release").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(session);
    }

    #[test]
    fn other_process_session_is_protected_until_that_process_exits() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::shadow_images::tests::child_session_holder",
                ])
                .env("RENZORA_SHADOW_TEST_ROOT", root.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let name = fs::read_to_string(root.path().join("ready")).expect("child acquired lease");
        let child_dir = root.path().join(name);
        let own = create_session(root.path()).unwrap();
        assert!(
            child_dir.exists(),
            "another process still owns its image directory"
        );
        fs::write(root.path().join("release"), b"release").unwrap();
        assert!(child.0.wait().unwrap().success());
        let next = create_session(root.path()).unwrap();
        assert!(!child_dir.exists(), "expired session is reclaimable");
        assert!(own.dir.exists());
        drop(next);
        drop(own);
    }
}
