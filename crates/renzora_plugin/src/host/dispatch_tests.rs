use super::*;

use std::alloc::{GlobalAlloc, System};
use std::cell::Cell;

thread_local! {
    // Per-thread and const-initialized so parallel tests do not contaminate the
    // measurement, and observing an allocation never allocates recursively.
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(current) = count.get() {
            count.set(Some(current + 1));
        }
    });
}

// SAFETY: every operation forwards the original allocation contract unchanged
// to System. Thread-local counters never inspect or modify allocated memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: ptr/layout/size are forwarded unchanged to their allocator.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: all allocations above originate from System.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

pub(super) fn allocations_during(run: impl FnOnce()) -> usize {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.with(|count| count.set(None));
        }
    }
    ALLOCATIONS.with(|count| count.set(Some(0)));
    let reset = Reset;
    run();
    let count = ALLOCATIONS.with(|count| count.get().expect("measurement enabled"));
    drop(reset);
    count
}

#[derive(Component)]
#[repr(C)]
struct Counter(u32);

fn raw_plan(world: &mut World) -> TermPlan {
    TermPlan {
        id: world.register_component::<Counter>(),
        access: sys::Access::ReadOptional,
        marshal: Marshal::Raw,
        cell_size: size_of::<Counter>(),
    }
}

#[test]
fn raw_cells_append_without_replacing_the_destination() {
    let mut world = World::new();
    let plan = raw_plan(&mut world);
    let entity = world.spawn(Counter(42)).id();
    let entity: FilteredEntityRef = world.entity(entity).into();
    let mut bytes = Vec::with_capacity(4_000);
    let allocation = bytes.as_ptr();
    ALLOCATIONS.with(|count| count.set(Some(0)));
    for _ in 0..1_000 {
        assert!(append_cell(&entity, &plan, &mut bytes));
    }
    let allocations = ALLOCATIONS.with(|count| count.replace(None));
    assert_eq!(allocations, Some(0), "warm cell copies allocate nothing");
    assert_eq!(bytes.len(), 4_000);
    assert_eq!(bytes.as_ptr(), allocation);
    for cell in bytes.chunks_exact(4) {
        assert_eq!(cell, 42_u32.to_ne_bytes());
    }
}

#[test]
fn missing_optional_cell_leaves_destination_unchanged() {
    let mut world = World::new();
    let plan = raw_plan(&mut world);
    let entity = world.spawn_empty().id();
    let entity: FilteredEntityRef = world.entity(entity).into();
    let mut bytes = vec![1, 2, 3];
    assert!(!append_cell(&entity, &plan, &mut bytes));
    assert_eq!(bytes, [1, 2, 3]);
}

#[test]
fn transform_cell_still_uses_the_guest_mirror_layout() {
    let mut world = World::new();
    let plan = TermPlan {
        id: world.register_component::<Transform>(),
        access: sys::Access::Read,
        marshal: Marshal::Transform,
        cell_size: size_of::<sys::Transform>(),
    };
    let transform = Transform::from_xyz(2.0, 3.0, 4.0);
    let expected = to_mirror(&transform);
    let entity = world.spawn(transform).id();
    let entity: FilteredEntityRef = world.entity(entity).into();
    let mut bytes = Vec::with_capacity(plan.cell_size);
    assert!(append_cell(&entity, &plan, &mut bytes));
    // SAFETY: expected is the initialized, plain C-ABI mirror, not Bevy layout.
    let expected = unsafe {
        std::slice::from_raw_parts(
            (&expected as *const sys::Transform).cast::<u8>(),
            size_of::<sys::Transform>(),
        )
    };
    assert_eq!(bytes, expected);
}

#[test]
fn clearing_rows_retains_owned_capacity_without_stale_data() {
    let mut world = World::new();
    let mut state = ViewState::new(vec![raw_plan(&mut world)], Vec::new());
    state.staging[0].extend_from_slice(&[1; 128]);
    state.baseline[0].extend_from_slice(&[1; 128]);
    state.present[0].extend([true; 32]);
    state.entities.extend((0..32).map(sys::Entity));
    state.kept.extend([true; 32]);
    let capacities = (
        state.staging[0].capacity(),
        state.baseline[0].capacity(),
        state.present[0].capacity(),
        state.entities.capacity(),
        state.kept.capacity(),
    );
    state.clear_rows();
    assert!(state.staging[0].is_empty());
    assert!(state.baseline[0].is_empty());
    assert!(state.present[0].is_empty());
    assert!(state.entities.is_empty());
    assert!(state.kept.is_empty());
    assert_eq!(
        capacities,
        (
            state.staging[0].capacity(),
            state.baseline[0].capacity(),
            state.present[0].capacity(),
            state.entities.capacity(),
            state.kept.capacity(),
        )
    );
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ViewState>();
}

#[test]
fn real_dispatcher_handles_empty_and_smaller_frames_after_warmup() {
    unsafe extern "C" fn increment(call: *const sys::SystemCall) -> sys::SystemStatus {
        // SAFETY: this test installs one query and keeps observations alive
        // until after the schedule is dropped. All cells are writable u32s.
        unsafe {
            let call = &*call;
            let observations = &mut *(call.user as *mut Vec<usize>);
            let view = &*call.views;
            observations.push(view.entity_count);
            for row in 0..view.entity_count {
                let cell = (*view.cells.add(row)).cast::<u32>();
                cell.write_unaligned(cell.read_unaligned() + 1);
            }
        }
        sys::SystemStatus::Ok
    }

    let mut world = World::new();
    world.init_resource::<Time>();
    let mut plan = raw_plan(&mut world);
    plan.access = sys::Access::Write;
    let entities: Vec<_> = (0..4).map(|_| world.spawn(Counter(10)).id()).collect();
    let mut observations = Box::new(Vec::<usize>::new());
    let system = build_dispatcher(
        &mut world,
        vec![vec![plan]],
        Vec::new(),
        increment,
        observations.as_mut() as *mut Vec<usize> as usize,
        GenGate {
            counter: Default::default(),
            at: 0,
            cancelled: Default::default(),
        },
        sys::system_services::ALL,
    );
    let mut schedule = Schedule::default();
    schedule.add_systems(system);
    schedule.run(&mut world);
    for entity in entities {
        assert_eq!(world.get::<Counter>(entity).expect("counter").0, 11);
        world.despawn(entity);
    }
    schedule.run(&mut world);
    let entity = world.spawn(Counter(20)).id();
    schedule.run(&mut world);
    schedule.run(&mut world);
    assert_eq!(world.get::<Counter>(entity).expect("counter").0, 22);
    drop(schedule);
    assert_eq!(*observations, [4, 0, 1, 1]);
}

#[test]
fn resource_table_refreshes_after_resource_removal_and_reinsertion() {
    #[derive(Resource)]
    struct Value(u32);
    unsafe extern "C" fn observe(call: *const sys::SystemCall) -> sys::SystemStatus {
        // SAFETY: the test supplies one resource slot; user points to a Vec
        // that outlives the schedule. Missing resources are explicitly null.
        unsafe {
            let call = &*call;
            let observations = &mut *(call.user as *mut Vec<Option<u32>>);
            let ptr = (*call.resources).ptr.cast::<Value>();
            observations.push(ptr.as_ref().map(|value| value.0));
        }
        sys::SystemStatus::Ok
    }
    let mut world = World::new();
    world.init_resource::<Time>();
    let id = world.register_resource::<Value>();
    world.insert_resource(Value(5));
    let mut observations = Box::new(Vec::<Option<u32>>::new());
    let system = build_dispatcher(
        &mut world,
        Vec::new(),
        vec![TermPlan {
            id,
            access: sys::Access::ResRead,
            marshal: Marshal::Raw,
            cell_size: size_of::<Value>(),
        }],
        observe,
        observations.as_mut() as *mut Vec<Option<u32>> as usize,
        GenGate {
            counter: Default::default(),
            at: 0,
            cancelled: Default::default(),
        },
        sys::system_services::ALL,
    );
    let mut schedule = Schedule::default();
    schedule.add_systems(system);
    schedule.run(&mut world);
    world.remove_resource::<Value>();
    schedule.run(&mut world);
    world.insert_resource(Value(9));
    schedule.run(&mut world);
    drop(schedule);
    assert_eq!(*observations, [Some(5), None, Some(9)]);
}

struct CommandProbe {
    entity: sys::Entity,
    component: sys::ComponentId,
    calls: u32,
    status: sys::SystemStatus,
}

unsafe extern "C" fn command_probe(call: *const sys::SystemCall) -> sys::SystemStatus {
    // SAFETY: this fixture owns the user state until the schedule is dropped;
    // the sink copies the payload synchronously before this stack value dies.
    unsafe {
        let call = &*call;
        let state = &mut *(call.user as *mut CommandProbe);
        state.calls += 1;
        let mut value = state.calls + 10;
        let command = sys::Command {
            kind: sys::CommandKind::Insert,
            entity: state.entity,
            component: state.component,
            data: (&value as *const u32).cast(),
            data_len: size_of::<u32>(),
        };
        ((*call.commands).push)(call.commands, &command);
        value = 999;
        std::hint::black_box(value);
        state.status
    }
}

fn run_command_probe(status: sys::SystemStatus) -> (u32, u32) {
    let mut world = World::new();
    world.init_resource::<Time>();
    let id = world.register_component::<Counter>();
    let entity = world.spawn(Counter(0)).id();
    let mut probe = Box::new(CommandProbe {
        entity: sys::Entity(entity.to_bits()),
        component: sys::ComponentId(id.index() as u32),
        calls: 0,
        status,
    });
    let system = build_dispatcher(
        &mut world,
        Vec::new(),
        Vec::new(),
        command_probe,
        probe.as_mut() as *mut CommandProbe as usize,
        GenGate {
            counter: Default::default(),
            at: 0,
            cancelled: Default::default(),
        },
        sys::system_services::ALL,
    );
    let mut schedule = Schedule::default();
    schedule.add_systems(system);
    schedule.run(&mut world);
    schedule.run(&mut world);
    drop(schedule);
    (
        world.get::<Counter>(entity).expect("counter").0,
        probe.calls,
    )
}

#[test]
fn deferred_command_payload_survives_stack_reuse_and_repeated_calls() {
    assert_eq!(run_command_probe(sys::SystemStatus::Ok), (12, 2));
}

#[test]
fn refused_output_discards_commands_and_does_not_replay_next_frame() {
    assert_eq!(run_command_probe(sys::SystemStatus::Panicked), (0, 1));
    assert_eq!(run_command_probe(sys::SystemStatus(999)), (0, 1));
}
