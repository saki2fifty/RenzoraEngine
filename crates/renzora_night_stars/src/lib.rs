//! Night Stars — a procedural starfield on a sky dome.
//!
//! `NightStarsData` and `Sun` both live in the contract crate: `level_presets`
//! constructs the former when building a night sky, and this plugin reads the
//! latter to fade the field by sun elevation. Both are named by a binary and by
//! this runtime-loaded library, so both need one definition.

use bevy::pbr::Material;
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Re-exported so `renzora_night_stars::NightStarsData` still resolves.
pub use renzora::NightStarsData;

// ============================================================================
// Data types
// ============================================================================

// ============================================================================
// Star Material
// ============================================================================

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
#[type_path = "night_stars"]
pub struct NightStarsMaterial {
    /// density, brightness, star_size, twinkle_speed
    #[uniform(0)]
    pub params_a: Vec4,
    /// twinkle_amount, horizon_fade, unused, unused
    #[uniform(1)]
    pub params_b: Vec4,
    /// Star color tint (r, g, b, unused)
    #[uniform(2)]
    pub star_color: LinearRgba,
}

impl Material for NightStarsMaterial {
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Path("embedded://renzora_night_stars/night_stars.wgsl".into())
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    fn specialize(
        _pipeline: &bevy::pbr::MaterialPipeline,
        descriptor: &mut bevy::render::render_resource::RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        _key: bevy::pbr::MaterialPipelineKey<Self>,
    ) -> Result<(), bevy::render::render_resource::SpecializedMeshPipelineError> {
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

// ============================================================================
// Marker & State
// ============================================================================

#[derive(Component)]
pub struct NightStarsDomeMarker;

#[derive(Resource, Default)]
pub struct NightStarsState {
    pub entity: Option<Entity>,
    pub material_handle: Option<Handle<NightStarsMaterial>>,
    pub mesh_handle: Option<Handle<Mesh>>,
}

// ============================================================================
// Sync System
// ============================================================================

fn sync_night_stars(
    mut commands: Commands,
    mut state: ResMut<NightStarsState>,
    stars_query: Query<&NightStarsData>,
    camera_query: Query<&Transform, (With<Camera3d>, Without<renzora::IsolatedCamera>)>,
    // `Sun` moved to the contract crate; a plugin cannot link renzora_lighting.
    sun_query: Query<&renzora::Sun>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut star_materials: ResMut<Assets<NightStarsMaterial>>,
    has_data: Query<(), With<NightStarsData>>,
    mut removed: RemovedComponents<NightStarsData>,
) {
    let had_removals = removed.read().count() > 0;
    if had_removals && has_data.is_empty() {
        if let Some(ent) = state.entity.take() {
            commands.entity(ent).despawn();
            state.material_handle = None;
            state.mesh_handle = None;
        }
        return;
    }

    let Some(data) = stars_query.iter().next() else {
        return;
    };

    // Toggle off → tear down the dome but keep the settings component so
    // the user can re-enable in one click.
    if !data.enabled {
        if let Some(ent) = state.entity.take() {
            commands.entity(ent).despawn();
            state.material_handle = None;
            state.mesh_handle = None;
        }
        return;
    }

    let Some(camera_transform) = camera_query.iter().next() else {
        return;
    };

    let camera_pos = camera_transform.translation;

    // Sun elevation in radians for day/night fading (positive = above horizon)
    let sun_elevation = sun_query
        .iter()
        .next()
        .map(|s| s.elevation.to_radians())
        .unwrap_or(1.0); // default to daytime if no Sun component

    let params_a = Vec4::new(
        data.density,
        data.brightness,
        data.star_size,
        data.twinkle_speed,
    );
    let params_b = Vec4::new(data.twinkle_amount, data.horizon_fade, sun_elevation, 0.0);
    let star_color = LinearRgba::new(data.color.0, data.color.1, data.color.2, 1.0);

    if let Some(dome_entity) = state.entity {
        if commands.get_entity(dome_entity).is_ok() {
            if let Some(ref mat_handle) = state.material_handle {
                if let Some(mut mat) = star_materials.get_mut(mat_handle) {
                    mat.params_a = params_a;
                    mat.params_b = params_b;
                    mat.star_color = star_color;
                }
            }
            let transform = Transform::from_translation(camera_pos).with_scale(Vec3::splat(800.0));
            commands.entity(dome_entity).insert(transform);
        } else {
            state.entity = None;
            state.material_handle = None;
            state.mesh_handle = None;
        }
    }

    if state.entity.is_none() {
        let mesh_handle = meshes.add(Sphere::new(1.0).mesh().uv(64, 32));
        let material_handle = star_materials.add(NightStarsMaterial {
            params_a,
            params_b,
            star_color,
        });
        let transform = Transform::from_translation(camera_pos).with_scale(Vec3::splat(800.0));

        let dome_entity = commands
            .spawn((
                Mesh3d(mesh_handle.clone()),
                MeshMaterial3d(material_handle.clone()),
                transform,
                NightStarsDomeMarker,
                // Same guard as the cloud deck: `reject_unnamed_entities`
                // despawns any `Transform` entity with no `Name`, and enforces
                // always in a shipped game. Without this the starfield is
                // despawned and rebuilt every frame in an exported build.
                // `HideInHierarchy` rather than a `Name` because this is chrome,
                // and a name would serialise it into saved scenes.
                renzora::core::HideInHierarchy,
                bevy::light::NotShadowCaster,
                bevy::light::NotShadowReceiver,
            ))
            .id();

        state.entity = Some(dome_entity);
        state.mesh_handle = Some(mesh_handle);
        state.material_handle = Some(material_handle);
    }
}

// ============================================================================
// Plugin
// ============================================================================

#[derive(Default)]
pub struct NightStarsPlugin;

impl Plugin for NightStarsPlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "night_stars") {
            return;
        }
        info!("[runtime] NightStarsPlugin");
        bevy::asset::embedded_asset!(app, "night_stars.wgsl");

        app.register_type::<NightStarsData>()
            .init_resource::<NightStarsState>()
            .add_plugins(MaterialPlugin::<NightStarsMaterial>::default())
            .add_systems(Update, sync_night_stars);
    }
}

renzora::add!(NightStarsPlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::render::sync_world::SyncWorldPlugin,
        ));
        app.init_asset::<Mesh>();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(NightStarsPlugin);
        app
    }

    #[test]
    fn embedded_shader_resolves_with_unchanged_contents() {
        let app = app();
        let ShaderRef::Path(path) = NightStarsMaterial::fragment_shader() else {
            panic!("expected embedded shader");
        };
        let server = app.world().resource::<AssetServer>();
        let source = server.get_source(path.source()).unwrap();
        let mut reader = bevy::tasks::block_on(source.reader().read(path.path())).unwrap();
        let mut bytes = Vec::new();
        bevy::tasks::block_on(reader.read_to_end(&mut bytes)).unwrap();
        assert_eq!(bytes, include_bytes!("night_stars.wgsl"));
        assert_eq!(
            NightStarsMaterial::type_path(),
            "night_stars::NightStarsMaterial"
        );
    }

    #[test]
    fn dome_reuses_assets_follows_camera_and_disappears_when_disabled() {
        let mut app = app();
        let source = app.world_mut().spawn(NightStarsData::default()).id();
        let camera = app
            .world_mut()
            .spawn((Camera3d::default(), Transform::from_xyz(1.0, 2.0, 3.0)))
            .id();
        app.world_mut().run_system_once(sync_night_stars).unwrap();
        let state = app.world().resource::<NightStarsState>();
        let dome = state.entity.unwrap();
        let material = state.material_handle.clone().unwrap();
        let mesh = state.mesh_handle.clone().unwrap();
        assert!(app.world().get::<renzora::HideInHierarchy>(dome).is_some());
        assert_eq!(
            app.world().get::<Transform>(dome).unwrap().translation,
            Vec3::new(1.0, 2.0, 3.0)
        );
        app.world_mut()
            .get_mut::<Transform>(camera)
            .unwrap()
            .translation = Vec3::X;
        app.world_mut().run_system_once(sync_night_stars).unwrap();
        let state = app.world().resource::<NightStarsState>();
        assert_eq!(state.entity, Some(dome));
        assert_eq!(state.material_handle.as_ref(), Some(&material));
        assert_eq!(state.mesh_handle.as_ref(), Some(&mesh));
        assert_eq!(
            app.world().get::<Transform>(dome).unwrap().translation,
            Vec3::X
        );
        app.world_mut()
            .get_mut::<NightStarsData>(source)
            .unwrap()
            .enabled = false;
        app.world_mut().run_system_once(sync_night_stars).unwrap();
        assert!(app.world().resource::<NightStarsState>().entity.is_none());
        assert!(app.world().get_entity(dome).is_err());
    }

    #[test]
    fn saved_disable_preference_skips_material_and_system_installation() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["night_stars".into()]));
        app.add_plugins(NightStarsPlugin);
        assert!(!app.world().contains_resource::<NightStarsState>());
        app.update();
    }
}
