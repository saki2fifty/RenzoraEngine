//! `AttemptOutcome` — the per-attempt terminal state, distinct from the
//! per-request `BuildOutcome`. See `phase2-cached-compiler-design.md` §3.1.

/// What happened to one cargo attempt. Process-local — never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttemptOutcome {
    /// Cargo exited zero. Staging → publication succeeded.
    Success,
    /// Cargo exited non-zero with a compile-error diagnosis. NOT transient —
    /// the source has a syntax/type/link error.
    CompileError,
    /// I/O failure, AV interference, network drop — transient.
    /// The scheduler applies jittered bounded backoff and re-enqueues
    /// when eligible.
    TransientInfrastructure,
    /// Cancelled by project close or explicit user retry.
    Cancelled,
    /// Editor shutdown before the attempt completed.
    Shutdown,
}

impl AttemptOutcome {
    /// Whether this outcome leaves the source published generation active.
    /// Compile errors leave the previously published gen intact.
    pub fn leaves_active_intact(&self) -> bool {
        matches!(
            self,
            AttemptOutcome::CompileError | AttemptOutcome::TransientInfrastructure | AttemptOutcome::Cancelled | AttemptOutcome::Shutdown | AttemptOutcome::Success
        )
    }
}
