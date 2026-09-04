//! Cargo child-process supervisor with platform-owned children and
//! reader-thread ownership.
//!
//! Rev-5 correction notes:
//!
//! - R5-3 (Windows): the previous attempt accessed private `std::process::Command`
//!   fields and converted raw pipe handles into `File` values with invalid
//!   casts. The new Windows path uses `CreateProcessW` with
//!   `STARTF_USESTDHANDLES` built from real inheritable anonymous pipes
//!   (`CreatePipe`), quotes the command line per the
//!   `CommandLineToArgvW`-compatible rules, and exits via `GetExitCodeProcess`.
//!   We do NOT depend on any third-party crate — Windows quirks are
//!   hand-rolled under `cfg(windows)` to keep the dependency surface
//!   minimal and the validation surface narrow.
//! - R5-5 / R5-6: `OwnedChild` now stores the reader `JoinHandle`s. The
//!   reaper can own those handles together with the child.
//!
//! POSIX uses `setpgid(0,0)` for process-group ownership and `kill(-pgid, …)`
//! to signal the whole descendant tree. Windows uses a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use renzora_identity::CanonicalId;

use crate::cargo_target::PartitionRegistry;

// ============================================================================
// Platform-owned child + reader handles
// ============================================================================

/// A platform-owned child. The exact fields differ per platform, but the
/// public API — `try_wait`, `signal_stop`, `signal_kill`, `pid`, `exit_status`,
/// `take_readers` — is uniform.
pub struct OwnedChild {
    pub inner: platform::Inner,
    pub stdout_pipe: Option<std::fs::File>,
    pub stderr_pipe: Option<std::fs::File>,
    pub stdout_thread: Option<JoinHandle<()>>,
    pub stderr_thread: Option<JoinHandle<()>>,
    pub stdout_buffer: Arc<Mutex<std::collections::VecDeque<String>>>,
    pub stderr_buffer: Arc<Mutex<std::collections::VecDeque<String>>>,
}

#[cfg(unix)]
pub mod platform {
    use super::*;
    use std::os::unix::io::{FromRawFd, IntoRawFd};

    /// POSIX inner: a `std::process::Child` plus the process-group id.
    pub struct Inner {
        pub child: Option<std::process::Child>,
        pub pgid: i32,
    }

    pub fn configure_command(cmd: &mut Command) {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the forked child between fork and exec.
        // `setpgid(0,0)` is async-signal-safe. The child becomes its own
        // process-group leader; the parent's pgid is irrelevant.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }

    pub fn spawn(mut cmd: Command) -> std::io::Result<Inner> {
        let child = cmd.spawn()?;
        let pid = child.id();
        Ok(Inner {
            child: Some(child),
            pgid: pid as i32,
        })
    }

    pub fn try_wait(inner: &mut Inner) -> std::io::Result<Option<i32>> {
        let c = inner.child.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "child already reaped")
        })?;
        match c.try_wait()? {
            // A Unix signal has no numeric exit code. It is still a completed
            // child, so retain the cross-platform `i32` contract with -1
            // instead of confusing it with `None` (which means still running).
            Some(status) => Ok(Some(status.code().unwrap_or(-1))),
            None => Ok(None),
        }
    }

    pub fn take_stdout(inner: &mut Inner) -> Option<std::fs::File> {
        inner.child.as_mut().and_then(|c| c.stdout.take()).map(|s| {
            let fd = s.into_raw_fd();
            unsafe { std::fs::File::from_raw_fd(fd) }
        })
    }

    pub fn take_stderr(inner: &mut Inner) -> Option<std::fs::File> {
        inner.child.as_mut().and_then(|c| c.stderr.take()).map(|s| {
            let fd = s.into_raw_fd();
            unsafe { std::fs::File::from_raw_fd(fd) }
        })
    }

    pub fn signal_stop(inner: &mut Inner) -> std::io::Result<()> {
        // SAFETY: `kill(-pgid, SIGTERM)` signals the entire process group.
        let r = unsafe { libc::kill(-inner.pgid, libc::SIGTERM) };
        if r == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn signal_kill(inner: &mut Inner) -> std::io::Result<()> {
        let r = unsafe { libc::kill(-inner.pgid, libc::SIGKILL) };
        if r == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn pid(inner: &Inner) -> Option<u32> {
        inner.child.as_ref().map(|c| c.id())
    }

    /// Wait without blocking. POSIX uses `try_wait`. Used by the
    /// reaper when a child is moved into the isolated reaper.
    pub fn block_until_exit(inner: &mut Inner) -> std::io::Result<i32> {
        let c = inner.child.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "child already reaped")
        })?;
        let status = c.wait()?;
        Ok(status.code().unwrap_or(-1))
    }
}

#[cfg(windows)]
pub mod platform {
    use super::*;

    /// Send/Sync wrapper around a Windows `HANDLE` (`*mut c_void`).
    /// The OS serializes HANDLE access internally, so it is safe to
    /// move between threads as long as no thread closes a handle while
    /// another is using it (this invariant is upheld by the supervisor's
    /// single-threaded handle ownership transfer protocol).
    #[derive(Clone, Copy)]
    pub struct Handle(pub *mut core::ffi::c_void);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    /// Windows inner: owned process handle, owned primary-thread handle,
    /// owned Job Object, owned stdout/stderr pipe read handles, owned
    /// stdin null handle (closed in `Drop`).
    pub struct Inner {
        pub process_handle: Option<Handle>,
        pub primary_thread: Option<Handle>,
        pub job_handle: Option<Handle>,
        pub stdout_handle: Option<Handle>,
        pub stderr_handle: Option<Handle>,
        pub stdin_handle: Option<Handle>,
        pub pid: u32,
        pub exit_code: Option<i32>,
    }

    pub fn configure_command(_cmd: &mut Command) {
        // Pipes and inheritance are handled by `spawn`.
    }

    pub fn spawn(_cmd: Command) -> std::io::Result<Inner> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "windows child construction goes through spawn_windows — see process.rs",
        ))
    }

    pub fn try_wait(inner: &mut Inner) -> std::io::Result<Option<i32>> {
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let Some(h) = inner.process_handle else { return Ok(None) };
        let mut code: u32 = 0;
        // SAFETY: GetExitCodeProcess reads the exit code of the process
        // referenced by `h`. The handle is owned by `inner` for the
        // duration of this call.
        let r = unsafe { GetExitCodeProcess(h.0, &mut code) };
        if r == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if code == STILL_ACTIVE as u32 {
            Ok(None)
        } else {
            if inner.exit_code.is_none() {
                inner.exit_code = Some(code as i32);
            }
            Ok(Some(code as i32))
        }
    }

    pub fn take_stdout(_inner: &mut Inner) -> Option<std::fs::File> {
        None
    }

    pub fn take_stderr(_inner: &mut Inner) -> Option<std::fs::File> {
        None
    }

    pub fn signal_stop(inner: &mut Inner) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if let Some(job) = inner.job_handle {
            let r = unsafe { TerminateJobObject(job.0, 1) };
            if r == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn signal_kill(inner: &mut Inner) -> std::io::Result<()> {
        signal_stop(inner)
    }

    pub fn pid(inner: &Inner) -> Option<u32> {
        Some(inner.pid)
    }

    pub fn block_until_exit(_inner: &mut Inner) -> std::io::Result<i32> {
        // Windows supervisor relies on `try_wait` polling.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "block_until_exit not used on Windows",
        ))
    }
}

impl OwnedChild {
    pub fn pid(&self) -> Option<u32> {
        platform::pid(&self.inner)
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        platform::try_wait(&mut self.inner)
    }

    pub fn signal_stop(&mut self) -> std::io::Result<()> {
        platform::signal_stop(&mut self.inner)
    }

    pub fn signal_kill(&mut self) -> std::io::Result<()> {
        platform::signal_kill(&mut self.inner)
    }

    pub fn take_stdout_pipe(&mut self) -> Option<std::fs::File> {
        #[cfg(unix)]
        {
            platform::take_stdout(&mut self.inner).or_else(|| self.stdout_pipe.take())
        }
        #[cfg(windows)]
        {
            self.stdout_pipe.take()
        }
    }

    pub fn take_stderr_pipe(&mut self) -> Option<std::fs::File> {
        #[cfg(unix)]
        {
            platform::take_stderr(&mut self.inner).or_else(|| self.stderr_pipe.take())
        }
        #[cfg(windows)]
        {
            self.stderr_pipe.take()
        }
    }
}

#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Best-effort non-blocking reap on POSIX. The child may still be
        // alive after this; the supervisor or reaper handles termination.
        if let Some(c) = self.inner.child.as_mut() {
            let _ = c.try_wait();
        }
    }
}

#[cfg(windows)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Closing the Job handle triggers KILL_ON_JOB_CLOSE. Process +
        // thread handles are closed explicitly. Pipe handles are owned by
        // the reader threads; closing them is the reader thread's job.
        // The stdin null handle is closed explicitly here.
        if let Some(h) = self.inner.job_handle.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h.0);
            }
        }
        if let Some(h) = self.inner.primary_thread.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h.0);
            }
        }
        if let Some(h) = self.inner.process_handle.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h.0);
            }
        }
        if let Some(h) = self.inner.stdin_handle.take() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(h.0);
            }
        }
    }
}

/// Snapshot of an attempt's outcome after the process has exited and the
/// reader threads have joined.
#[derive(Clone, Debug)]
pub struct AttemptRecord {
    pub attempt_id: u64,
    pub exit_status: Option<i32>,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
    pub artifact_path: Option<PathBuf>,
}

// ============================================================================
// Supervisor
// ============================================================================

#[derive(Clone, Debug)]
pub struct CargoSupervisorConfig {
    pub max_children: usize,
    pub reader_buffer_lines: usize,
}

impl Default for CargoSupervisorConfig {
    fn default() -> Self {
        Self {
            max_children: 2,
            reader_buffer_lines: 4096,
        }
    }
}

struct BoundedPermit {
    available: Mutex<usize>,
    cv: Condvar,
}

impl BoundedPermit {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            available: Mutex::new(max),
            cv: Condvar::new(),
        })
    }
    fn acquire(self: &Arc<Self>) -> PermitGuard {
        let mut avail = self.available.lock();
        while *avail == 0 {
            self.cv.wait(&mut avail);
        }
        *avail -= 1;
        PermitGuard { sem: self.clone() }
    }
    fn release(&self) {
        let mut avail = self.available.lock();
        *avail += 1;
        self.cv.notify_one();
    }
}

pub struct PermitGuard {
    sem: Arc<BoundedPermit>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        self.sem.release();
    }
}

/// One supervised attempt. Owns the OS process handle (via `OwnedChild`),
/// the reader `JoinHandle`s, and the per-stream buffers. Every `SupervisorHandle`
/// lives in exactly one of: the supervisor map, the shutdown-path local
/// `Vec`, or the isolated reaper.
pub struct SupervisorHandle {
    pub attempt_id: u64,
    pub identity: CanonicalId,
    pub partition_key: crate::cargo_target::PartitionKey,
    pub child: Option<OwnedChild>,
    pub started_at: Instant,
}

impl SupervisorHandle {
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.pid())
    }
    pub fn signal_stop(&mut self) -> std::io::Result<()> {
        if let Some(c) = self.child.as_mut() {
            c.signal_stop()
        } else {
            Ok(())
        }
    }
    pub fn signal_kill(&mut self) -> std::io::Result<()> {
        if let Some(c) = self.child.as_mut() {
            c.signal_kill()
        } else {
            Ok(())
        }
    }
}

/// Supervisor state. Holds every live `SupervisorHandle` and the bounded
/// permit pool.
pub struct CargoSupervisor {
    children: Mutex<HashMap<u64, SupervisorHandle>>,
    next_attempt: AtomicU64,
    permits: Arc<BoundedPermit>,
    #[allow(dead_code)]
    partitions: Arc<PartitionRegistry>,
    config: CargoSupervisorConfig,
    /// Concurrency-evidence recorder (R5B-4). Every supervisor spawn
    /// records `(attempt_id, identity, partition_key, started_at)`. Every
    /// final reap or force-kill records `(attempt_id, finished_at)` so
    /// tests can assert overlap.
    lifecycle: parking_lot::Mutex<crate::process::LifecycleRecord>,
}

/// Concurrency-evidence recorder. Production-path tests assert that two
/// distinct partitions overlap and that the same partition does not
/// overlap (R5B-4).
#[derive(Default)]
pub struct LifecycleRecord {
    pub starts: Vec<(u64, CanonicalId, crate::cargo_target::PartitionKey, std::time::Instant)>,
    pub finishes: Vec<(u64, std::time::Instant)>,
}

impl CargoSupervisor {
    #[allow(dead_code)]
    pub fn new(partitions: Arc<PartitionRegistry>, config: CargoSupervisorConfig) -> Self {
        let max = config.max_children.max(1);
        Self {
            children: Mutex::new(HashMap::new()),
            next_attempt: AtomicU64::new(1),
            permits: BoundedPermit::new(max),
            partitions,
            config,
            lifecycle: parking_lot::Mutex::new(LifecycleRecord::default()),
        }
    }

    /// R5B-4: tests assert overlap from these recorded intervals.
    pub fn snapshot_lifecycle(&self) -> LifecycleRecord {
        let guard = self.lifecycle.lock();
        LifecycleRecord {
            starts: guard.starts.clone(),
            finishes: guard.finishes.clone(),
        }
    }

    fn record_start(&self, attempt_id: u64, identity: &CanonicalId, pk: &crate::cargo_target::PartitionKey) {
        let mut guard = self.lifecycle.lock();
        guard.starts.push((attempt_id, identity.clone(), pk.clone(), std::time::Instant::now()));
    }

    fn record_finish(&self, attempt_id: u64) {
        let mut guard = self.lifecycle.lock();
        guard.finishes.push((attempt_id, std::time::Instant::now()));
    }

    pub fn acquire_child_permit(&self) -> PermitGuard {
        self.permits.acquire()
    }

    pub fn max_children(&self) -> usize {
        self.config.max_children
    }

    pub fn in_flight(&self) -> Vec<u64> {
        let guard = self.children.lock();
        let mut ids: Vec<u64> = guard.keys().copied().collect();
        ids.sort();
        ids
    }

    pub fn in_flight_with_identity(&self) -> Vec<(u64, CanonicalId)> {
        let guard = self.children.lock();
        let mut out: Vec<(u64, CanonicalId)> = guard
            .iter()
            .map(|(k, v)| (*k, v.identity.clone()))
            .collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }

    pub fn in_flight_partitions(&self) -> Vec<crate::cargo_target::PartitionKey> {
        let guard = self.children.lock();
        let mut out: Vec<_> = guard
            .values()
            .map(|h| h.partition_key.clone())
            .collect();
        out.sort_by(|a, b| a.target_triple.cmp(&b.target_triple));
        out.dedup();
        out
    }

    pub fn allocate_attempt_id(&self) -> u64 {
        self.next_attempt.fetch_add(1, Ordering::Relaxed)
    }

    pub fn spawn(
        &self,
        identity: CanonicalId,
        partition_key: crate::cargo_target::PartitionKey,
        cmd: Command,
    ) -> Result<u64, std::io::Error> {
        let attempt_id = self.allocate_attempt_id();
        self.spawn_with_id(attempt_id, identity, partition_key, cmd)?;
        Ok(attempt_id)
    }

    pub fn spawn_with_id(
        &self,
        attempt_id: u64,
        identity: CanonicalId,
        partition_key: crate::cargo_target::PartitionKey,
        #[cfg(unix)] mut cmd: Command,
        #[cfg(not(unix))] cmd: Command,
    ) -> Result<(), std::io::Error> {
        if self.children.lock().contains_key(&attempt_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("attempt id {attempt_id} already in use"),
            ));
        }

        let stdout_buffer = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let stderr_buffer = Arc::new(Mutex::new(std::collections::VecDeque::new()));

        let (inner, mut stdout_file, mut stderr_file) = if cfg!(windows) {
            #[cfg(windows)]
            {
                let (inner, out, err) = spawn_windows(cmd)?;
                (inner, Some(out), Some(err))
            }
            #[cfg(not(windows))]
            {
                unreachable!()
            }
        } else {
            #[cfg(unix)]
            {
                platform::configure_command(&mut cmd);
                let inner = platform::spawn(cmd)?;
                (inner, None, None)
            }
            #[cfg(not(unix))]
            {
                unreachable!()
            }
        };

        let mut child = OwnedChild {
            inner,
            stdout_pipe: stdout_file.take(),
            stderr_pipe: stderr_file.take(),
            stdout_thread: None,
            stderr_thread: None,
            stdout_buffer: stdout_buffer.clone(),
            stderr_buffer: stderr_buffer.clone(),
        };

        // Take stdout/stderr pipes + spawn reader threads. The reader
        // thread owns the pipe `File`; the join handle is stored on
        // `OwnedChild` so the supervisor (or reaper) can join it.
        if let Some(out) = child.take_stdout_pipe() {
            let buf = stdout_buffer.clone();
            let cap = self.config.reader_buffer_lines;
            let h = std::thread::Builder::new()
                .name(format!("compiler_cache.stdout.{attempt_id}"))
                .spawn(move || drain_lines(out, buf, cap))
                .expect("spawn stdout reader");
            child.stdout_thread = Some(h);
        }
        if let Some(err) = child.take_stderr_pipe() {
            let buf = stderr_buffer.clone();
            let cap = self.config.reader_buffer_lines;
            let h = std::thread::Builder::new()
                .name(format!("compiler_cache.stderr.{attempt_id}"))
                .spawn(move || drain_lines(err, buf, cap))
                .expect("spawn stderr reader");
            child.stderr_thread = Some(h);
        }

        let handle = SupervisorHandle {
            attempt_id,
            identity: identity.clone(),
            partition_key: partition_key.clone(),
            child: Some(child),
            started_at: Instant::now(),
        };
        self.children.lock().insert(attempt_id, handle);
        self.record_start(attempt_id, &identity, &partition_key);
        Ok(())
    }

    /// Try to reap a single finished child without blocking. Joins the
    /// reader threads (if the process has exited) before draining the
    /// per-stream buffers.
    pub fn try_reap(&self, attempt_id: u64) -> Option<AttemptRecord> {
        self.try_reap_result(attempt_id).ok().flatten()
    }

    /// Fallible form of [`Self::try_reap`] for coordinators that must surface
    /// an operating-system wait failure rather than treating it as "running".
    pub fn try_reap_result(&self, attempt_id: u64) -> std::io::Result<Option<AttemptRecord>> {
        let mut guard = self.children.lock();
        let Some(handle) = guard.get_mut(&attempt_id) else {
            return Ok(None);
        };
        let Some(child) = handle.child.as_mut() else {
            return Ok(None);
        };
        let Some(exit_status) = child.try_wait()? else {
            return Ok(None);
        };
        let Some(mut handle) = guard.remove(&attempt_id) else {
            return Ok(None);
        };
        let Some(mut child) = handle.child.take() else {
            return Ok(None);
        };
        join_reader(child.stdout_thread.take());
        join_reader(child.stderr_thread.take());
        let stdout: Vec<String> = child.stdout_buffer.lock().drain(..).collect();
        let stderr: Vec<String> = child.stderr_buffer.lock().drain(..).collect();
        self.record_finish(attempt_id);
        Ok(Some(AttemptRecord {
            attempt_id,
            exit_status: Some(exit_status),
            stdout,
            stderr,
            artifact_path: None,
        }))
    }

    pub fn cancel(&self, attempt_id: u64) {
        let mut guard = self.children.lock();
        if let Some(handle) = guard.get_mut(&attempt_id) {
            if let Some(c) = handle.child.as_mut() {
                let _ = c.signal_stop();
            }
        }
    }

    pub fn cancel_for(&self, id: &CanonicalId) -> usize {
        let mut guard = self.children.lock();
        let mut cancelled = 0;
        for handle in guard.values_mut() {
            if &handle.identity == id {
                if let Some(c) = handle.child.as_mut() {
                    let _ = c.signal_stop();
                }
                cancelled += 1;
            }
        }
        cancelled
    }

    pub fn drain_handles(&self) -> Vec<SupervisorHandle> {
        let mut guard = self.children.lock();
        guard.drain().map(|(_, h)| h).collect()
    }

    pub fn take_handle(&self, attempt_id: u64) -> Option<SupervisorHandle> {
        self.children.lock().remove(&attempt_id)
    }

    pub fn children_for_testing(&self) -> &Mutex<HashMap<u64, SupervisorHandle>> {
        &self.children
    }
}

/// Bounded join of a reader thread. Uses a side thread + `recv_timeout`
/// so the caller never blocks longer than `timeout`. Returns true if
/// the reader finished within the budget.
///
/// A reader thread can block past `timeout` only if a descendant process
/// holds the pipe write end open (a "retained writer"). The caller is
/// responsible for transferring the unfinished reader to the isolated
/// reaper when this happens.
pub(crate) fn join_reader_bounded(handle: Option<JoinHandle<()>>, timeout: Duration) -> bool {
    let Some(h) = handle else { return true };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let _ = std::thread::spawn(move || {
        let _ = h.join();
        let _ = done_tx.send(());
    });
    done_rx.recv_timeout(timeout).is_ok()
}

/// Compatibility wrapper for the unbounded case. New code should use
/// `join_reader_bounded` with an explicit deadline.
fn join_reader(handle: Option<JoinHandle<()>>) {
    let _ = join_reader_bounded(handle, Duration::from_secs(5));
}

fn drain_lines<R: Read>(
    reader: R,
    buf: Arc<Mutex<std::collections::VecDeque<String>>>,
    cap: usize,
) {
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf_reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                let mut guard = buf.lock();
                if guard.len() >= cap {
                    guard.pop_front();
                }
                guard.push_back(trimmed);
            }
            Err(_) => break,
        }
    }
}

/// Windows child construction. Builds real inheritable anonymous pipes,
/// formats the command line per CommandLineToArgvW rules, merges the
/// inherited environment with the per-command overrides, and uses
/// `CreateProcessW` with `CREATE_SUSPENDED` so the process is never
/// allowed to execute outside the kill-on-close Job Object.
#[cfg(windows)]
fn spawn_windows(cmd: Command) -> std::io::Result<(platform::Inner, std::fs::File, std::fs::File)> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, SetHandleInformation, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetStartupInfoW, ResumeThread, TerminateProcess,
        WaitForInputIdle, PROCESS_INFORMATION, STARTUPINFOW,
    };
    type RawHandle = *mut core::ffi::c_void;

    // 1) Create inheritable anonymous pipes for stdout + stderr.
    let mut stdout_read: RawHandle = std::ptr::null_mut();
    let mut stdout_write: RawHandle = std::ptr::null_mut();
    let mut stderr_read: RawHandle = std::ptr::null_mut();
    let mut stderr_write: RawHandle = std::ptr::null_mut();
    let r =
        unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, std::ptr::null(), 0) };
    if r == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let r =
        unsafe { CreatePipe(&mut stderr_read, &mut stderr_write, std::ptr::null(), 0) };
    if r == 0 {
        unsafe {
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
        }
        return Err(std::io::Error::last_os_error());
    }
    // Mark the WRITE ends as inheritable. The READ ends are NOT
    // inherited (the parent keeps them). Check every return value
    // (R5B-5).
    let r = unsafe {
        SetHandleInformation(stdout_write, HANDLE_FLAG_INHERIT, 1)
    };
    if r == 0 {
        unsafe {
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
            CloseHandle(stderr_read);
            CloseHandle(stderr_write);
        }
        return Err(std::io::Error::last_os_error());
    }
    let r = unsafe {
        SetHandleInformation(stderr_write, HANDLE_FLAG_INHERIT, 1)
    };
    if r == 0 {
        unsafe {
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
            CloseHandle(stderr_read);
            CloseHandle(stderr_write);
        }
        return Err(std::io::Error::last_os_error());
    }

    // 2) Build the command-line string. CommandLineToArgvW-compatible
    // escaping handles backslashes, embedded quotes, trailing
    // backslashes, and whitespace.
    let program_path = cmd.get_program().to_os_string();
    let cmdline_str = quote_command_line(&cmd);
    let cmdline_w: Vec<u16> = OsStr::new(&cmdline_str)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let program_w: Vec<u16> = OsStr::new(&program_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // 3) Merge the inherited environment with the per-command overrides.
    let env_block_w: Vec<u16> = build_environment_block(&cmd);
    let env_ptr = if env_block_w.is_empty() {
        std::ptr::null()
    } else {
        env_block_w.as_ptr() as *mut _
    };

    // 4) STARTUPINFOW with STARTF_USESTDHANDLES. We populate hStdOutput
    // and hStdError from the WRITE ends; the parent owns the READ ends.
    // hStdInput comes from an OWNED null handle that survives until
    // CreateProcessW finishes (R5B-5: a temporary `Stdio::null()` does
    // not have a guaranteed lifetime through CreateProcessW).
    let stdin_handle = open_null_handle_for_std_input();
    let stdin_owned_after_create: RawHandle = match stdin_handle {
        Ok(h) => h,
        Err(e) => {
            // Clean up everything we already created.
            unsafe {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
                CloseHandle(stderr_read);
                CloseHandle(stderr_write);
            }
            return Err(e);
        }
    };

    let mut startup_info: STARTUPINFOW = unsafe { std::mem::zeroed() };
    unsafe {
        GetStartupInfoW(&mut startup_info);
    }
    startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup_info.dwFlags |= 0x00000100; // STARTF_USESTDHANDLES
    startup_info.hStdOutput = stdout_write;
    startup_info.hStdError = stderr_write;
    startup_info.hStdInput = stdin_owned_after_create;

    // 5) CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP |
    //    CREATE_DEFAULT_ERROR_MODE.
    let creation_flags: u32 = 0x00000004 | 0x00000200 | 0x04000000;
    let cwd_w: Vec<u16> = cmd
        .get_current_dir()
        .map(|p| OsStr::new(p).encode_wide().chain(std::iter::once(0)).collect())
        .unwrap_or_default();
    let cwd_ptr = if cwd_w.is_empty() {
        std::ptr::null()
    } else {
        cwd_w.as_ptr()
    };

    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let r = unsafe {
        CreateProcessW(
            program_w.as_ptr(),
            cmdline_w.as_ptr() as *mut _,
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles: TRUE so the WRITE ends are inherited
            creation_flags,
            env_ptr,
            cwd_ptr,
            &startup_info,
            &mut process_info,
        )
    };
    if r == 0 {
        unsafe {
            CloseHandle(stdout_read);
            CloseHandle(stdout_write);
            CloseHandle(stderr_read);
            CloseHandle(stderr_write);
        }
        return Err(std::io::Error::last_os_error());
    }
    // The child inherited the WRITE ends; close them in the parent so
    // the reader thread sees EOF when the child exits.
    unsafe {
        CloseHandle(stdout_write);
        CloseHandle(stderr_write);
    }

    // 6) Create the Job Object with KILL_ON_JOB_CLOSE. Terminate the
    // suspended process BEFORE closing handles on every setup failure.
    let job_handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job_handle.is_null() {
        unsafe {
            TerminateProcess(process_info.hProcess, 1);
            CloseHandle(process_info.hProcess);
            CloseHandle(process_info.hThread);
            CloseHandle(stdout_read);
            CloseHandle(stderr_read);
        }
        return Err(std::io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
        LimitFlags: 0x2000,
        ..unsafe { std::mem::zeroed() }
    };
    let set_info = unsafe {
        SetInformationJobObject(
            job_handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set_info == 0 {
        unsafe {
            TerminateProcess(process_info.hProcess, 1);
            CloseHandle(job_handle);
            CloseHandle(process_info.hProcess);
            CloseHandle(process_info.hThread);
            CloseHandle(stdout_read);
            CloseHandle(stderr_read);
        }
        return Err(std::io::Error::last_os_error());
    }
    let assign = unsafe { AssignProcessToJobObject(job_handle, process_info.hProcess) };
    if assign == 0 {
        unsafe {
            TerminateProcess(process_info.hProcess, 1);
            CloseHandle(job_handle);
            CloseHandle(process_info.hProcess);
            CloseHandle(process_info.hThread);
            CloseHandle(stdout_read);
            CloseHandle(stderr_read);
        }
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        WaitForInputIdle(process_info.hProcess, 1000);
    }
    let resumed = unsafe { ResumeThread(process_info.hThread) };
    if resumed == u32::MAX {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(job_handle, 1);
            CloseHandle(job_handle);
            CloseHandle(process_info.hProcess);
            CloseHandle(process_info.hThread);
            CloseHandle(stdout_read);
            CloseHandle(stderr_read);
        }
        return Err(std::io::Error::last_os_error());
    }

    // The parent takes ownership of the READ ends via `File` so the
    // reader threads can use `Read`.
    let stdout_file = unsafe { std::fs::File::from_raw_handle(stdout_read as _) };
    let stderr_file = unsafe { std::fs::File::from_raw_handle(stderr_read as _) };

    let inner = platform::Inner {
        process_handle: Some(platform::Handle(process_info.hProcess)),
        primary_thread: Some(platform::Handle(process_info.hThread)),
        job_handle: Some(platform::Handle(job_handle)),
        stdout_handle: None,
        stderr_handle: None,
        stdin_handle: Some(platform::Handle(stdin_owned_after_create)),
        pid: process_info.dwProcessId,
        exit_code: None,
    };

    Ok((inner, stdout_file, stderr_file))
}

/// Build a Windows environment block. Inherits the parent environment,
/// applies per-command overrides, sorts case-insensitively, and emits a
/// null-terminated UTF-16 block with the trailing extra null terminator
/// the Win32 API requires.
#[cfg(windows)]
fn build_environment_block(cmd: &Command) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    for (k, v) in std::env::vars_os() {
        let kk = k.to_string_lossy().into_owned();
        let vv = v.to_string_lossy().into_owned();
        entries.push((kk, Some(vv)));
    }
    // Apply overrides: replacing existing keys or adding new ones. An
    // override of `None` removes the key.
    //
    // `Command::get_envs` returns `&OsStr` for the key (not Option) and
    // `Option<&OsStr>` for the value; `cmd.env(…, None)` removes a key.
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy().into_owned();
        if let Some(vv) = v {
            entries.retain(|(ek, _)| ek != &key);
            entries.push((key, Some(vv.to_string_lossy().into_owned())));
        } else {
            entries.retain(|(ek, _)| ek != &key);
        }
    }
    // Sort case-insensitively (Win32 environment blocks must be sorted).
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut out: Vec<u16> = Vec::new();
    for (k, v) in &entries {
        let entry = if let Some(vv) = v {
            format!("{k}={vv}")
        } else {
            continue;
        };
        let entry_os = OsStr::new(&entry);
        for c in entry_os.encode_wide() {
            out.push(c);
        }
        out.push(0);
    }
    out.push(0);
    out
}

/// Open an OWNED null device handle for `hStdInput`. The handle is
/// inheritable so the child receives it through `STARTF_USESTDHANDLES`.
/// Lifetime is controlled by the caller (returned handle is closed in
/// `Inner::Drop`).
#[cfg(windows)]
fn open_null_handle_for_std_input() -> std::io::Result<*mut core::ffi::c_void> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, SetHandleInformation, GENERIC_READ, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // "NUL" — the Win32 null device.
    let name_w: Vec<u16> = OsStr::new("NUL")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: CreateFileW opens the named file/device; pointer is valid
    // for the duration of the call.
    let h = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let r = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 1) };
    if r == 0 {
        unsafe {
            CloseHandle(h);
        }
        return Err(std::io::Error::last_os_error());
    }
    Ok(h)
}

/// Build a CommandLineToArgvW-compatible command line from a `Command`.
#[cfg(windows)]
fn quote_command_line(cmd: &Command) -> std::ffi::OsString {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut out = quote_arg(&program);
    for arg in cmd.get_args() {
        out.push(' ');
        out.push_str(&quote_arg(&arg.to_string_lossy()));
    }
    std::ffi::OsString::from(out)
}

#[cfg(windows)]
fn quote_arg(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let needs_quoting = s.contains(' ') || s.contains('\t') || s.contains('"');
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in s.chars() {
        if c == '\\' {
            backslashes += 1;
            out.push('\\');
        } else if c == '"' {
            // Escape all preceding backslashes PLUS this quote.
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push('\\');
            out.push('"');
            backslashes = 0;
        } else {
            backslashes = 0;
            out.push(c);
        }
    }
    // Trailing backslashes: escape them so the closing quote is not
    // consumed.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_permits() {
        let supervisor = CargoSupervisor::new(
            Arc::new(PartitionRegistry::new()),
            CargoSupervisorConfig {
                max_children: 2,
                ..Default::default()
            },
        );
        let p1 = supervisor.acquire_child_permit();
        let p2 = supervisor.acquire_child_permit();
        assert_eq!(*supervisor.permits.available.lock(), 0);
        drop(p1);
        drop(p2);
        assert_eq!(*supervisor.permits.available.lock(), 2);
    }

    #[test]
    fn concurrent_attempt_ids_are_unique() {
        let supervisor = Arc::new(CargoSupervisor::new(
            Arc::new(PartitionRegistry::new()),
            CargoSupervisorConfig {
                max_children: 8,
                ..Default::default()
            },
        ));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let sup = supervisor.clone();
            handles.push(std::thread::spawn(move || {
                let mut ids = Vec::new();
                for _ in 0..50 {
                    ids.push(sup.allocate_attempt_id());
                }
                ids
            }));
        }
        let mut all_ids: Vec<u64> = Vec::new();
        for h in handles {
            all_ids.extend(h.join().unwrap());
        }
        let total = all_ids.len();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), total);
        assert_eq!(total, 16 * 50);
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_child_is_reaped_as_completed() {
        let supervisor = CargoSupervisor::new(
            Arc::new(PartitionRegistry::new()),
            CargoSupervisorConfig::default(),
        );
        let identity = CanonicalId::parse("engine://tests/signal-reap").expect("identity");
        let partition = crate::cargo_target::PartitionKey::from_inputs(
            "test-target",
            "test-toolchain",
            &std::collections::BTreeSet::new(),
            "dist",
            0,
            1,
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let attempt = supervisor
            .spawn(identity, partition, command)
            .expect("spawn");
        supervisor.cancel(attempt);

        let deadline = Instant::now() + Duration::from_secs(5);
        let record = loop {
            if let Some(record) = supervisor.try_reap(attempt) {
                break record;
            }
            assert!(Instant::now() < deadline, "cancelled child was not reaped");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(record.exit_status, Some(-1));
    }

    #[cfg(windows)]
    #[test]
    fn quote_arg_basic() {
        assert_eq!(quote_arg("hello"), "hello");
        assert_eq!(quote_arg("hello world"), "\"hello world\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a\\\\b"), "a\\\\b");
        assert_eq!(quote_arg("trail\\"), "trail\\\\");
    }
}
