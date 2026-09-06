//! Hierarchy tree cache — rebuilds the tree only when ECS changes actually
//! affect it.
//!
//! The panel's `ui()` runs every frame in an Update-schedule system and used
//! to call `build_entity_tree()` unconditionally, which iterates every
//! archetype and walks each entity's ancestor chain. For scenes with
//! thousands of entities this dominated frame time.
//!
//! We now cache the tree in `HierarchyTreeCache` and flip a `HierarchyDirty`
//! flag in a cheap observer system that watches `Added<T>` / `Changed<T>` /
//! `RemovedComponents<T>` for the components the tree actually depends on.
//! The exclusive `update_hierarchy_cache` system runs in `Update`, rebuilds
//! only when dirty, and the panel reads from the cached `Vec<EntityNode>`.

use std::collections::HashSet;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use renzora_editor_framework::{
    EditorLocked, EntityIcon, EntityLabelColor, HideInHierarchy, HierarchyFilter, HierarchyOrder,
};

use crate::state::{build_entity_tree, EntityNode, HierarchySpawnSeq};

/// Candidates before user filtering, plus every ancestor they depend on.
#[derive(Resource, Default)]
pub(crate) struct HierarchyDependencies(HashSet<Entity>);

#[derive(SystemParam)]
pub struct HierarchyChangeScope<'w, 's> {
    previous: Res<'w, HierarchyDependencies>,
    candidates: Query<'w, 's, Entity, (With<Name>, crate::state::HierarchyCandidate)>,
    icons: Option<Res<'w, renzora::ComponentIconRegistry>>,
}

impl HierarchyChangeScope<'_, '_> {
    fn affects_tree(&self, entity: Entity) -> bool {
        self.icons
            .as_ref()
            .is_some_and(|icons| icons.has_world_dependent_icons())
            || self.previous.0.contains(&entity)
            || self.candidates.contains(entity)
    }
}

/// Cached entity tree, produced by `update_hierarchy_cache`.
#[derive(Resource, Default)]
pub struct HierarchyTreeCache {
    pub nodes: Vec<EntityNode>,
    /// Monotonic counter; consumers can compare against a stored value to
    /// detect rebuilds without diffing the tree.
    pub version: u64,
}

/// Dirty flag: set by `mark_hierarchy_dirty` whenever a component the tree
/// depends on is added/changed/removed. Cleared by `update_hierarchy_cache`
/// after a successful rebuild.
#[derive(Resource)]
pub struct HierarchyDirty(pub bool);

impl Default for HierarchyDirty {
    // Default-dirty so the first frame populates the cache.
    fn default() -> Self {
        Self(true)
    }
}

/// Observe ECS changes that affect the hierarchy tree and flip the dirty
/// flag. Cheap — just iterates filtered queries, doesn't build anything.
pub fn mark_hierarchy_dirty(
    mut dirty: ResMut<HierarchyDirty>,
    filter: Option<Res<HierarchyFilter>>,
    scope: HierarchyChangeScope,
    changed_name: Query<Entity, Or<(Added<Name>, Changed<Name>)>>,
    changed_child_of: Query<Entity, Changed<ChildOf>>,
    changed_visibility: Query<Entity, Changed<Visibility>>,
    changed_locked: Query<Entity, Changed<EditorLocked>>,
    changed_hide: Query<Entity, Changed<HideInHierarchy>>,
    changed_order: Query<Entity, Changed<HierarchyOrder>>,
    mut removed_name: RemovedComponents<Name>,
    mut removed_child_of: RemovedComponents<ChildOf>,
    mut removed_hide: RemovedComponents<HideInHierarchy>,
    // The label colour and icon override, grouped for the same reason as
    // `AssetBadgeChanges` below — four more bare params would push this system
    // past Bevy's per-system cap.
    mut identity: IdentityChanges,
    // Asset badges (script/blueprint/material) ride on these components, so
    // their add/change/remove must rebuild the tree too (grouped into one param
    // to stay under Bevy's per-system param-count cap).
    mut badges: AssetBadgeChanges,
) {
    if dirty.0 {
        return;
    }

    if filter.as_ref().is_some_and(|f| f.is_changed())
        || scope.icons.as_ref().is_some_and(|icons| icons.is_changed())
    {
        dirty.0 = true;
        return;
    }

    if changed_name.iter().any(|e| scope.affects_tree(e))
        || changed_child_of.iter().any(|e| scope.affects_tree(e))
        || changed_visibility.iter().any(|e| scope.affects_tree(e))
        || changed_locked.iter().any(|e| scope.affects_tree(e))
        || changed_hide.iter().any(|e| scope.affects_tree(e))
        || changed_order.iter().any(|e| scope.affects_tree(e))
        || removed_name.read().any(|e| scope.affects_tree(e))
        || removed_child_of.read().any(|e| scope.affects_tree(e))
        || removed_hide.read().any(|e| scope.affects_tree(e))
        || identity.dirty(&scope)
        || badges.dirty(&scope)
    {
        dirty.0 = true;
    }
}

/// Change detection for the entity's *authored* identity — the label colour and
/// the icon override the inspector's entity header edits. Both are set and
/// cleared from outside the hierarchy, so without the removal halves an entity
/// reset to "Auto" would keep drawing its old icon until something unrelated
/// dirtied the tree.
#[derive(SystemParam)]
pub struct IdentityChanges<'w, 's> {
    eligibility: EligibilityChanges<'w, 's>,
    changed_label: Query<'w, 's, Entity, Changed<EntityLabelColor>>,
    changed_icon: Query<'w, 's, Entity, Changed<EntityIcon>>,
    removed_label: RemovedComponents<'w, 's, EntityLabelColor>,
    removed_icon: RemovedComponents<'w, 's, EntityIcon>,
}

impl IdentityChanges<'_, '_> {
    fn dirty(&mut self, scope: &HierarchyChangeScope) -> bool {
        self.eligibility.dirty(scope)
            || self.changed_label.iter().any(|e| scope.affects_tree(e))
            || self.changed_icon.iter().any(|e| scope.affects_tree(e))
            || self.removed_label.read().any(|e| scope.affects_tree(e))
            || self.removed_icon.read().any(|e| scope.affects_tree(e))
    }
}

#[derive(SystemParam)]
struct EligibilityChanges<'w, 's> {
    changed: Query<
        'w,
        's,
        Entity,
        Or<(
            Added<Node>,
            Added<renzora_ember::game_ui::UiCanvas>,
            Changed<renzora_ember::game_ui::UiWidget>,
            Added<bevy::input::gamepad::Gamepad>,
        )>,
    >,
    disabled: Query<
        'w,
        's,
        Entity,
        (
            Allow<bevy::ecs::entity_disabling::Disabled>,
            Added<bevy::ecs::entity_disabling::Disabled>,
        ),
    >,
    node: RemovedComponents<'w, 's, Node>,
    canvas: RemovedComponents<'w, 's, renzora_ember::game_ui::UiCanvas>,
    widget: RemovedComponents<'w, 's, renzora_ember::game_ui::UiWidget>,
    gamepad: RemovedComponents<'w, 's, bevy::input::gamepad::Gamepad>,
    enabled: RemovedComponents<'w, 's, bevy::ecs::entity_disabling::Disabled>,
}

impl EligibilityChanges<'_, '_> {
    fn dirty(&mut self, scope: &HierarchyChangeScope) -> bool {
        self.changed
            .iter()
            .chain(self.disabled.iter())
            .any(|e| scope.affects_tree(e))
            || self.node.read().any(|e| scope.affects_tree(e))
            || self.canvas.read().any(|e| scope.affects_tree(e))
            || self.widget.read().any(|e| scope.affects_tree(e))
            || self.gamepad.read().any(|e| scope.affects_tree(e))
            || self.enabled.read().any(|e| scope.affects_tree(e))
    }
}

/// Change detection for the components that drive the hierarchy's asset badges,
/// grouped so `mark_hierarchy_dirty` stays under Bevy's system param-count cap.
#[derive(SystemParam)]
pub struct AssetBadgeChanges<'w, 's> {
    // `Changed` already fires on the add tick, so it covers attach + edit.
    changed_script: Query<'w, 's, Entity, Changed<renzora_scripting::ScriptComponent>>,
    changed_material: Query<'w, 's, Entity, Changed<renzora::core::MaterialRef>>,
    removed_script: RemovedComponents<'w, 's, renzora_scripting::ScriptComponent>,
    removed_material: RemovedComponents<'w, 's, renzora::core::MaterialRef>,
}

impl AssetBadgeChanges<'_, '_> {
    fn dirty(&mut self, scope: &HierarchyChangeScope) -> bool {
        self.changed_script.iter().any(|e| scope.affects_tree(e))
            || self.changed_material.iter().any(|e| scope.affects_tree(e))
            || self.removed_script.read().any(|e| scope.affects_tree(e))
            || self.removed_material.read().any(|e| scope.affects_tree(e))
    }
}

/// Exclusive system: rebuilds `HierarchyTreeCache` when dirty. Runs in
/// `Update` so the cache is populated before the panel reads it.
pub fn update_hierarchy_cache(world: &mut World, mut last_build: Local<Option<f32>>) {
    let dirty = world.resource::<HierarchyDirty>().0;
    // An empty tree is a valid cached scene, not a request to rebuild forever.
    // HierarchyDirty starts true, so the first build needs no size-based gate.
    if !dirty {
        return;
    }

    // Real scene edits (and unrestricted icon callbacks) can still churn. Keep
    // the existing 100 ms debounce on this exclusive scan even though unrelated
    // editor chrome no longer invalidates the standard scene cache.
    let now = world.resource::<Time>().elapsed_secs();
    if last_build.is_some_and(|last| now - last < 0.1) {
        return;
    }
    *last_build = Some(now);

    world.resource_scope(|world, mut dependencies: Mut<HierarchyDependencies>| {
        dependencies.0.clear();
        let mut candidates =
            world.query_filtered::<Entity, (With<Name>, crate::state::HierarchyCandidate)>();
        for entity in candidates.iter(world) {
            let mut current = entity;
            while dependencies.0.insert(current) {
                let Some(parent) = world.get::<ChildOf>(current) else {
                    break;
                };
                current = parent.parent();
            }
        }
    });

    let nodes = world.resource_scope(|world, mut seq: Mut<HierarchySpawnSeq>| {
        build_entity_tree(world, &mut seq)
    });
    let mut cache = world.resource_mut::<HierarchyTreeCache>();
    cache.nodes = nodes;
    cache.version = cache.version.wrapping_add(1);
    world.resource_mut::<HierarchyDirty>().0 = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance(app: &mut App) {
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(101));
        app.update();
    }

    #[test]
    fn chrome_churn_is_ignored_but_scene_ancestors_and_eligibility_are_not() {
        let mut app = App::new();
        app.init_resource::<HierarchyTreeCache>()
            .init_resource::<HierarchyDependencies>()
            .init_resource::<HierarchyDirty>()
            .init_resource::<HierarchySpawnSeq>()
            .init_resource::<Time>()
            .add_systems(
                Update,
                (mark_hierarchy_dirty, update_hierarchy_cache).chain(),
            );
        let ancestor = app.world_mut().spawn_empty().id();
        let parent = app.world_mut().spawn(Name::new("parent")).id();
        let scene = app
            .world_mut()
            .spawn((Name::new("scene"), ChildOf(ancestor)))
            .id();
        advance(&mut app);
        advance(&mut app);
        let version = app.world().resource::<HierarchyTreeCache>().version;
        for _ in 0..1000 {
            let chrome = app
                .world_mut()
                .spawn((Name::new("status"), Node::default(), Visibility::Visible))
                .id();
            advance(&mut app);
            app.world_mut().despawn(chrome);
            advance(&mut app);
        }
        assert_eq!(
            app.world().resource::<HierarchyTreeCache>().version,
            version
        );
        app.world_mut().entity_mut(ancestor).insert(ChildOf(parent));
        advance(&mut app);
        assert!(app.world().resource::<HierarchyTreeCache>().version > version);
        let nodes = &app.world().resource::<HierarchyTreeCache>().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].entity, parent);
        assert_eq!(nodes[0].children[0].entity, scene);
        app.world_mut().entity_mut(scene).insert(Node::default());
        advance(&mut app);
        assert!(app.world().resource::<HierarchyTreeCache>().nodes[0]
            .children
            .is_empty());
        app.world_mut().entity_mut(scene).remove::<Node>();
        advance(&mut app);
        assert_eq!(
            app.world().resource::<HierarchyTreeCache>().nodes[0]
                .children
                .len(),
            1
        );
        app.world_mut()
            .entity_mut(scene)
            .insert(bevy::ecs::entity_disabling::Disabled);
        advance(&mut app);
        assert!(app.world().resource::<HierarchyTreeCache>().nodes[0]
            .children
            .is_empty());
        app.world_mut()
            .entity_mut(scene)
            .remove::<bevy::ecs::entity_disabling::Disabled>();
        advance(&mut app);
        assert_eq!(
            app.world().resource::<HierarchyTreeCache>().nodes[0]
                .children
                .len(),
            1
        );
    }

    #[test]
    fn unrestricted_icon_callbacks_keep_conservative_invalidation() {
        let mut app = App::new();
        app.init_resource::<HierarchyTreeCache>()
            .init_resource::<HierarchyDependencies>()
            .init_resource::<HierarchyDirty>()
            .init_resource::<HierarchySpawnSeq>()
            .init_resource::<renzora::ComponentIconRegistry>()
            .init_resource::<Time>()
            .add_systems(
                Update,
                (mark_hierarchy_dirty, update_hierarchy_cache).chain(),
            );
        let entry = || renzora::ComponentIconEntry {
            type_id: std::any::TypeId::of::<Name>(),
            name: "named",
            icon: "circle",
            color: [1, 2, 3],
            priority: 1,
            dynamic_icon_fn: Some(|world, entity| {
                world.get::<Name>(entity).map(|_| ("circle", [1, 2, 3]))
            }),
        };
        app.world_mut()
            .resource_mut::<renzora::ComponentIconRegistry>()
            .register_entity_local(entry());
        advance(&mut app);
        let before = app.world().resource::<HierarchyTreeCache>().version;
        app.world_mut()
            .spawn((Name::new("chrome"), Node::default()));
        advance(&mut app);
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, before);
        app.world_mut()
            .resource_mut::<renzora::ComponentIconRegistry>()
            .register(entry());
        advance(&mut app);
        let before = app.world().resource::<HierarchyTreeCache>().version;
        app.world_mut()
            .spawn((Name::new("other chrome"), Node::default()));
        advance(&mut app);
        assert_eq!(
            app.world().resource::<HierarchyTreeCache>().version,
            before + 1
        );
    }

    #[test]
    fn empty_scene_is_cached_and_later_edits_still_rebuild() {
        let mut app = App::new();
        app.init_resource::<HierarchyTreeCache>()
            .init_resource::<HierarchyDependencies>()
            .init_resource::<HierarchyDirty>()
            .init_resource::<HierarchySpawnSeq>()
            .init_resource::<Time>()
            .add_systems(
                Update,
                (mark_hierarchy_dirty, update_hierarchy_cache).chain(),
            );
        app.update();
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 1);
        for _ in 0..1_000 {
            app.world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_millis(20));
            app.update();
        }
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 1);
        let entity = app.world_mut().spawn(Name::new("scene entity")).id();
        app.update();
        assert_eq!(app.world().resource::<HierarchyTreeCache>().nodes.len(), 1);
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 2);
        app.world_mut().despawn(entity);
        app.update();
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 2);
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(101));
        app.update();
        assert!(app
            .world()
            .resource::<HierarchyTreeCache>()
            .nodes
            .is_empty());
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 3);
        for _ in 0..1_000 {
            app.update();
        }
        assert_eq!(app.world().resource::<HierarchyTreeCache>().version, 3);
    }
}
