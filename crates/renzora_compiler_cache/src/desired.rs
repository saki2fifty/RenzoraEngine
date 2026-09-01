//! Per-`CanonicalId` desired-revision store.
//!
//! Holds the latest user-visible source state plus the
//! `last_attempted_revision` / `last_attempt_outcome` / `transient_retry_at`
//! fields the scheduler needs to enforce the quiescent-failure rule
//! (R2-2). See `phase2-cached-compiler-design.md` §3.1 / §3.2 / §4.3.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use renzora_identity::CanonicalId;

use crate::status::AttemptOutcome;
use crate::types::{ArtifactKind, Revision};

/// Per-`CanonicalId` state. See `phase2-cached-compiler-design.md` §3.1.
#[derive(Debug)]
pub struct DesiredRecord {
    /// Latest user-visible source state. Always advances on edit.
    pub latest_revision: Revision,
    /// R6-3: the request id assigned by `BuildService::submit` for the
    /// latest revision. The worker uses this to stamp `AttemptCompletion`s
    /// so the dispatcher routes back to the exact receiver.
    pub latest_request_id: u64,
    /// Source bytes at the latest revision.
    pub latest_snapshot: Arc<Vec<u8>>,
    /// Non-source fingerprint inputs at the latest revision.
    pub latest_inputs: Arc<crate::types::FingerprintInputs>,
    /// What kind of artifact the request asks for.
    pub latest_artifact_kind: ArtifactKind,
    /// Highest revision whose build finished successfully.
    pub completed_revision: Option<Revision>,
    /// The attempt currently being compiled, if any.
    pub active_attempt: Option<u64>,
    /// The revision of the most recent attempt, success or failure.
    pub last_attempted_revision: Option<Revision>,
    /// The outcome of the most recent attempt.
    pub last_attempt_outcome: Option<AttemptOutcome>,
    /// Earliest time a transient-failed revision may be retried (jittered
    /// bounded backoff).
    pub transient_retry_at: Option<Instant>,
    /// An explicit user "retry" action has been registered against
    /// `last_attempted_revision`.
    pub user_retry_pending: bool,
    /// Number of consecutive transient failures against this revision.
    /// Caps the retry loop at 5 attempts; the 6th transient is reclassified
    /// as `CompileFailed`.
    pub transient_attempts: u32,
}

/// The desired-revision store. Pure data — no locks on its own; the
/// scheduler's `SchedulerState` mutex protects transitions that move
/// ready/in-flight membership, and the store itself uses a `parking_lot::Mutex`
/// (cheap, no poisoning) for per-record mutations.
#[derive(Debug, Default)]
pub struct DesiredStore {
    records: Mutex<HashMap<CanonicalId, DesiredRecord>>,
    next_revision: AtomicU64,
    next_attempt: AtomicU64,
}

impl DesiredStore {
    /// Create an empty desired store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh `Revision` for a new edit.
    pub fn next_revision(&self) -> Revision {
        let r = self.next_revision.fetch_add(1, Ordering::Relaxed) + 1;
        Revision(r)
    }

    /// Allocate a fresh process-unique `AttemptId`. Never persisted.
    pub fn next_attempt_id(&self) -> u64 {
        self.next_attempt.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Record an edit to `id`. If the id is new, a fresh revision (>=1) is
    /// allocated. The snapshot is always replaced with the latest bytes.
    ///
    /// The caller is responsible for calling
    /// `scheduler.enqueue(id)` afterwards if `latest_revision > completed_revision`.
    pub fn record_edit(
        &self,
        id: CanonicalId,
        request_id: u64,
        snapshot: Arc<Vec<u8>>,
        inputs: Arc<crate::types::FingerprintInputs>,
        artifact_kind: ArtifactKind,
    ) -> Revision {
        let mut guard = self.records.lock();
        let entry = guard.entry(id.clone()).or_insert_with(|| DesiredRecord {
            latest_revision: Revision(0),
            latest_request_id: 0,
            latest_snapshot: snapshot.clone(),
            latest_inputs: inputs.clone(),
            latest_artifact_kind: artifact_kind,
            completed_revision: None,
            active_attempt: None,
            last_attempted_revision: None,
            last_attempt_outcome: None,
            transient_retry_at: None,
            user_retry_pending: false,
            transient_attempts: 0,
        });
        // Bump the revision. Always strictly greater than previous.
        let next = Revision(entry.latest_revision.0 + 1);
        entry.latest_revision = next;
        entry.latest_request_id = request_id;
        entry.latest_snapshot = snapshot;
        entry.latest_inputs = inputs;
        entry.latest_artifact_kind = artifact_kind;
        // A new edit clears the transient retry counter (the new revision
        // is a fresh attempt).
        entry.transient_attempts = 0;
        next
    }

    /// Return the request id assigned for the LATEST revision of `id`
    /// (R6-3). The worker uses this to stamp `AttemptCompletion`s.
    pub fn latest_request_id(&self, id: &CanonicalId) -> Option<u64> {
        self.records.lock().get(id).map(|r| r.latest_request_id)
    }

    /// Look up the latest revision for `id`. `None` if the id has never been
    /// recorded.
    pub fn latest_revision(&self, id: &CanonicalId) -> Option<Revision> {
        self.records.lock().get(id).map(|r| r.latest_revision)
    }

    /// Snapshot of (latest_revision, latest_snapshot, latest_inputs, latest_artifact_kind).
    pub fn snapshot(
        &self,
        id: &CanonicalId,
    ) -> Option<(Revision, Arc<Vec<u8>>, Arc<crate::types::FingerprintInputs>, ArtifactKind)> {
        let guard = self.records.lock();
        guard.get(id).map(|r| (r.latest_revision, r.latest_snapshot.clone(), r.latest_inputs.clone(), r.latest_artifact_kind))
    }

    /// Mark `id` as currently being compiled by `attempt`.
    pub fn record_attempt(&self, id: &CanonicalId, attempt: u64, scheduled_revision: Revision) {
        let mut guard = self.records.lock();
        if let Some(r) = guard.get_mut(id) {
            r.active_attempt = Some(attempt);
            r.last_attempted_revision = Some(scheduled_revision);
        }
    }

    /// Record the outcome of a finished attempt and update completed_revision.
    pub fn record_outcome(
        &self,
        id: &CanonicalId,
        outcome: AttemptOutcome,
        scheduled_revision: Revision,
        transient_retry_at: Option<Instant>,
    ) {
        let mut guard = self.records.lock();
        let Some(r) = guard.get_mut(id) else { return };
        r.active_attempt = None;
        r.last_attempt_outcome = Some(outcome);
        r.transient_retry_at = transient_retry_at;
        if matches!(outcome, AttemptOutcome::Success) {
            if r.completed_revision.is_none_or(|c| scheduled_revision.0 > c.0) {
                r.completed_revision = Some(scheduled_revision);
            }
            r.transient_retry_at = None;
            r.user_retry_pending = false;
        }
    }

    /// Record an explicit user retry. Resets the transient backoff and sets
    /// the user-retry flag.
    pub fn record_user_retry(&self, id: &CanonicalId) {
        let mut guard = self.records.lock();
        if let Some(r) = guard.get_mut(id) {
            r.user_retry_pending = true;
            r.transient_retry_at = None;
        }
    }

    /// Clear the user-retry flag (after the scheduler has dequeued).
    pub fn clear_user_retry(&self, id: &CanonicalId) {
        let mut guard = self.records.lock();
        if let Some(r) = guard.get_mut(id) {
            r.user_retry_pending = false;
        }
    }

    /// Remove a record entirely (e.g. on project close).
    pub fn remove(&self, id: &CanonicalId) {
        self.records.lock().remove(id);
    }

    /// True when the id is currently being compiled by a worker.
    pub fn is_in_flight(&self, id: &CanonicalId) -> bool {
        self.records
            .lock()
            .get(id)
            .and_then(|r| r.active_attempt)
            .is_some()
    }

    /// Return ids whose transient_retry_at has elapsed AND whose
    /// `last_attempted_revision == latest_revision` (i.e. the user has not
    /// edited since the transient failure). These are eligible for re-enqueue.
    /// The map key IS the canonical id; we never reconstruct it from the
    /// snapshot.
    pub fn transient_eligible_now(&self) -> Vec<(CanonicalId, Revision)> {
        let now = Instant::now();
        let guard = self.records.lock();
        guard
            .iter()
            .filter_map(|(id, r)| {
                r.transient_retry_at
                    .filter(|t| *t <= now)
                    .and_then(|_| {
                        let last_attempted = r.last_attempted_revision?;
                        if last_attempted == r.latest_revision
                            && matches!(
                                r.last_attempt_outcome,
                                Some(AttemptOutcome::TransientInfrastructure)
                            )
                        {
                            Some((id.clone(), r.latest_revision))
                        } else {
                            None
                        }
                    })
            })
            .collect()
    }

    /// Record a transient retry outcome AND track the attempt count so the
    /// 5-attempt cap can be enforced. Returns the attempt count after
    /// recording (>= 1). Cap at 5: the next transient thereafter is
    /// reclassified as a CompileFailed outcome by the worker.
    pub fn record_transient_outcome(
        &self,
        id: &CanonicalId,
        scheduled_revision: Revision,
        backoff: std::time::Duration,
    ) -> u32 {
        let mut guard = self.records.lock();
        let Some(r) = guard.get_mut(id) else { return 0 };
        r.active_attempt = None;
        r.last_attempt_outcome = Some(AttemptOutcome::TransientInfrastructure);
        r.last_attempted_revision = Some(scheduled_revision);
        r.transient_retry_at = Some(Instant::now() + backoff);
        // Count: increment a counter stored inside the record.
        let count = r.transient_attempts.saturating_add(1);
        r.transient_attempts = count;
        count
    }

    /// Snapshot of the current per-id records (for tests and reporting).
    pub fn records(&self) -> Vec<(CanonicalId, Revision)> {
        let guard = self.records.lock();
        let mut out: Vec<_> = guard.iter().map(|(k, v)| (k.clone(), v.latest_revision)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}
