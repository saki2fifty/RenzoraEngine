//! Process lifetime guard for an explicitly approved editor restart.

use std::ffi::OsString;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use renzora::{spawn_replacement_process, EnginePluginGenerationStamp};

use crate::load_candidate_generation;
use crate::startup::{PendingStartup, StartupError, StartupSelection};

/// A replacement either remains pending or has acknowledged readiness.
#[derive(Debug)]
pub enum ReplacementProgress {
    /// Keep the original editor alive while the candidate starts.
    Pending,
    /// Startup was acknowledged and recovery selection committed.
    Ready(StartupSelection),
}

/// A launched candidate that cannot silently outlive a failed handoff.
///
/// Own this on a worker: generation verification and process reaping may block.
/// The caller must obtain unsaved-work consent before launching, and must not
/// exit the original editor on `Pending` or any error.
pub struct PendingReplacement {
    child: Option<Child>,
    startup: PendingStartup,
    deadline: Instant,
    accepted: bool,
}

impl PendingReplacement {
    /// Validate and launch the selected executable with caller-defined OS arguments.
    pub fn launch(
        cache: &Path,
        generation: u64,
        stamp: &EnginePluginGenerationStamp,
        timeout: Duration,
        arguments: impl FnOnce(&Path, &str) -> Vec<OsString>,
    ) -> Result<Self, StartupError> {
        if timeout.is_zero() {
            return Err(StartupError::Invalid(
                "startup timeout must be positive".into(),
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| StartupError::Invalid("startup timeout overflows".into()))?;
        let startup = PendingStartup::begin(cache, generation, stamp)?;
        let selected = load_candidate_generation(cache)?;
        if selected.manifest.generation != generation || &selected.manifest.stamp != stamp {
            return Err(StartupError::Invalid(
                "candidate changed before launch".into(),
            ));
        }
        let editor = selected
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.role == "editor")
            .ok_or_else(|| StartupError::Invalid("candidate has no editor".into()))?;
        let cache = std::fs::canonicalize(cache)?;
        let child = spawn_replacement_process(
            &selected.root.join(&editor.file),
            arguments(&cache, startup.token()),
        )?;
        Ok(Self {
            child: Some(child),
            startup,
            deadline,
            accepted: false,
        })
    }

    /// Poll liveness and deadline before accepting the matching readiness record.
    pub fn poll(&mut self) -> Result<ReplacementProgress, StartupError> {
        if self.accepted {
            return Err(StartupError::Invalid("replacement already accepted".into()));
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| StartupError::Invalid("replacement process was released".into()))?;
        check_health(child, self.deadline)?;
        let deadline = self.deadline;
        match self
            .startup
            .try_confirm_if(|| check_health(child, deadline))?
        {
            Some(selection) => {
                self.accepted = true;
                Ok(ReplacementProgress::Ready(selection))
            }
            None => Ok(ReplacementProgress::Pending),
        }
    }

    /// Transfer an acknowledged child to the launcher for eventual reaping.
    pub fn finish(mut self) -> Result<Child, StartupError> {
        self.try_finish()
    }

    /// Recheck liveness without dropping the guard on failure.
    /// A frame-schedule caller can send a failed guard back to a cleanup worker.
    pub fn try_finish(&mut self) -> Result<Child, StartupError> {
        if !self.accepted {
            return Err(StartupError::Invalid("replacement is not ready".into()));
        }
        if let Some(child) = self.child.as_mut() {
            check_health(child, self.deadline)?;
        }
        self.child
            .take()
            .ok_or_else(|| StartupError::Invalid("replacement was already released".into()))
    }

    /// Stop a candidate and restore the old selection before final handoff.
    /// This also covers new unsaved edits discovered after acknowledgement.
    pub fn cancel(mut self) -> Result<(), StartupError> {
        self.stop()?;
        self.startup.restore_previous()?;
        self.accepted = false;
        Ok(())
    }

    fn stop(&mut self) -> std::io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            if let Err(error) = child.kill() {
                // The process can exit between try_wait and kill.
                if child.try_wait()?.is_none() {
                    return Err(error);
                }
            }
            child.wait()?;
        }
        Ok(())
    }
}

fn check_health(child: &mut Child, deadline: Instant) -> Result<(), StartupError> {
    if let Some(status) = child.try_wait()? {
        return Err(StartupError::Invalid(format!(
            "replacement exited before readiness: {status}"
        )));
    }
    if Instant::now() >= deadline {
        return Err(StartupError::Invalid(
            "replacement startup timed out; keep the current editor".into(),
        ));
    }
    Ok(())
}

impl Drop for PendingReplacement {
    fn drop(&mut self) {
        // Readiness is not the final handoff. Unless finish transferred the
        // child, abandonment must keep the old editor and recovery selection.
        if self.child.is_some() {
            let _ = self.stop();
            let _ = self.startup.restore_previous();
        }
    }
}
