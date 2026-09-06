//! Script name snapshots, preserving the engine query's duplicate-name order.

use std::{collections::HashMap, sync::Arc};

use bevy::ecs::change_detection::Tick;
use bevy::ecs::{component::ComponentId, entity_disabling::DefaultQueryFilters};
use bevy::prelude::*;

#[derive(Resource)]
pub(crate) struct Names {
    query: QueryState<(Entity, Ref<'static, Name>)>,
    filters: Vec<ComponentId>,
    order: Vec<(Entity, Tick)>,
    scratch: Vec<(Entity, Tick)>,
    ids: Arc<HashMap<String, u64>>,
    entities: Arc<HashMap<String, Entity>>,
}

pub(crate) fn release_unused(
    mut commands: Commands,
    scripts: Query<(), With<crate::ScriptComponent>>,
    cache: Option<Res<Names>>,
) {
    if cache.is_some() && scripts.is_empty() {
        commands.remove_resource::<Names>();
    }
}

impl FromWorld for Names {
    fn from_world(world: &mut World) -> Self {
        Self {
            query: world.query(),
            filters: world
                .resource::<DefaultQueryFilters>()
                .disabling_ids()
                .collect(),
            order: Vec::new(),
            scratch: Vec::new(),
            ids: Arc::default(),
            entities: Arc::default(),
        }
    }
}

pub(super) fn snapshot(
    world: &mut World,
) -> (Arc<HashMap<String, u64>>, Arc<HashMap<String, Entity>>) {
    world.init_resource::<Names>();
    world.resource_scope(|world, mut names: Mut<Names>| {
        if !world
            .resource::<DefaultQueryFilters>()
            .disabling_ids()
            .eq(names.filters.iter().copied())
        {
            names.filters = world
                .resource::<DefaultQueryFilters>()
                .disabling_ids()
                .collect();
            names.query = world.query();
        }
        let Names {
            query,
            order,
            scratch,
            ids,
            entities,
            ..
        } = &mut *names;
        scratch.clear();
        let mut changed = false;
        scratch.extend(query.iter(world).map(|(entity, name)| {
            // Keep Bevy's change-age semantics, including resumed
            // execution and multiple writes within one change tick.
            changed |= name.is_changed();
            (entity, name.last_changed())
        }));
        // Order matters for duplicate names and changes when an entity migrates
        // archetypes. Removed/disabled entities disappear from this same query.
        if changed || scratch != order {
            let mut next_ids = HashMap::with_capacity(scratch.len());
            let mut next_entities = HashMap::with_capacity(scratch.len());
            for (entity, name) in query.iter(world) {
                next_ids.insert(name.as_str().to_owned(), entity.to_bits());
                next_entities.insert(name.as_str().to_owned(), entity);
            }
            *ids = Arc::new(next_ids);
            *entities = Arc::new(next_entities);
            std::mem::swap(order, scratch);
        }
        (Arc::clone(ids), Arc::clone(entities))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_shaped_name_workload() {
        let mut world = World::new();
        for i in 0..10_000 {
            world.spawn(Name::new(format!("actor_{i}")));
        }
        let initial = snapshot(&mut world);
        world.clear_trackers();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let (ids, entities) = snapshot(&mut world);
            assert_eq!(ids.len(), 10_000);
            assert_eq!(entities.len(), 10_000);
            assert!(Arc::ptr_eq(&ids, &initial.0));
            assert!(Arc::ptr_eq(&entities, &initial.1));
            std::hint::black_box((ids, entities));
        }
        eprintln!(
            "script names: 10000 entities, 100 frames: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn snapshots_follow_rename_removal_duplicates_and_reactivation() {
        use bevy::ecs::entity_disabling::Disabled;
        #[derive(Component)]
        struct Other;
        let mut world = World::new();
        let first = world.spawn(Name::new("same")).id();
        let second = world.spawn(Name::new("same")).id();
        let old = snapshot(&mut world);
        world.entity_mut(first).insert(Other);
        let current = snapshot(&mut world);
        let expected = world
            .query::<(Entity, &Name)>()
            .iter(&world)
            .last()
            .unwrap()
            .0;
        assert_eq!(current.1["same"], expected);
        world.increment_change_tick();
        world.entity_mut(first).insert(Name::new("renamed"));
        let renamed = snapshot(&mut world);
        assert_eq!(renamed.1["renamed"], first);
        assert!(!old.1.contains_key("renamed"));
        // A second edit in the same tick still changes the name table.
        world.entity_mut(first).insert(Name::new("second_edit"));
        assert_eq!(snapshot(&mut world).1["second_edit"], first);
        world.entity_mut(first).insert(Name::new("renamed"));
        world.entity_mut(first).insert(Disabled);
        assert!(!snapshot(&mut world).1.contains_key("renamed"));
        world.entity_mut(first).remove::<Disabled>();
        assert_eq!(snapshot(&mut world).1["renamed"], first);
        world.entity_mut(first).remove::<Name>();
        assert!(!snapshot(&mut world).1.contains_key("renamed"));
        world.despawn(second);
        assert!(snapshot(&mut world).1.is_empty());
    }

    #[test]
    fn scene_teardown_releases_the_index_without_another_script_pass() {
        let mut app = App::new();
        app.add_systems(Update, release_unused);
        let entity = app
            .world_mut()
            .spawn((Name::new("actor"), crate::ScriptComponent::default()))
            .id();
        snapshot(app.world_mut());
        app.update();
        assert!(app.world().contains_resource::<Names>());
        app.world_mut().despawn(entity);
        app.update();
        assert!(!app.world().contains_resource::<Names>());
    }

    #[test]
    fn newly_registered_default_filters_refresh_the_cached_query() {
        #[derive(Component)]
        struct Hidden;
        let mut world = World::new();
        let entity = world.spawn((Name::new("hidden"), Hidden)).id();
        assert_eq!(snapshot(&mut world).1["hidden"], entity);
        let marker = world.register_component::<Hidden>();
        world
            .resource_mut::<DefaultQueryFilters>()
            .register_disabling_component(marker);
        assert!(snapshot(&mut world).1.is_empty());
        world.entity_mut(entity).remove::<Hidden>();
        assert_eq!(snapshot(&mut world).1["hidden"], entity);
    }
}
