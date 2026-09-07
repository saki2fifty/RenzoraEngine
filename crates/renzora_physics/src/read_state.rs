//! Per-entity physics mirror component.
//!
//! [`PhysicsReadState`] holds a script-/blueprint-readable snapshot of the current
//! physics state for each entity with [`PhysicsBodyData`]. It's populated each
//! frame so that Lua's `get("PhysicsReadState.grounded")` and blueprint
//! `physics/is_grounded` nodes have an up-to-date value without having to query
//! Avian directly.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::data::PhysicsBodyData;
#[cfg(any(feature = "avian3d", feature = "avian2d"))]
use crate::data::RuntimePhysics2d;

/// Snapshot of per-entity physics state, refreshed each frame.
///
/// Read-only from scripts / blueprints — writes are ignored (the updater
/// overwrites every frame). Reflect-registered so the existing `get`/`set`
/// path dispatcher can access fields by name (e.g. `PhysicsReadState.grounded`).
#[derive(Component, Clone, Debug, Default, Reflect, Serialize, Deserialize)]
#[reflect(Component, Serialize, Deserialize)]
pub struct PhysicsReadState {
    /// True if a downward shape cast found ground this frame (below `max_slope`).
    pub grounded: bool,
    /// Linear velocity (world space). For kinematic bodies, this is the last
    /// commanded velocity rather than a solver-integrated value.
    pub velocity: Vec3,
    /// Scalar magnitude of `velocity`.
    pub speed: f32,
    /// Contact normal from the most recent ground hit (or `Vec3::Y` if airborne).
    pub ground_normal: Vec3,
}

/// Auto-inserts `PhysicsReadState` on any entity that has `PhysicsBodyData`
/// but not yet a read-state component.
pub fn auto_init_physics_read_state(
    mut commands: Commands,
    q: Query<Entity, (With<PhysicsBodyData>, Without<PhysicsReadState>)>,
) {
    for entity in &q {
        commands
            .entity(entity)
            .try_insert(PhysicsReadState::default());
    }
}

/// Refreshes `PhysicsReadState` from Avian's current state. 2D-backend bodies
/// are excluded — their avian2d twin below owns them (each backend has its own
/// `LinearVelocity` type, and this one would zero a 2D body's reading).
#[cfg(feature = "avian3d")]
pub fn update_physics_read_state(
    mut q: Query<
        (
            &mut PhysicsReadState,
            Option<&avian3d::prelude::LinearVelocity>,
        ),
        Without<RuntimePhysics2d>,
    >,
) {
    for (rs, lv) in &mut q {
        let v = lv.map(|lv| lv.0).unwrap_or(Vec3::ZERO);
        mirror_velocity(rs, v);
        // `grounded` + `ground_normal` are written by the `kinematic_slide`
        // drain system each time a slide runs.
    }
}

/// avian2d twin of [`update_physics_read_state`]: mirrors 2D velocity into the
/// same Vec3 fields (z = 0) so scripts read one shape either way.
#[cfg(feature = "avian2d")]
pub fn update_physics_read_state_2d(
    mut q: Query<
        (
            &mut PhysicsReadState,
            Option<&avian2d::prelude::LinearVelocity>,
        ),
        With<RuntimePhysics2d>,
    >,
) {
    for (rs, lv) in &mut q {
        let v = lv.map(|lv| lv.0.extend(0.0)).unwrap_or(Vec3::ZERO);
        mirror_velocity(rs, v);
    }
}

#[cfg(any(feature = "avian3d", feature = "avian2d"))]
fn mirror_velocity(mut state: Mut<PhysicsReadState>, velocity: Vec3) {
    let speed = velocity.length();
    // Compare the mirror, not the backend's change tick: removed velocities and
    // externally edited readings must still be corrected. Bit equality retains
    // signed-zero changes without repeatedly invalidating an identical NaN.
    if state.velocity.to_array().map(f32::to_bits) != velocity.to_array().map(f32::to_bits)
        || state.speed.to_bits() != speed.to_bits()
    {
        state.velocity = velocity;
        state.speed = speed;
    }
}

/// Per-entity collision snapshot, refreshed each frame from Avian's contact
/// pairs. Reflect-registered so blueprint `event/on_collision_enter`/`_exit`
/// (and Lua `get("CollisionReadState.entered")`) can read it by name. This is
/// the engine's first real collision-event source — previously the scripting
/// `on_collision` hook was an unpopulated stub.
///
/// Only the *first* entity entered/exited this frame is surfaced by name (the
/// blueprint event has a single `other` output); `colliding` reflects whether
/// any contact is currently active.
#[derive(Component, Clone, Debug, Default, Reflect)]
#[reflect(Component, Default)]
pub struct CollisionReadState {
    /// True while at least one contact is active this frame.
    pub colliding: bool,
    /// True on the frame a new contact began.
    pub entered: bool,
    /// True on the frame a contact ended.
    pub exited: bool,
    /// Name of the first entity that started touching this frame ("" if none).
    pub entered_name: String,
    /// Name of the first entity that stopped touching this frame ("" if none).
    pub exited_name: String,
    /// Last frame's colliding set, used to diff enter/exit. Not reflected.
    #[reflect(ignore)]
    prev: std::collections::HashSet<Entity>,
}

/// Auto-inserts `CollisionReadState` on any entity with `PhysicsBodyData`.
pub fn auto_init_collision_read_state(
    mut commands: Commands,
    q: Query<Entity, (With<PhysicsBodyData>, Without<CollisionReadState>)>,
) {
    for entity in &q {
        commands
            .entity(entity)
            .try_insert(CollisionReadState::default());
    }
}

/// Refreshes `CollisionReadState` by diffing each entity's current Avian contact
/// set against the previous frame's. 2D-backend bodies are excluded — the 3D
/// contact graph never contains them, so this would wipe their `prev` set every
/// frame and the 2D twin below would report a fresh "entered" forever.
#[cfg(feature = "avian3d")]
pub fn update_collision_read_state(
    mut q: Query<(Entity, &mut CollisionReadState), Without<RuntimePhysics2d>>,
    collisions: avian3d::prelude::Collisions,
    names: Query<&Name>,
) {
    for (entity, rs) in &mut q {
        refresh_collision_component(rs, &names, || {
            collisions.entities_colliding_with(entity)
        });
    }
}

/// avian2d twin of [`update_collision_read_state`], reading the 2D contact graph.
#[cfg(feature = "avian2d")]
pub fn update_collision_read_state_2d(
    mut q: Query<(Entity, &mut CollisionReadState), With<RuntimePhysics2d>>,
    collisions: avian2d::prelude::Collisions,
    names: Query<&Name>,
) {
    for (entity, rs) in &mut q {
        refresh_collision_component(rs, &names, || {
            collisions.entities_colliding_with(entity)
        });
    }
}

#[cfg(any(feature = "avian3d", feature = "avian2d"))]
fn refresh_collision_component<I: Iterator<Item = Entity>>(
    mut state: Mut<CollisionReadState>,
    names: &Query<&Name>,
    contacts: impl Fn() -> I,
) {
    // The diff reports every field/set mutation. Preserve allocation reuse
    // without marking all settled bodies Changed merely by borrowing mutably.
    if refresh_collision_state(state.bypass_change_detection(), names, contacts) {
        state.set_changed();
    }
}

/// `contacts` must yield unique collider entities from an unchanged graph.
/// Avian's ContactGraph enforces one edge per pair; both callers borrow it for
/// the entire refresh. A transition may need a second traversal, but a settled
/// contact set needs no allocation, rehashing into a new set, or pool retention.
#[cfg(any(feature = "avian3d", feature = "avian2d"))]
fn refresh_collision_state<I: Iterator<Item = Entity>>(
    state: &mut CollisionReadState,
    names: &Query<&Name>,
    contacts: impl Fn() -> I,
) -> bool {
    let mut count = 0;
    let unchanged = contacts().all(|entity| {
        count += 1;
        state.prev.contains(&entity)
    }) && count == state.prev.len();
    if unchanged {
        let changed = state.colliding != (count != 0)
            || state.entered
            || state.exited
            || !state.entered_name.is_empty()
            || !state.exited_name.is_empty();
        state.colliding = count != 0;
        state.entered = false;
        state.exited = false;
        state.entered_name.clear();
        state.exited_name.clear();
        changed
    } else {
        diff_collision_state(state, contacts().collect(), names);
        true
    }
}

/// Shared enter/exit diff for both backends' collision updaters.
#[cfg(any(feature = "avian3d", feature = "avian2d"))]
fn diff_collision_state(
    rs: &mut CollisionReadState,
    current: std::collections::HashSet<Entity>,
    names: &Query<&Name>,
) {
    let name_of = |e: Option<&Entity>| {
        e.and_then(|x| names.get(*x).ok())
            .map(|n| n.as_str())
            .unwrap_or_default()
    };
    // The public snapshot exposes only one name per transition. Stop at the
    // first difference instead of allocating lists of contacts we never use.
    let entered = current.difference(&rs.prev).next();
    let exited = rs.prev.difference(&current).next();
    rs.colliding = !current.is_empty();
    rs.entered = entered.is_some();
    rs.exited = exited.is_some();
    // Keep each entity's name capacity across one-frame notifications instead
    // of dropping the allocation as soon as a transition flag clears.
    rs.entered_name.clear();
    rs.entered_name.push_str(name_of(entered));
    rs.exited_name.clear();
    rs.exited_name.push_str(name_of(exited));
    rs.prev = current;
}

#[cfg(all(test, any(feature = "avian3d", feature = "avian2d")))]
mod tests {
    use super::*;
    use bevy::ecs::system::SystemState;
    use std::collections::HashSet;

    #[test]
    fn collision_component_only_marks_real_transitions_changed() {
        use bevy::ecs::system::RunSystemOnce;
        #[derive(Resource, Default)]
        struct Contacts(Vec<Entity>);
        fn refresh(
            mut states: Query<&mut CollisionReadState>,
            names: Query<&Name>,
            contacts: Res<Contacts>,
        ) {
            for state in &mut states {
                refresh_collision_component(state, &names, || contacts.0.iter().copied());
            }
        }
        let mut world = World::new();
        world.init_resource::<Contacts>();
        let body = world.spawn(CollisionReadState::default()).id();
        let other = world.spawn(Name::new("other")).id();
        let mut changed = world.query_filtered::<Entity, Changed<CollisionReadState>>();
        for touching in [false, true, false] {
            world.resource_mut::<Contacts>().0 = if touching { vec![other] } else { vec![] };
            for frame in 0..1000 {
                let had_contacts = !world.get::<CollisionReadState>(body).unwrap().prev.is_empty();
                let state = world.get::<CollisionReadState>(body).unwrap();
                let expected_change = touching != had_contacts || state.entered || state.exited;
                world.clear_trackers();
                world.run_system_once(refresh).unwrap();
                assert_eq!(changed.iter(&world).count(), usize::from(expected_change));
                let state = world.get::<CollisionReadState>(body).unwrap();
                assert_eq!(state.colliding, touching);
                if frame > 0 {
                    assert!(!state.entered && !state.exited);
                }
            }
        }
    }

    #[test]
    fn stable_contacts_skip_rebuild_and_transitions_keep_frame_semantics() {
        use std::cell::Cell;
        let mut world = World::new();
        let a = world.spawn(Name::new("a")).id();
        let b = world.spawn(Name::new("b")).id();
        let c = world.spawn(Name::new("c")).id();
        let mut query = SystemState::<Query<&Name>>::new(&mut world);
        let names = query.get(&world).expect("name query");
        let mut state = CollisionReadState::default();
        refresh_collision_state(&mut state, &names, || [a, b].into_iter());
        assert!(state.entered && state.colliding && !state.exited);
        let calls = Cell::new(0);
        let original_order: Vec<_> = state.prev.iter().copied().collect();
        for _ in 0..1000 {
            refresh_collision_state(&mut state, &names, || {
                calls.set(calls.get() + 1);
                // Source iteration order is not part of set equality.
                [b, a].into_iter()
            });
            assert!(state.colliding && !state.entered && !state.exited);
            assert!(state.entered_name.is_empty() && state.exited_name.is_empty());
            assert_eq!(
                state.prev.iter().copied().collect::<Vec<_>>(),
                original_order
            );
        }
        // A rebuild requests a second iterator; stable frames never do.
        assert_eq!(calls.get(), 1000);
        refresh_collision_state(&mut state, &names, || [b, c].into_iter());
        assert!(state.entered && state.exited && state.colliding);
        assert_eq!(state.entered_name, "c");
        assert_eq!(state.exited_name, "a");
        refresh_collision_state(&mut state, &names, || [b].into_iter());
        assert!(!state.entered && state.exited && state.colliding);
        assert_eq!(state.exited_name, "c");
        refresh_collision_state(&mut state, &names, std::iter::empty);
        assert!(!state.entered && state.exited && !state.colliding);
        assert_eq!(state.exited_name, "b");
        refresh_collision_state(&mut state, &names, std::iter::empty);
        assert!(!state.entered && !state.exited && !state.colliding);
        assert!(state.exited_name.is_empty());
        eprintln!(
            "stable contacts: 1000 frames, 1000 comparison traversals, zero rebuild traversals"
        );
    }

    #[test]
    fn collision_names_reuse_capacity_without_mixing_entities() {
        let mut world = World::new();
        let entities: Vec<_> = (0..64)
            .map(|i| world.spawn(Name::new(format!("contact-{i:02}"))).id())
            .collect();
        let mut query = SystemState::<Query<&Name>>::new(&mut world);
        let names = query.get(&world).expect("name query");
        let mut states = [CollisionReadState::default(), CollisionReadState::default()];
        let frame = |states: &mut [CollisionReadState; 2], active| {
            for (i, state) in states.iter_mut().enumerate() {
                let mut current = HashSet::new();
                let contacts = &entities[i * 32..(i + 1) * 32];
                if active {
                    current.extend(contacts.iter().copied());
                }
                diff_collision_state(state, current, &names);
                assert_eq!(state.colliding, active);
                assert_eq!(state.entered, active);
                assert_eq!(state.exited, !active);
                assert_eq!(state.prev.len(), if active { 32 } else { 0 });
                assert!(state.prev.iter().all(|entity| contacts.contains(entity)));
                if active {
                    assert!(state.exited_name.is_empty());
                } else {
                    assert!(state.entered_name.is_empty());
                }
            }
        };
        let capacity = |states: &[CollisionReadState; 2]| {
            states
                .iter()
                .map(|s| s.entered_name.capacity() + s.exited_name.capacity())
                .sum::<usize>()
        };
        for i in 0..4 {
            frame(&mut states, i % 2 == 0);
        }
        let warmed = capacity(&states);
        assert!(
            states
                .iter()
                .all(|state| state.entered_name.capacity() >= 10
                    && state.exited_name.capacity() >= 10)
        );
        for i in 0..1000 {
            frame(&mut states, i % 2 == 0);
            assert_eq!(capacity(&states), warmed);
        }
        eprintln!("collision reuse: 1000 alternating frames across two entities retained {warmed} bytes of name capacity after warmup");
    }

    #[test]
    fn velocity_mirror_preserves_float_bits() {
        #[derive(Resource)]
        struct Input(Vec3);
        fn update(mut query: Query<&mut PhysicsReadState>, input: Res<Input>) {
            for state in &mut query {
                mirror_velocity(state, input.0);
            }
        }
        let mut app = App::new();
        app.insert_resource(Input(Vec3::ZERO))
            .add_systems(Update, update);
        let entity = app.world_mut().spawn(PhysicsReadState::default()).id();
        for velocity in [
            Vec3::new(-0.0, 0.0, 0.0),
            Vec3::new(f32::INFINITY, 0.0, 0.0),
            Vec3::new(f32::from_bits(0x7fc0_0123), 0.0, 0.0),
            Vec3::ZERO,
        ] {
            app.world_mut().resource_mut::<Input>().0 = velocity;
            app.update();
            let state = app.world().get::<PhysicsReadState>(entity).expect("mirror");
            assert_eq!(
                state.velocity.to_array().map(f32::to_bits),
                velocity.to_array().map(f32::to_bits)
            );
            assert_eq!(state.speed.to_bits(), velocity.length().to_bits());
        }
    }

    #[cfg(all(feature = "avian3d", feature = "avian2d"))]
    #[test]
    fn velocity_mirrors_stay_quiet_and_follow_backend_changes() {
        #[derive(Resource, Default)]
        struct Changes(usize);
        fn observe(q: Query<(), Changed<PhysicsReadState>>, mut count: ResMut<Changes>) {
            count.0 += q.iter().count();
        }
        let mut app = App::new();
        app.init_resource::<Changes>().add_systems(
            Update,
            (
                update_physics_read_state,
                update_physics_read_state_2d,
                observe,
            )
                .chain(),
        );
        let body = app
            .world_mut()
            .spawn((
                PhysicsReadState {
                    grounded: true,
                    ground_normal: Vec3::X,
                    ..default()
                },
                avian3d::prelude::LinearVelocity(Vec3::new(3.0, 4.0, 0.0)),
                avian2d::prelude::LinearVelocity(Vec2::new(0.0, 12.0)),
            ))
            .id();
        app.update();
        assert_eq!(
            app.world()
                .get::<PhysicsReadState>(body)
                .expect("mirror")
                .speed,
            5.0
        );
        let initial = app.world().resource::<Changes>().0;
        for _ in 0..1000 {
            app.update();
        }
        assert_eq!(app.world().resource::<Changes>().0, initial);
        app.world_mut().entity_mut(body).insert(RuntimePhysics2d);
        app.update();
        assert_eq!(
            app.world()
                .get::<PhysicsReadState>(body)
                .expect("mirror")
                .speed,
            12.0
        );
        let switched = app.world().resource::<Changes>().0;
        assert_eq!(switched, initial + 1);
        for _ in 0..1000 {
            app.update();
        }
        assert_eq!(app.world().resource::<Changes>().0, switched);
        app.world_mut()
            .entity_mut(body)
            .remove::<avian2d::prelude::LinearVelocity>();
        app.update();
        assert_eq!(
            app.world()
                .get::<PhysicsReadState>(body)
                .expect("mirror")
                .velocity,
            Vec3::ZERO
        );
        // Repair a stale reading even when the backend has not changed.
        app.world_mut()
            .get_mut::<PhysicsReadState>(body)
            .expect("mirror")
            .speed = 123.0;
        app.update();
        let state = app.world().get::<PhysicsReadState>(body).expect("mirror");
        assert_eq!(state.speed, 0.0);
        assert!(state.grounded);
        assert_eq!(state.ground_normal, Vec3::X);
        app.world_mut()
            .entity_mut(body)
            .remove::<RuntimePhysics2d>();
        app.update();
        assert_eq!(
            app.world()
                .get::<PhysicsReadState>(body)
                .expect("mirror")
                .speed,
            5.0
        );
        eprintln!("velocity mirrors: 1000 stable 3D frames and 1000 stable 2D frames produced zero extra change notifications");
    }

    #[test]
    fn collision_snapshot_matches_full_diff_through_contact_lifecycle() {
        let mut world = World::new();
        let entities: Vec<_> = (0..2048)
            .map(|i| world.spawn(Name::new(format!("contact-{i}"))).id())
            .collect();
        let unnamed = world.spawn_empty().id();
        let removed = world.spawn(Name::new("removed")).id();
        world.despawn(removed);
        let mut query = SystemState::<Query<&Name>>::new(&mut world);
        let names = query
            .get(&world)
            .expect("name query is valid for this world");
        let mut rs = CollisionReadState::default();
        let mut largest_temporary_list = 0;
        let first: HashSet<_> = entities[..1024].iter().copied().collect();
        let second: HashSet<_> = entities[1024..].iter().copied().collect();
        // Idle, enter, stay, simultaneous exits/enters, missing names, exit,
        // and clearing the one-frame exit flags all retain the old semantics.
        for current in [
            HashSet::new(),
            first.clone(),
            first,
            second,
            HashSet::from([unnamed]),
            HashSet::from([removed]),
            HashSet::new(),
            HashSet::new(),
        ] {
            let entered: Vec<_> = current.difference(&rs.prev).copied().collect();
            let exited: Vec<_> = rs.prev.difference(&current).copied().collect();
            largest_temporary_list = largest_temporary_list.max(entered.len() + exited.len());
            let name_of = |entity: Option<&Entity>| {
                entity
                    .and_then(|entity| names.get(*entity).ok())
                    .map(|name| name.as_str())
                    .unwrap_or_default()
            };
            let expected_entered = name_of(entered.first()).to_owned();
            let expected_exited = name_of(exited.first()).to_owned();
            let expected_current = current.clone();
            diff_collision_state(&mut rs, current, &names);
            assert_eq!(rs.colliding, !expected_current.is_empty());
            assert_eq!(rs.entered, !entered.is_empty());
            assert_eq!(rs.exited, !exited.is_empty());
            assert_eq!(rs.entered_name, expected_entered);
            assert_eq!(rs.exited_name, expected_exited);
            assert_eq!(rs.prev, expected_current);
        }
        assert_eq!(largest_temporary_list, 2048);
    }
}
