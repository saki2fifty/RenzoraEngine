//! Retire dispatchers without leaving per-reload systems or interned sets behind.

use super::*;
use bevy::ecs::schedule::{InternedScheduleLabel, ScheduleCleanupPolicy};
use bevy::ecs::system::ScheduleSystem;
use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct AttemptSet(u64);

struct Attempt {
    slot: usize,
    generation: u32,
    cancelled: Arc<AtomicBool>,
    schedules: HashSet<InternedScheduleLabel>,
    pending: Vec<(InternedScheduleLabel, ScheduleSystem)>,
}

#[derive(Resource, Default)]
struct SystemLedger {
    dirty: bool,
    next: u64,
    free: Vec<AttemptSet>,
    attempts: HashMap<AttemptSet, Attempt>,
}

pub(super) fn begin(
    world: &mut World,
    slot: usize,
    generation: u32,
) -> (AttemptSet, Arc<AtomicBool>) {
    let mut ledger = world.get_resource_or_insert_with(SystemLedger::default);
    ledger.dirty = true;
    // Bevy interns SystemSets for the process lifetime. Recycle keys only after
    // all systems are removed, rather than interning every reload. Empty sets
    // stay in schedules as a bounded reusable pool.
    let set = ledger.free.pop().unwrap_or_else(|| {
        let set = AttemptSet(ledger.next);
        ledger.next = ledger
            .next
            .checked_add(1)
            .expect("plugin attempt set space exhausted");
        set
    });
    let cancelled = Arc::new(AtomicBool::new(false));
    ledger.attempts.insert(
        set,
        Attempt {
            slot,
            generation,
            cancelled: cancelled.clone(),
            schedules: HashSet::new(),
            pending: Vec::new(),
        },
    );
    (set, cancelled)
}

pub(super) fn register(
    world: &mut World,
    set: AttemptSet,
    label: InternedScheduleLabel,
    system: impl System<In = (), Out = ()>,
) {
    let active = world
        .resource::<Schedules>()
        .get_temporarily_removed()
        .contains(&label);
    let mut ledger = world.resource_mut::<SystemLedger>();
    let attempt = ledger
        .attempts
        .get_mut(&set)
        .expect("init owns a live attempt");
    attempt.schedules.insert(label);
    if active {
        // Inserting a replacement for an active schedule loses the new systems
        // when World::schedule_scope restores the original. Wait for its return.
        attempt.pending.push((label, Box::new(system)));
    } else {
        world
            .resource_mut::<Schedules>()
            .entry(label)
            .add_systems(system.in_set(set));
    }
}

pub(super) fn finish(world: &mut World, set: AttemptSet, success: bool) {
    world.resource_mut::<SystemLedger>().dirty = true;
    if !success {
        if let Some(attempt) = world.resource_mut::<SystemLedger>().attempts.get_mut(&set) {
            attempt.cancelled.store(true, Ordering::Relaxed);
        }
    }
    flush(world);
}

pub(super) fn retire(world: &mut World, slot: usize, generation: u32) {
    if let Some(mut ledger) = world.get_resource_mut::<SystemLedger>() {
        ledger.dirty = true;
        for attempt in ledger.attempts.values() {
            if attempt.slot == slot && attempt.generation == generation {
                // Cancellation is irrevocable even if a failed candidate's
                // generation number is reused by a later successful attempt.
                attempt.cancelled.store(true, Ordering::Relaxed);
            }
        }
    }
    flush(world);
}

pub(super) fn flush(world: &mut World) {
    if world
        .get_resource::<SystemLedger>()
        .is_none_or(|ledger| !ledger.dirty)
        || !world.contains_resource::<Schedules>()
    {
        return;
    }
    world.resource_scope(|world, mut ledger: Mut<SystemLedger>| {
        let active = world.resource::<Schedules>().get_temporarily_removed();
        for (&set, attempt) in &mut ledger.attempts {
            let cancelled = attempt.cancelled.load(Ordering::Relaxed);
            let mut waiting = Vec::new();
            for (label, config) in attempt.pending.drain(..) {
                if cancelled {
                    // A pending registration never reached the schedule. Other
                    // systems in the attempt may already be there, so retain
                    // the label until the removal pass checks it.
                    drop(config);
                } else if active.contains(&label) {
                    waiting.push((label, config));
                } else {
                    world.resource_mut::<Schedules>().entry(label).add_systems(config.in_set(set));
                }
            }
            attempt.pending = waiting;
            if !cancelled { continue; }
            attempt.schedules.retain(|&label| {
                if active.contains(&label) { return true; }
                if !world.resource::<Schedules>().contains(label) { return false; }
                let result = world.try_schedule_scope(label, |world, schedule| {
                    remove_from_schedule(schedule, world, set)
                });
                match result {
                    Ok(Ok(_)) | Ok(Err(bevy::ecs::schedule::ScheduleError::SetNotFound)) => false,
                    other => {
                        warn!("[plugin] retaining cancelled system set {set:?} for cleanup retry: {other:?}");
                        true
                    }
                }
            });
        }
        let done: Vec<_> = ledger.attempts.iter()
            .filter(|(_, attempt)| attempt.schedules.is_empty() && attempt.pending.is_empty())
            .map(|(&set, _)| set).collect();
        for set in done {
            ledger.attempts.remove(&set);
            ledger.free.push(set);
        }
        ledger.dirty = ledger.attempts.values().any(|attempt| {
            !attempt.pending.is_empty() || attempt.cancelled.load(Ordering::Relaxed)
        });
    });
}

fn remove_from_schedule(
    schedule: &mut Schedule,
    world: &mut World,
    set: AttemptSet,
) -> Result<usize, bevy::ecs::schedule::ScheduleError> {
    let removed =
        schedule.remove_systems_in_set(set, world, ScheduleCleanupPolicy::RemoveSystemsOnly)?;
    // Rebuild now so the old executable drops its system boxes before recycling
    // the set key, even for seldom-run schedules.
    schedule.initialize(world)?;
    Ok(removed)
}

#[derive(Resource)]
struct MaintenanceInstalled;

/// Install bounded plugin-system cleanup and delayed active-schedule insertion.
/// Idempotent so hosts with both directory and loose plugins share one sweep.
pub fn install(app: &mut App) {
    if app.world().contains_resource::<MaintenanceInstalled>() {
        return;
    }
    app.insert_resource(MaintenanceInstalled)
        .add_systems(First, flush)
        .add_systems(Last, flush);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    unsafe extern "C" fn count(call: *const sys::SystemCall) -> sys::SystemStatus {
        // SAFETY: each test keeps the atomic probe alive until schedules drop.
        unsafe { &*((*call).user.cast::<AtomicUsize>()) }.fetch_add(1, Ordering::Relaxed);
        sys::SystemStatus::Ok
    }

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<Time>();
        world.init_resource::<Schedules>();
        world
    }

    fn attempt(
        world: &mut World,
        counter: &PluginGeneration,
        generation: u32,
        probe: &AtomicUsize,
    ) -> AttemptSet {
        let (set, cancelled) = begin(world, 0, generation);
        let system = build_dispatcher(
            world,
            vec![],
            vec![],
            count,
            probe as *const AtomicUsize as usize,
            GenGate {
                counter: counter.clone(),
                at: generation,
                cancelled,
            },
            0,
        );
        register(world, set, Update.intern(), system);
        set
    }

    #[test]
    fn thousand_reloads_keep_one_system_and_recycle_attempt_sets() {
        let mut world = world();
        let counter = PluginGeneration::default();
        let probe = AtomicUsize::new(0);
        for generation in 0..=1000 {
            let set = attempt(&mut world, &counter, generation, &probe);
            finish(&mut world, set, true);
            counter.store(generation, Ordering::Relaxed);
            if generation > 0 {
                retire(&mut world, 0, generation - 1);
            }
            world.run_schedule(Update);
            let ledger = world.resource::<SystemLedger>();
            assert_eq!(ledger.attempts.len(), 1);
            assert!(ledger.next <= 2, "set keys must not grow per reload");
            let count = world
                .resource::<Schedules>()
                .get(Update)
                .unwrap()
                .systems()
                .unwrap()
                .count();
            assert_eq!(count, 1);
            let schedule_sets = world.resource::<Schedules>().get(Update).unwrap()
                .graph().system_sets.len();
            assert!(schedule_sets <= 3, "private and implicit sets must stay bounded");
            if [0, 10, 100, 1000].contains(&generation) {
                println!("reloads={generation}: systems={count}, live_attempts={}, allocated_set_keys={}, schedule_sets={schedule_sets}", ledger.attempts.len(), ledger.next);
            }
        }
        assert_eq!(probe.load(Ordering::Relaxed), 1001);
        let allocations = crate::host::dispatch_tests::allocations_during(|| {
            for _ in 0..1000 {
                flush(&mut world);
            }
        });
        assert_eq!(allocations, 0, "idle maintenance must not allocate");
    }

    #[test]
    fn active_schedule_registration_survives_and_failed_generation_never_revives() {
        let mut world = world();
        let counter = PluginGeneration::default();
        let failed_calls = AtomicUsize::new(0);
        let good_calls = AtomicUsize::new(0);
        let failed = attempt(&mut world, &counter, 1, &failed_calls);
        world.schedule_scope(Update, |world, schedule| {
            // Failed candidate is already installed in the active schedule;
            // cleanup cannot remove it until this scope returns.
            finish(world, failed, false);
            let good = attempt(world, &counter, 1, &good_calls);
            assert_ne!(failed, good, "pending removal keys cannot be recycled");
            finish(world, good, true);
            counter.store(1, Ordering::Relaxed);
            schedule.run(world);
            assert_eq!(failed_calls.load(Ordering::Relaxed), 0);
            assert_eq!(good_calls.load(Ordering::Relaxed), 0);
            assert!(
                !world.resource::<Schedules>().contains(Update),
                "must not overwrite active schedule"
            );
        });
        flush(&mut world);
        world.run_schedule(Update);
        assert_eq!(failed_calls.load(Ordering::Relaxed), 0);
        assert_eq!(good_calls.load(Ordering::Relaxed), 1);
        assert_eq!(world.resource::<SystemLedger>().attempts.len(), 1);
        assert_eq!(
            world
                .resource::<Schedules>()
                .get(Update)
                .unwrap()
                .systems()
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn failed_pending_registration_is_discarded_and_existing_system_survives() {
        let mut world = world();
        let counter = PluginGeneration::default();
        let good_calls = AtomicUsize::new(0);
        let failed_calls = AtomicUsize::new(0);
        let good = attempt(&mut world, &counter, 0, &good_calls);
        finish(&mut world, good, true);
        world.schedule_scope(Update, |world, schedule| {
            let failed = attempt(world, &counter, 1, &failed_calls);
            finish(world, failed, false);
            schedule.run(world);
        });
        flush(&mut world);
        world.run_schedule(Update);
        assert_eq!(good_calls.load(Ordering::Relaxed), 2);
        assert_eq!(failed_calls.load(Ordering::Relaxed), 0);
        assert_eq!(world.resource::<SystemLedger>().attempts.len(), 1);
    }

    #[test]
    fn retiring_drops_owned_system_memory_without_another_schedule_run() {
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut world = world();
        for generation in 0..100 {
            let (set, _) = begin(&mut world, 0, generation);
            let owned = DropProbe(dropped.clone());
            let system = IntoSystem::into_system(move || {
                std::hint::black_box(&owned);
            });
            register(&mut world, set, Update.intern(), system);
            finish(&mut world, set, true);
            retire(&mut world, 0, generation);
            assert_eq!(dropped.load(Ordering::Relaxed), generation as usize + 1);
        }
        assert_eq!(world.resource::<SystemLedger>().next, 1);
    }

    #[test]
    fn installed_maintenance_completes_active_preupdate_registration() {
        #[derive(Resource, Default)]
        struct Probe(Arc<AtomicUsize>, bool);
        fn activate(world: &mut World) {
            if world.resource::<Probe>().1 {
                return;
            }
            let probe = world.resource::<Probe>().0.clone();
            let (set, cancelled) = begin(world, 0, 0);
            let system = build_dispatcher(
                world,
                vec![],
                vec![],
                count,
                probe.as_ref() as *const AtomicUsize as usize,
                GenGate {
                    counter: Default::default(),
                    at: 0,
                    cancelled,
                },
                0,
            );
            register(world, set, PreUpdate.intern(), system);
            finish(world, set, true);
            world.resource_mut::<Probe>().1 = true;
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).init_resource::<Probe>();
        install(&mut app);
        install(&mut app);
        app.add_systems(PreUpdate, activate);
        app.update();
        assert_eq!(app.world().resource::<Probe>().0.load(Ordering::Relaxed), 0);
        app.update();
        assert_eq!(app.world().resource::<Probe>().0.load(Ordering::Relaxed), 1);
        app.update();
        assert_eq!(app.world().resource::<Probe>().0.load(Ordering::Relaxed), 2);
        assert!(!app.world().resource::<SystemLedger>().dirty);
    }
}
