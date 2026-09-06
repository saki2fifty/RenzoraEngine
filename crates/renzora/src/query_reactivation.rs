//! Invalidation for derived state that is skipped while an entity is disabled.

use std::collections::HashMap;

use bevy::ecs::component::{ComponentId, Mutable};
use bevy::ecs::entity_disabling::DefaultQueryFilters;
use bevy::ecs::lifecycle::{RemovedComponentEntity, RemovedComponentMessages};
use bevy::ecs::message::MessageCursor;
use bevy::prelude::*;

/// Marks `T` changed when an entity becomes eligible for ordinary queries again.
///
/// Run every frame before the corresponding change-filtered derived-state system.
/// Edits made while disabled can otherwise age past that system's last-run tick.
/// Each installation owns independent removal cursors, including custom disabling
/// components registered before schedule initialization. Still-disabled entities
/// are ignored; removing their last disabling marker produces another event.
pub fn refresh_reactivated_components<T: Component<Mutability = Mutable>>(
    filters: Res<DefaultQueryFilters>,
    removals: &RemovedComponentMessages,
    mut readers: Local<HashMap<ComponentId, MessageCursor<RemovedComponentEntity>>>,
    mut components: Query<&mut T>,
) {
    for id in filters.disabling_ids() {
        let Some(messages) = removals.get(id) else {
            continue;
        };
        for removed in readers.entry(id).or_default().read(messages) {
            if let Ok(mut component) = components.get_mut(removed.clone().into()) {
                component.set_changed();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::entity_disabling::Disabled;

    #[derive(Component)]
    struct Input(u32);
    #[derive(Component)]
    struct OtherInput;
    #[derive(Component)]
    struct Hidden;
    #[derive(Resource, Default)]
    struct Seen(Vec<u32>, usize);

    fn derive(inputs: Query<&Input, Changed<Input>>, mut seen: ResMut<Seen>) {
        seen.0.extend(inputs.iter().map(|input| input.0));
    }

    fn derive_other(inputs: Query<(), Changed<OtherInput>>, mut seen: ResMut<Seen>) {
        seen.1 += inputs.iter().count();
    }

    #[test]
    fn reactivation_refreshes_once_after_last_marker_with_independent_readers() {
        let mut app = App::new();
        let hidden = app.world_mut().register_component::<Hidden>();
        app.world_mut()
            .resource_mut::<DefaultQueryFilters>()
            .register_disabling_component(hidden);
        app.init_resource::<Seen>().add_systems(
            Update,
            (
                refresh_reactivated_components::<Input>,
                refresh_reactivated_components::<OtherInput>,
                derive,
                derive_other,
            )
                .chain(),
        );
        let entity = app.world_mut().spawn((Input(1), OtherInput)).id();
        app.update();
        for _ in 0..1000 {
            app.update();
        }
        assert_eq!(app.world().resource::<Seen>().0, [1]);
        assert_eq!(app.world().resource::<Seen>().1, 1);
        app.world_mut()
            .entity_mut(entity)
            .insert((Disabled, Hidden));
        app.world_mut().get_mut::<Input>(entity).unwrap().0 = 2;
        for _ in 0..4 {
            app.update();
        }
        app.world_mut().entity_mut(entity).remove::<Disabled>();
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, [1]);
        app.world_mut().entity_mut(entity).remove::<Hidden>();
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, [1, 2]);
        assert_eq!(app.world().resource::<Seen>().1, 2);
        for _ in 0..1000 {
            app.update();
        }
        assert_eq!(app.world().resource::<Seen>().0, [1, 2]);
        assert_eq!(app.world().resource::<Seen>().1, 2);
        app.world_mut().entity_mut(entity).insert(Disabled);
        app.world_mut().get_mut::<Input>(entity).unwrap().0 = 3;
        for _ in 0..4 {
            app.update();
        }
        app.world_mut().entity_mut(entity).remove::<Disabled>();
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, [1, 2, 3]);
        app.world_mut().entity_mut(entity).insert(Disabled);
        app.world_mut().despawn(entity);
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, [1, 2, 3]);
    }
}
