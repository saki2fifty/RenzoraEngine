//! Seeded tree generation and runtime wind integration.
//! The generator's upstream licensing is preserved beside this crate.

use bevy::prelude::*;
use renzora::{HideInHierarchy, WindSway};
use tree::{Leaves, Tree, TreeProceduralGenerationPlugin};

pub mod tree;

/// Install tree generation without editor registries.
#[derive(Default)]
pub struct ProceduralTreePlugin;

impl Plugin for ProceduralTreePlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "procedural_tree") {
            return;
        }
        app.add_plugins(TreeProceduralGenerationPlugin);
        app.add_systems(
            Update,
            (
                tag_generated_leaves,
                prune_stale_leaves,
                sway_generated_trees,
            ),
        );
    }
}

fn tag_generated_leaves(
    mut commands: Commands,
    trees: Query<&Leaves>,
    needs_tag: Query<(), (With<Mesh3d>, Without<HideInHierarchy>)>,
) {
    for leaves in trees.iter() {
        if needs_tag.get(leaves.0).is_ok() {
            commands.entity(leaves.0).insert(HideInHierarchy);
        }
    }
}

// Insert only: preserve author overrides and restored scene values.
fn sway_generated_trees(
    mut commands: Commands,
    trees: Query<(Entity, &Leaves), With<Tree>>,
    needs_sway: Query<(), Without<WindSway>>,
) {
    for (trunk, leaves) in trees.iter() {
        if needs_sway.get(trunk).is_ok() {
            commands.entity(trunk).insert(WindSway {
                response: 0.55,
                flutter: 0.0,
                amplitude: 0.25,
                ..default()
            });
        }
        if needs_sway.get(leaves.0).is_ok() {
            commands.entity(leaves.0).insert(WindSway {
                response: 1.0,
                flutter: 1.0,
                amplitude: 0.4,
                ..default()
            });
        }
    }
}

fn prune_stale_leaves(mut commands: Commands, stale: Query<(Entity, &Name), Without<Mesh3d>>) {
    for (entity, name) in stale.iter() {
        if name.as_str() == "ProcGenTreeLeaves" {
            commands.entity(entity).despawn();
        }
    }
}

renzora::add!(ProceduralTreePlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<Assets<Image>>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(ProceduralTreePlugin);
        app
    }

    #[test]
    fn shared_tree_hook_generates_textured_meshes_and_preserves_wind_overrides() {
        let mut app = app();
        let tree = app
            .world_mut()
            .spawn((Name::new("Authored tree"), Tree::default()))
            .id();
        app.world_mut().flush();
        app.update();
        let leaf = app.world().get::<Leaves>(tree).unwrap().0;
        assert!(app.world().get::<Mesh3d>(tree).is_some());
        assert!(app.world().get::<Mesh3d>(leaf).is_some());
        assert!(app.world().get::<HideInHierarchy>(leaf).is_some());
        assert_eq!(
            app.world().get::<Name>(tree).unwrap().as_str(),
            "Authored tree"
        );
        let material = app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(leaf)
            .unwrap();
        let material = app
            .world()
            .resource::<Assets<StandardMaterial>>()
            .get(&material.0)
            .unwrap();
        assert!(material.base_color_texture.is_some());
        assert_eq!(material.alpha_mode, AlphaMode::Mask(0.5));
        app.world_mut().get_mut::<WindSway>(tree).unwrap().response = 0.9;
        app.update();
        assert_eq!(app.world().get::<WindSway>(tree).unwrap().response, 0.9);
    }

    #[test]
    fn seeded_generation_is_repeatable() {
        let settings = tree::TreeMeshSettings::default();
        let (a, leaves_a) =
            tree::meshgen::generate_tree_meshes(&settings, &mut fastrand::Rng::with_seed(42))
                .unwrap();
        let (b, leaves_b) =
            tree::meshgen::generate_tree_meshes(&settings, &mut fastrand::Rng::with_seed(42))
                .unwrap();
        assert_eq!(
            a.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().as_float3(),
            b.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().as_float3()
        );
        assert_eq!(leaves_a.count_vertices(), leaves_b.count_vertices());
        assert!(a.attribute(Mesh::ATTRIBUTE_UV_1).is_some());
    }

    #[test]
    fn disabled_startup_does_not_require_generation_assets() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["procedural_tree".into()]));
        app.add_plugins(ProceduralTreePlugin);
        assert!(!app.is_plugin_added::<TreeProceduralGenerationPlugin>());
        app.update();
    }
}
