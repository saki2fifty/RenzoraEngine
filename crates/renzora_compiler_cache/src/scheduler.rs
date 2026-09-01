//! Single-lock `ReadyScheduler` with `Condvar` predicate loop and atomic
//! ready-to-in-flight leasing.
//!
//! See `phase2-cached-compiler-design.md` §3.3 / §3.4 / §3.5 for the
//! invariants and §10.3 for the coalescing rules.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use renzora_identity::CanonicalId;

/// Scheduler configuration.
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Max time a `wait_for_idle` call will block before returning. Tests use
    /// small values (1 ms) to drive the predicate loop deterministically.
    pub wait_park_interval: Duration,
    /// Whether the scheduler has been stopped (set on `stop()`). Useful for
    /// exponential backoff tests that need to wake the scheduler without an
    /// enqueue.
    pub wake_on_stop: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            wait_park_interval: Duration::from_millis(10),
            wake_on_stop: true,
        }
    }
}

/// The single mutable state protected by one mutex. See §3.3 of the design.
pub(crate) struct SchedulerState {
    pub(crate) ready_queue: VecDeque<CanonicalId>,
    pub(crate) ready_set: HashSet<CanonicalId>,
    pub(crate) in_flight: HashSet<CanonicalId>,
    pub(crate) stopped: bool,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
            ready_set: HashSet::new(),
            in_flight: HashSet::new(),
            stopped: false,
        }
    }
}

/// The single-lock scheduler. One `Mutex` owns every membership transition.
/// The `Condvar` releases blocked dequeuers whenever enqueue, finish, or stop
/// mutates state.
pub struct ReadyScheduler {
    state: Mutex<SchedulerState>,
    wake: Condvar,
    config: SchedulerConfig,
}

impl ReadyScheduler {
    /// Construct a new scheduler.
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            state: Mutex::new(SchedulerState::new()),
            wake: Condvar::new(),
            config,
        }
    }

    /// Enqueue an id. Idempotent: an id already in `ready_set` or
    /// `in_flight` is left untouched. Always `notify_one` after a successful
    /// insert so a blocked dequeuer wakes.
    pub fn enqueue(&self, id: CanonicalId) {
        let mut state = self.state.lock();
        if state.stopped {
            return;
        }
        if state.ready_set.contains(&id) || state.in_flight.contains(&id) {
            return;
        }
        state.ready_set.insert(id.clone());
        state.ready_queue.push_back(id);
        self.wake.notify_one();
    }

    /// Atomically lease the next ready id: move it from `ready_set` /
    /// Atomically lease the next ready id: move it from `ready_set` /
    /// `ready_queue` to `in_flight`. Returns `None` if the scheduler has been
    /// stopped and the queue is empty.
    pub fn lease_next(&self) -> LeaseGuard<'_> {
        let mut state = self.state.lock();
        loop {
            if state.stopped && state.ready_queue.is_empty() {
                return LeaseGuard {
                    scheduler: self,
                    id: None,
                };
            }
            if let Some(id) = state.ready_queue.pop_front() {
                state.ready_set.remove(&id);
                let in_flight_clone = id.clone();
                state.in_flight.insert(id.clone());
                drop(state);
                return LeaseGuard {
                    scheduler: self,
                    id: Some(in_flight_clone),
                };
            }
            self.wake.wait(&mut state);
        }
    }

    /// Same as `lease_next` but with a timeout. Returns `None` if the
    /// scheduler remained empty for the entire `timeout`.
    pub fn lease_next_timeout(&self, timeout: Duration) -> Option<LeaseGuard<'_>> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        loop {
            if let Some(id) = state.ready_queue.pop_front() {
                state.ready_set.remove(&id);
                state.in_flight.insert(id.clone());
                return Some(LeaseGuard {
                    scheduler: self,
                    id: Some(id),
                });
            }
            if state.stopped {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let interval = self.config.wait_park_interval.min(remaining);
            self.wake.wait_for(&mut state, interval);
        }
    }

    /// Finish an attempt. `needs_follow_up` is true iff the latest revision
    /// advanced past `completed_revision` (success path with newer edits in
    /// flight); the id is moved directly to `ready_set`.
    pub fn finish(&self, id: &CanonicalId, needs_follow_up: bool) {
        let mut state = self.state.lock();
        state.in_flight.remove(id);
        if needs_follow_up && !state.stopped && !state.ready_set.contains(id) {
            state.ready_set.insert(id.clone());
            state.ready_queue.push_back(id.clone());
        }
        self.wake.notify_one();
    }

    /// Remove an id from ready / in-flight membership (e.g. on project close).
    pub fn remove(&self, id: &CanonicalId) {
        let mut state = self.state.lock();
        state.ready_set.remove(id);
        state.ready_queue.retain(|q| q != id);
        state.in_flight.remove(id);
        self.wake.notify_one();
    }

    /// Stop the scheduler. Every blocking `lease_next` returns `None` once
    /// the queue drains.
    pub fn stop(&self) {
        let mut state = self.state.lock();
        state.stopped = true;
        self.wake.notify_all();
    }

    /// Current queue size (for diagnostics).
    pub fn queue_len(&self) -> usize {
        self.state.lock().ready_queue.len()
    }

    /// Current in-flight size.
    pub fn in_flight_len(&self) -> usize {
        self.state.lock().in_flight.len()
    }

    /// Whether `id` is currently ready or in-flight (test / diagnostics).
    pub fn contains(&self, id: &CanonicalId) -> bool {
        let state = self.state.lock();
        state.ready_set.contains(id) || state.in_flight.contains(id)
    }

    /// Snapshot of ready + in-flight membership (test / invariants).
    pub fn membership_snapshot(&self) -> (Vec<CanonicalId>, Vec<CanonicalId>) {
        let state = self.state.lock();
        let mut ready: Vec<CanonicalId> = state.ready_queue.iter().cloned().collect();
        let mut in_flight: Vec<CanonicalId> = state.in_flight.iter().cloned().collect();
        ready.sort();
        in_flight.sort();
        (ready, in_flight)
    }

    /// Notify one blocked dequeuer (test helper for "wake without an
    /// enqueue"). Used by T2.14 to prove spurious notifications don't pop an
    /// empty queue.
    pub fn poke(&self) {
        self.wake.notify_one();
    }
}

/// A lease returned by `lease_next`. Holds the id in `in_flight` until
/// dropped; the `Drop` impl removes the id from `in_flight` (the worker's
/// `finish` call does this explicitly, but the Drop is a safety net in case
/// the worker panics).
pub struct LeaseGuard<'a> {
    scheduler: &'a ReadyScheduler,
    id: Option<CanonicalId>,
}

impl LeaseGuard<'_> {
    /// The leased id, or `None` if the scheduler has been stopped.
    pub fn id(&self) -> Option<&CanonicalId> {
        self.id.as_ref()
    }
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut state = self.scheduler.state.lock();
            state.in_flight.remove(&id);
            self.scheduler.wake.notify_one();
        }
    }
}

/// Convenience wrapper for sharing the scheduler across the worker pool.
pub type SharedScheduler = Arc<ReadyScheduler>;

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> CanonicalId {
        CanonicalId::parse(&format!("project://{s}")).unwrap()
    }

    #[test]
    fn enqueue_is_idempotent() {
        let sched = ReadyScheduler::new(SchedulerConfig::default());
        let a = id("a.rs");
        sched.enqueue(a.clone());
        sched.enqueue(a.clone());
        sched.enqueue(a.clone());
        let (ready, in_flight) = sched.membership_snapshot();
        assert_eq!(ready.len(), 1, "queue membership is {ready:?}");
        assert_eq!(in_flight.len(), 0);
    }

    #[test]
    fn lease_moves_id_to_in_flight() {
        let sched = ReadyScheduler::new(SchedulerConfig::default());
        let a = id("a.rs");
        sched.enqueue(a.clone());
        let lease = sched.lease_next_timeout(Duration::from_millis(10)).unwrap();
        assert_eq!(lease.id(), Some(&a));
        let (ready, in_flight) = sched.membership_snapshot();
        assert!(ready.is_empty());
        assert_eq!(in_flight, vec![a.clone()]);
        drop(lease);
        let (ready, in_flight) = sched.membership_snapshot();
        assert!(ready.is_empty());
        assert!(in_flight.is_empty());
    }

    #[test]
    fn stop_unblocks_dequeuers() {
        let sched = Arc::new(ReadyScheduler::new(SchedulerConfig::default()));
        let sched2 = sched.clone();
        let handle = std::thread::spawn(move || {
            let lease = sched2.lease_next_timeout(Duration::from_secs(1));
            assert!(lease.is_none(), "stopped scheduler with empty queue returns None");
        });
        std::thread::sleep(Duration::from_millis(20));
        sched.stop();
        handle.join().unwrap();
    }
}
