//! Pool water rendering and rippling height simulation.

pub mod material;
pub mod simulation;

use bevy::asset::embedded_asset;
use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::prepass::DepthPrepass;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::pbr::MaterialPlugin;
use bevy::prelude::*;
pub use renzora::pool_water::PoolWater;
use renzora::pool_water::{pool_mesh_subdivisions, pool_sim_dimension};

use material::{PoolWaterMaterial, PoolWaterUniforms};
use simulation::WaterSim;

// ── Components ────────────────────────────────────────────────────────────────

/// Marker on the parent entity linking to its water surface child.
#[derive(Component)]
pub struct PoolWaterLink(pub Entity);

/// Marker on the child water surface entity linking back to its pool parent.
#[derive(Component)]
pub struct PoolWaterSurface(pub Entity);

// ── Mesh generation ───────────────────────────────────────────────────────────

fn generate_pool_water_mesh(half_x: f32, half_z: f32, subdivisions: u32) -> Mesh {
    let subdivisions = pool_mesh_subdivisions(subdivisions);
    let verts_per_edge = subdivisions + 1;
    let total_verts = (verts_per_edge * verts_per_edge) as usize;
    let total_indices = (subdivisions * subdivisions * 6) as usize;

    let mut positions = Vec::with_capacity(total_verts);
    let mut normals = Vec::with_capacity(total_verts);
    let mut uvs = Vec::with_capacity(total_verts);
    let mut indices = Vec::with_capacity(total_indices);

    for z in 0..verts_per_edge {
        for x in 0..verts_per_edge {
            let fx = x as f32 / subdivisions as f32;
            let fz = z as f32 / subdivisions as f32;
            positions.push([
                -half_x + fx * half_x * 2.0,
                0.0,
                -half_z + fz * half_z * 2.0,
            ]);
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([fx, fz]);
        }
    }

    for z in 0..subdivisions {
        for x in 0..subdivisions {
            let tl = z * verts_per_edge + x;
            let tr = tl + 1;
            let bl = tl + verts_per_edge;
            let br = bl + 1;
            indices.push(tl);
            indices.push(bl);
            indices.push(tr);
            indices.push(tr);
            indices.push(bl);
            indices.push(br);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

// ── Systems ───────────────────────────────────────────────────────────────────

/// Auto-setup: spawn a water surface child entity inside the pool container.
fn setup_pool_water(
    mut commands: Commands,
    query: Query<(Entity, &PoolWater), Without<PoolWaterLink>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<PoolWaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (entity, pool) in query.iter() {
        // Unit-sized plane (0.5 half-extents) — inherits parent's XZ scale
        let mesh = meshes.add(generate_pool_water_mesh(0.5, 0.5, pool.mesh_subdivisions));

        let sim = WaterSim::new(
            pool_sim_dimension(pool.sim_resolution as usize),
            pool_sim_dimension(pool.sim_resolution as usize),
            pool.damping,
            pool.wave_speed,
            &mut images,
        );

        let mat = materials.add(PoolWaterMaterial {
            uniforms: PoolWaterUniforms::default(),
            heightfield: Some(sim.texture_handle.clone()),
        });

        // Spawn water surface as a child, positioned at the top face of the pool.
        // Uses a unit-sized plane (1x1) so it inherits the parent's XZ scale.
        // Positioned at y=0.5 (top of a unit cube) with a tiny offset to avoid z-fight.
        let surface_id = commands
            .spawn((
                Name::new("Water Surface"),
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(Vec3::new(0.0, 0.5 - pool.water_level, 0.0)),
                sim,
                PoolWaterSurface(entity),
                // The height is local to the pool, including scaled/rotated
                // parents; preserving an unpropagated world transform loses it.
                ChildOf(entity),
            ))
            .id();

        commands.entity(entity).insert(PoolWaterLink(surface_id));
    }
}

/// Apply authored structural edits without rebuilding unchanged surfaces.
fn sync_pool_water_settings(
    pools: Query<&PoolWater>,
    mut surfaces: Query<(
        &PoolWaterSurface,
        &mut WaterSim,
        &mut Transform,
        &mut Mesh3d,
        &MeshMaterial3d<PoolWaterMaterial>,
    )>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<PoolWaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    for (surface, mut sim, mut transform, mut mesh, material) in &mut surfaces {
        let Ok(pool) = pools.get(surface.0) else {
            continue;
        };
        let height = 0.5 - pool.water_level;
        if transform.translation.y != height {
            transform.translation.y = height;
        }
        let resolution = pool_sim_dimension(pool.sim_resolution as usize);
        if sim.width != resolution || sim.height != resolution {
            // A different grid restarts the ripple state. Keep the surface and
            // material identities; the old texture is released through handles.
            *sim = WaterSim::new(
                resolution,
                resolution,
                pool.damping,
                pool.wave_speed,
                &mut images,
            );
            if let Some(mut material) = materials.get_mut(&material.0) {
                material.heightfield = Some(sim.texture_handle.clone());
            }
        }
        if sim.damping != pool.damping {
            sim.damping = pool.damping;
        }
        if sim.speed != pool.wave_speed {
            sim.speed = pool.wave_speed;
        }
        let subdivisions = pool_mesh_subdivisions(pool.mesh_subdivisions);
        let expected_vertices = (u64::from(subdivisions) + 1).pow(2);
        if meshes
            .get(&mesh.0)
            .is_none_or(|mesh| mesh.count_vertices() as u64 != expected_vertices)
        {
            let replacement = generate_pool_water_mesh(0.5, 0.5, subdivisions);
            if meshes.contains(mesh.0.id()) {
                *meshes.get_mut(&mesh.0).expect("mesh checked above") = replacement;
            } else {
                mesh.0 = meshes.add(replacement);
            }
        }
    }
}

/// Clean up water surface when PoolWater is removed.
fn cleanup_pool_water(
    mut commands: Commands,
    mut removed: RemovedComponents<PoolWater>,
    links: Query<&PoolWaterLink>,
    pools: Query<(), With<PoolWater>>,
) {
    for entity in removed.read() {
        if pools.contains(entity) {
            continue;
        }
        if let Ok(link) = links.get(entity) {
            if let Ok(mut ec) = commands.get_entity(link.0) {
                ec.despawn();
            }
        }
        if let Ok(mut ec) = commands.get_entity(entity) {
            ec.remove::<PoolWaterLink>();
        }
    }
}

/// Ensure cameras have DepthPrepass for depth-based water effects.
fn ensure_depth_prepass(
    mut commands: Commands,
    cameras: Query<Entity, (With<Camera3d>, Without<DepthPrepass>)>,
    pool_exists: Query<(), With<PoolWater>>,
) {
    if pool_exists.is_empty() {
        return;
    }
    for entity in cameras.iter() {
        commands.entity(entity).try_insert(DepthPrepass);
    }
}

/// Step simulation, upload texture, sync uniforms every frame.
fn update_pool_water(
    time: Res<Time>,
    pool_query: Query<(&PoolWater, &PoolWaterLink)>,
    mut surface_query: Query<(
        &mut WaterSim,
        &MeshMaterial3d<PoolWaterMaterial>,
        &PoolWaterSurface,
    )>,
    mut materials: ResMut<Assets<PoolWaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
    sun_query: Query<&GlobalTransform, With<DirectionalLight>>,
) {
    let t = time.elapsed_secs();
    let dt = time.delta_secs();

    let sun_dir = sun_query
        .iter()
        .next()
        .map(|tr| tr.forward().as_vec3())
        .unwrap_or(Vec3::new(-0.3, -0.7, -0.4).normalize());

    for (mut sim, mat_handle, surface) in surface_query.iter_mut() {
        let Ok((pool, _)) = pool_query.get(surface.0) else {
            continue;
        };

        // Step simulation
        sim.step();
        sim.upload(&mut images);

        // Add ambient ripples (very subtle)
        if (t * 2.0).fract() < dt * 2.0 {
            let rx = hash_f32(t * 13.7) * 0.8 + 0.1;
            let ry = hash_f32(t * 17.3) * 0.8 + 0.1;
            sim.add_drop(rx, ry, 0.03, 0.01);
        }

        // Sync material uniforms
        if let Some(mut mat) = materials.get_mut(&mat_handle.0) {
            mat.uniforms.light_direction = Vec4::new(sun_dir.x, sun_dir.y, sun_dir.z, 0.0);
            mat.uniforms.ior = pool.ior;
            mat.uniforms.fresnel_min = pool.fresnel_min;
            mat.uniforms.caustic_intensity = pool.caustic_intensity;
            mat.uniforms.time = t;
            mat.uniforms.height_scale = pool.height_scale;
            mat.uniforms.specular_power = pool.specular_power;
            mat.uniforms.refraction_strength = pool.refraction_strength;
            mat.uniforms.max_depth = pool.max_depth;

            let dc = pool.deep_color;
            mat.uniforms.deep_color = Vec4::new(dc[0], dc[1], dc[2], 1.0);
            let sc = pool.shallow_color;
            mat.uniforms.shallow_color = Vec4::new(sc[0], sc[1], sc[2], 1.0);
            let fc = pool.foam_color;
            mat.uniforms.foam_color = Vec4::new(fc[0], fc[1], fc[2], 1.0);
            mat.uniforms.absorption = Vec4::new(
                pool.absorption_r,
                pool.absorption_g,
                pool.absorption_b,
                pool.foam_depth,
            );
        }
    }
}

/// Public API: add a drop to a specific pool water entity.
pub fn add_drop_to_pool(
    entity: Entity,
    sim_query: &mut Query<&mut WaterSim>,
    uv_x: f32,
    uv_y: f32,
    radius: f32,
    strength: f32,
) {
    if let Ok(mut sim) = sim_query.get_mut(entity) {
        sim.add_drop(uv_x, uv_y, radius, strength);
    }
}

fn hash_f32(x: f32) -> f32 {
    let s = (x * 127.1 + 311.7).sin() * 43_758.547;
    s.fract()
}

// ── Plugin ────────────────────────────────────────────────────────────────────

/// Install pool water rendering and simulation without inspector dependencies.
#[derive(Default)]
pub struct PoolWaterPlugin;

impl Plugin for PoolWaterPlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "pool_water") {
            return;
        }

        embedded_asset!(app, "pool_water.wgsl");

        app.add_plugins(MaterialPlugin::<PoolWaterMaterial>::default())
            .register_type::<PoolWater>()
            .add_systems(
                Update,
                (
                    ensure_depth_prepass,
                    cleanup_pool_water,
                    setup_pool_water.after(cleanup_pool_water),
                    sync_pool_water_settings.after(setup_pool_water),
                    update_pool_water.after(sync_pool_water_settings),
                ),
            );
    }
}

renzora::add!(PoolWaterPlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::AssetPlugin;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::render::sync_world::SyncWorldPlugin;
    use bevy::shader::ShaderRef;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default(), SyncWorldPlugin));
        app.init_asset::<Mesh>().init_asset::<Image>();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(PoolWaterPlugin);
        app
    }

    #[test]
    fn shader_resolves_and_material_identity_is_preserved() {
        let app = app();
        for shader in [
            PoolWaterMaterial::vertex_shader(),
            PoolWaterMaterial::fragment_shader(),
        ] {
            let ShaderRef::Path(path) = shader else {
                panic!("expected embedded shader");
            };
            let source = app
                .world()
                .resource::<AssetServer>()
                .get_source(path.source())
                .unwrap();
            let mut reader = bevy::tasks::block_on(source.reader().read(path.path())).unwrap();
            let mut bytes = Vec::new();
            bevy::tasks::block_on(reader.read_to_end(&mut bytes)).unwrap();
            assert_eq!(bytes, include_bytes!("pool_water.wgsl"));
        }
        assert_eq!(
            PoolWaterMaterial::type_path(),
            "pool_water::material::PoolWaterMaterial"
        );
    }

    #[test]
    fn surface_ripples_reuse_assets_and_cleanup_with_parent_settings() {
        let mut app = app();
        let pool = app
            .world_mut()
            .spawn((
                PoolWater {
                    mesh_subdivisions: 4,
                    sim_resolution: 8,
                    ..default()
                },
                Transform::default(),
            ))
            .id();
        let camera = app.world_mut().spawn(Camera3d::default()).id();
        app.world_mut().run_system_once(setup_pool_water).unwrap();
        app.world_mut()
            .run_system_once(ensure_depth_prepass)
            .unwrap();
        assert!(app.world().get::<DepthPrepass>(camera).is_some());
        let surface = app.world().get::<PoolWaterLink>(pool).unwrap().0;
        assert_eq!(app.world().get::<ChildOf>(surface).unwrap().parent(), pool);
        assert_eq!(
            app.world().get::<Transform>(surface).unwrap().translation.y,
            0.45
        );
        let mesh = app.world().get::<Mesh3d>(surface).unwrap().0.clone();
        assert_eq!(
            app.world()
                .resource::<Assets<Mesh>>()
                .get(&mesh)
                .unwrap()
                .count_vertices(),
            25
        );
        let material = app
            .world()
            .get::<MeshMaterial3d<PoolWaterMaterial>>(surface)
            .unwrap()
            .0
            .clone();
        let texture = app
            .world()
            .get::<WaterSim>(surface)
            .unwrap()
            .texture_handle
            .clone();
        app.world_mut()
            .get_mut::<WaterSim>(surface)
            .unwrap()
            .add_drop(0.5, 0.5, 0.4, 0.1);
        app.world_mut().run_system_once(update_pool_water).unwrap();
        let sim = app.world().get::<WaterSim>(surface).unwrap();
        assert!(sim.heights.iter().all(|value| value.is_finite()));
        assert!(sim.heights.iter().any(|value| *value != 0.0));
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&texture)
            .unwrap();
        assert!(image.data.as_ref().unwrap().iter().any(|byte| *byte != 0));
        app.world_mut().run_system_once(setup_pool_water).unwrap();
        assert_eq!(app.world().get::<PoolWaterLink>(pool).unwrap().0, surface);
        assert_eq!(app.world().get::<Mesh3d>(surface).unwrap().0, mesh);
        assert_eq!(
            app.world()
                .get::<MeshMaterial3d<PoolWaterMaterial>>(surface)
                .unwrap()
                .0,
            material
        );
        app.world_mut().entity_mut(pool).remove::<PoolWater>();
        app.world_mut().run_system_once(cleanup_pool_water).unwrap();
        assert!(app.world().get_entity(surface).is_err());
        assert!(app.world().get::<PoolWaterLink>(pool).is_none());
    }

    #[test]
    fn height_is_local_to_translated_rotated_scaled_parent() {
        let mut app = app();
        app.add_plugins(bevy::transform::TransformPlugin);
        let parent = Transform::from_xyz(5.0, 7.0, -3.0)
            .with_rotation(Quat::from_rotation_z(0.4))
            .with_scale(Vec3::new(2.0, 3.0, 4.0));
        let pool = app
            .world_mut()
            .spawn((
                PoolWater {
                    mesh_subdivisions: 2,
                    sim_resolution: 4,
                    ..default()
                },
                parent,
            ))
            .id();
        app.update();
        let surface = app.world().get::<PoolWaterLink>(pool).unwrap().0;
        let expected = parent.transform_point(Vec3::new(0.0, 0.45, 0.0));
        assert!(app
            .world()
            .get::<GlobalTransform>(surface)
            .unwrap()
            .translation()
            .abs_diff_eq(expected, 1e-5));
    }

    #[test]
    fn extreme_grid_sizes_are_bounded_and_do_not_reallocate_each_frame() {
        let mut app = app();
        let pool = app
            .world_mut()
            .spawn((
                PoolWater {
                    sim_resolution: u32::MAX,
                    mesh_subdivisions: u32::MAX,
                    ..default()
                },
                Transform::default(),
            ))
            .id();
        app.update();
        let surface = app.world().get::<PoolWaterLink>(pool).unwrap().0;
        let sim = app.world().get::<WaterSim>(surface).unwrap();
        assert_eq!((sim.width, sim.height), (1024, 1024));
        assert_eq!(sim.heights.len(), 1024 * 1024);
        let texture = sim.texture_handle.clone();
        let storage = sim.heights.as_ptr();
        let mesh = app.world().get::<Mesh3d>(surface).unwrap().0.clone();
        assert_eq!(
            app.world()
                .resource::<Assets<Mesh>>()
                .get(&mesh)
                .unwrap()
                .count_vertices(),
            513 * 513
        );
        for _ in 0..3 {
            app.update();
            let sim = app.world().get::<WaterSim>(surface).unwrap();
            assert_eq!(sim.texture_handle, texture);
            assert_eq!(sim.heights.as_ptr(), storage);
        }
        app.world_mut()
            .get_mut::<PoolWater>(pool)
            .unwrap()
            .sim_resolution = 0;
        app.world_mut()
            .get_mut::<PoolWater>(pool)
            .unwrap()
            .mesh_subdivisions = 0;
        app.update();
        assert_eq!(
            app.world().get::<WaterSim>(surface).unwrap().heights.len(),
            1
        );
        assert_eq!(
            app.world()
                .resource::<Assets<Mesh>>()
                .get(&mesh)
                .unwrap()
                .count_vertices(),
            4
        );
        let mut images = Assets::<Image>::default();
        let sim = WaterSim::new(usize::MAX, 0, 0.99, 2.0, &mut images);
        assert_eq!((sim.width, sim.height), (1024, 1));
    }

    #[test]
    fn live_settings_reuse_surface_material_and_unchanged_storage() {
        let mut app = app();
        let pool = app
            .world_mut()
            .spawn((
                PoolWater {
                    mesh_subdivisions: 2,
                    sim_resolution: 4,
                    ..default()
                },
                Transform::default(),
            ))
            .id();
        app.update();
        let surface = app.world().get::<PoolWaterLink>(pool).unwrap().0;
        let mesh = app.world().get::<Mesh3d>(surface).unwrap().0.clone();
        let material = app
            .world()
            .get::<MeshMaterial3d<PoolWaterMaterial>>(surface)
            .unwrap()
            .0
            .clone();
        let texture = app
            .world()
            .get::<WaterSim>(surface)
            .unwrap()
            .texture_handle
            .clone();
        {
            let mut settings = app.world_mut().get_mut::<PoolWater>(pool).unwrap();
            settings.water_level = 0.2;
            settings.damping = 0.98;
            settings.wave_speed = 1.5;
            settings.mesh_subdivisions = 3;
        }
        app.update();
        assert_eq!(
            app.world().get::<Transform>(surface).unwrap().translation.y,
            0.3
        );
        assert_eq!(app.world().get::<WaterSim>(surface).unwrap().damping, 0.98);
        assert_eq!(app.world().get::<WaterSim>(surface).unwrap().speed, 1.5);
        assert_eq!(
            app.world().get::<WaterSim>(surface).unwrap().texture_handle,
            texture
        );
        assert_eq!(
            app.world()
                .resource::<Assets<Mesh>>()
                .get(&mesh)
                .unwrap()
                .count_vertices(),
            16
        );
        app.world_mut()
            .get_mut::<PoolWater>(pool)
            .unwrap()
            .sim_resolution = 8;
        app.update();
        let sim = app.world().get::<WaterSim>(surface).unwrap();
        assert_eq!(sim.heights.len(), 64);
        assert_eq!((sim.width, sim.height), (8, 8));
        let new_texture = sim.texture_handle.clone();
        let height_storage = sim.heights.as_ptr();
        assert_ne!(new_texture, texture);
        assert_eq!(
            app.world()
                .resource::<Assets<PoolWaterMaterial>>()
                .get(&material)
                .unwrap()
                .heightfield
                .as_ref(),
            Some(&new_texture)
        );
        for _ in 0..100 {
            app.update();
        }
        assert_eq!(app.world().get::<PoolWaterLink>(pool).unwrap().0, surface);
        assert_eq!(app.world().get::<Mesh3d>(surface).unwrap().0, mesh);
        assert_eq!(
            app.world()
                .get::<MeshMaterial3d<PoolWaterMaterial>>(surface)
                .unwrap()
                .0,
            material
        );
        assert_eq!(
            app.world()
                .get::<WaterSim>(surface)
                .unwrap()
                .heights
                .as_ptr(),
            height_storage
        );
        assert_eq!(
            app.world().get::<WaterSim>(surface).unwrap().texture_handle,
            new_texture
        );
        let settings = app.world().get::<PoolWater>(pool).unwrap().clone();
        app.world_mut()
            .entity_mut(pool)
            .remove::<PoolWater>()
            .insert(settings);
        app.update();
        assert_eq!(app.world().get::<PoolWaterLink>(pool).unwrap().0, surface);
    }

    #[test]
    fn disabled_startup_does_not_install_material_or_systems() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["pool_water".into()]));
        app.add_plugins(PoolWaterPlugin);
        assert!(!app.is_plugin_added::<MaterialPlugin<PoolWaterMaterial>>());
        app.update();
    }
}
