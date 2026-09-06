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
    for (mut rs, lv) in &mut q {
        let v = lv.map(|lv| lv.0).unwrap_or(Vec3::ZERO);
        rs.velocity = v;
        rs.speed = v.length();
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
    for (mut rs, lv) in &mut q {
        let v = lv.map(|lv| lv.0.extend(0.0)).unwrap_or(Vec3::ZERO);
        rs.velocity = v;
        rs.speed = v.length();
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
    for (entity, mut rs) in &mut q {
        let mut current: std::collections::HashSet<Entity> = std::collections::HashSet::new();
        for pair in collisions.collisions_with(entity) {
            let other = if pair.collider1 == entity {
                pair.collider2
            } else {
                pair.collider1
            };
            current.insert(other);
        }
        diff_collision_state(&mut rs, current, &names);
    }
}

/// avian2d twin of [`update_collision_read_state`], reading the 2D contact graph.
#[cfg(feature = "avian2d")]
pub fn update_collision_read_state_2d(
    mut q: Query<(Entity, &mut CollisionReadState), With<RuntimePhysics2d>>,
    collisions: avian2d::prelude::Collisions,
    names: Query<&Name>,
) {
    for (entity, mut rs) in &mut q {
        let mut current: std::collections::HashSet<Entity> = std::collections::HashSet::new();
        for pair in collisions.collisions_with(entity) {
            let other = if pair.collider1 == entity {
                pair.collider2
            } else {
                pair.collider1
            };
            current.insert(other);
        }
        diff_collision_state(&mut rs, current, &names);
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
            .map(|n| n.as_str().to_string())
            .unwrap_or_default()
    };
    // The public snapshot exposes only one name per transition. Stop at the
    // first difference instead of allocating lists of contacts we never use.
    let entered = current.difference(&rs.prev).next();
    let exited = rs.prev.difference(&current).next();
    rs.colliding = !current.is_empty();
    rs.entered = entered.is_some();
    rs.exited = exited.is_some();
    rs.entered_name = name_of(entered);
    rs.exited_name = name_of(exited);
    rs.prev = current;
}

#[cfg(all(test, any(feature = "avian3d", feature = "avian2d")))]
mod tests {
    use super::*;
    use bevy::ecs::system::SystemState;
    use std::collections::HashSet;

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
