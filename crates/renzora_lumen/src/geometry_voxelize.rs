//! Geometry voxelization — Phase 2 follow-up.
//!
//! Bakes a sparse set of world-space sample points per mesh, then each
//! frame injects those samples (multiplied by their material's base
//! color) into the voxel radiance accumulation buffer alongside the
//! visible-surface inject. Result: voxels carry geometry data even for
//! surfaces never visible from the camera, which is what Phase 5 needs
//! to ray-trace through the cache.
//!
//! V1 limitations (deferred to follow-ups):
//!   - StandardMaterial.base_color only; no albedo texture sampling.
//!   - Static meshes only; skinned/morphed meshes ignore deformation.
//!   - CPU samples are still transformed every active frame. Storage is
//!     retained and unchanged uploads are skipped; per-mesh persistent
//!     buffers would be needed to avoid the transform work itself.
//!   - No occlusion bit yet — voxels just get color contributions, so
//!     inside-mesh "solid air" isn't represented. Phase 5's ray tracer
//!     will need an additional alpha/density signal.

use bevy::core_pipeline::Core3d;
use bevy::prelude::*;
use bevy::mesh::{Indices, Mesh, VertexAttributeValues};
use bevy::render::render_resource::binding_types::{storage_buffer_read_only_sized, storage_buffer_sized, uniform_buffer};
use bevy::render::render_resource::*;
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::Extract;
use bevy::render::{Render, RenderApp, RenderSystems};
use bytemuck::{Pod, Zeroable};

use crate::voxel_cache::{VoxelCacheResources, VoxelCacheView, VoxelGridUniform, CASCADE_COUNT, VOXEL_RES};

/// Approximate spacing between sample points along a triangle's
/// surface, in world units. 0.75m = half the voxel size (0.5m), so
/// every voxel a sufficiently-large triangle passes through gets at
/// least one sample with high probability. Combined with the resolve
/// pass's neighbor dilation this gives Phase 5's ray tracer enough
/// coverage to hit visible-from-anywhere geometry.
const SAMPLE_SPACING: f32 = 0.75;

/// Hard cap on samples per mesh so a particularly dense mesh doesn't
/// blow out the per-frame buffer.
const MAX_SAMPLES_PER_MESH: usize = 2048;

/// Beyond this distance from the camera, entities are skipped at
/// extract time. The voxel grid extends 16m from camera so 24m gives
/// a generous skirt for big meshes whose origin is far from their
/// actual geometry (e.g. terrain chunks).
const CULL_RADIUS: f32 = 24.0;

/// Cap on total samples uploaded per frame across all entities. Each
/// sample is 32 bytes; 200k samples = 6.4 MB/frame upload. Bumped from
/// 100k together with sample density so denser scenes get covered.
const MAX_SAMPLES_PER_FRAME: usize = 200_000;

/// Per-mesh-instance baked sample list. Lives on the entity (not the
/// asset) so per-instance albedo overrides work cleanly. Re-baked when
/// the mesh asset or material handle changes.
#[derive(Component, Clone, Default)]
pub struct MeshVoxelSamples {
    pub local_positions: Vec<Vec3>,
    pub albedo: LinearRgba,
}

/// Keeps transient handle-change work eligible across the bake budget.
#[derive(Component)]
struct PendingVoxelBake;

fn event_asset_id<A: Asset>(event: &AssetEvent<A>) -> AssetId<A> {
    match *event {
        AssetEvent::Added { id } | AssetEvent::Modified { id }
        | AssetEvent::Removed { id } | AssetEvent::Unused { id }
        | AssetEvent::LoadedWithDependencies { id } => id,
    }
}

fn invalidate_baked_assets(
    mut commands: Commands,
    mut meshes: MessageReader<AssetEvent<Mesh>>,
    mut materials: MessageReader<AssetEvent<StandardMaterial>>,
    mut graphs: MessageReader<AssetEvent<renzora_shader::material::GraphMaterial>>,
    samples: Query<(Entity, &Mesh3d, Option<&MeshMaterial3d<StandardMaterial>>,
        Option<&MeshMaterial3d<renzora_shader::material::GraphMaterial>>),
        (With<MeshVoxelSamples>, Allow<bevy::ecs::entity_disabling::Disabled>)>,
) {
    let meshes: std::collections::HashSet<_> = meshes.read().map(event_asset_id).collect();
    let materials: std::collections::HashSet<_> = materials.read().map(event_asset_id).collect();
    let graphs: std::collections::HashSet<_> = graphs.read().map(event_asset_id).collect();
    if meshes.is_empty() && materials.is_empty() && graphs.is_empty() {
        return;
    }
    for (entity, mesh, material, graph) in &samples {
        if meshes.contains(&mesh.id())
            || material.is_some_and(|m| materials.contains(&m.id()))
            || graph.is_some_and(|m| graphs.contains(&m.id()))
        {
            commands.entity(entity).try_insert(PendingVoxelBake);
        }
    }
}

/// Per-frame stats for the CPU bake throttle in
/// `bake_mesh_samples`. Surfaces in the debugger's Lumen panel so you
/// can see "how many meshes did we bake this frame, how long did it
/// take, how many samples did each produce on average".
///
/// GPU-side pass durations (inject + resolve) live in the render world
/// and aren't recorded here — use the Render Stats / Tracy panels for
/// those.
#[derive(bevy::prelude::Resource, Default, Clone)]
pub struct LumenBakeStats {
    /// Wall-clock of the last `bake_mesh_samples` system call.
    pub last_bake_dur: std::time::Duration,
    /// Rolling average over the last ~60 frames.
    pub avg_bake_dur: std::time::Duration,
    /// Worst single-frame bake observed this session.
    pub max_bake_dur: std::time::Duration,
    /// Entities baked in the last frame (0..=MAX_BAKES_PER_FRAME).
    pub bakes_last_frame: usize,
    /// Lifetime count of meshes baked.
    pub total_bakes: u64,
    /// Lifetime sum of sample points emitted across all bakes.
    pub total_samples_baked: u64,
    /// Capacity of `MAX_BAKES_PER_FRAME` so the panel can show
    /// "saturated" when the throttle is the bottleneck.
    pub bake_budget_per_frame: usize,
    /// Internal rolling-average ring buffer. Skipped by the panel.
    recent_durs: std::collections::VecDeque<std::time::Duration>,
    recent_total: std::time::Duration,
}

impl LumenBakeStats {
    pub(crate) fn record(&mut self, dur: std::time::Duration, bakes: usize, samples: u64) {
        self.last_bake_dur = dur;
        self.bakes_last_frame = bakes;
        self.total_bakes += bakes as u64;
        self.total_samples_baked += samples;
        if dur > self.max_bake_dur {
            self.max_bake_dur = dur;
        }
        // Keep diagnostic overhead bounded without shifting or summing history.
        if self.recent_durs.len() == 60 {
            self.recent_total -= self.recent_durs.pop_front().expect("history is full");
        }
        self.recent_durs.push_back(dur);
        self.recent_total += dur;
        self.avg_bake_dur = self.recent_total / self.recent_durs.len() as u32;
        self.bake_budget_per_frame = MAX_BAKES_PER_FRAME;
    }
}

/// Maximum number of entities that get their samples baked in a single
/// frame. Bake walks the mesh's triangle list which can be expensive
/// (a 4k-triangle mesh at 0.75m spacing can produce thousands of
/// samples). Without a cap, the first time the user flies into a new
/// area we'd bake hundreds of newly-visible entities in one frame,
/// stalling the main loop for seconds and the OS would release mouse
/// capture as a not-responding recovery.
const MAX_BAKES_PER_FRAME: usize = 4;

/// Bakes voxel samples for `Mesh3d` entities backed by either:
///   - `MeshMaterial3d<StandardMaterial>` directly, or
///   - `MeshMaterial3d<GraphMaterial>` (renzora_shader's node-graph
///     wrapper, which is `ExtendedMaterial<StandardMaterial, ...>`).
///
/// Throttled to `MAX_BAKES_PER_FRAME` entities; the rest get picked
/// up on subsequent frames as the query keeps yielding them.
#[allow(clippy::too_many_arguments)]
fn bake_mesh_samples(
    mut commands: Commands,
    meshes: Res<Assets<Mesh>>,
    standard_materials: Res<Assets<StandardMaterial>>,
    graph_materials: Res<Assets<renzora_shader::material::GraphMaterial>>,
    mut stats: ResMut<LumenBakeStats>,
    standard_query: Query<
        (
            Entity,
            &Mesh3d,
            &MeshMaterial3d<StandardMaterial>,
        ),
        Or<(Without<MeshVoxelSamples>, With<PendingVoxelBake>, Changed<Mesh3d>, Changed<MeshMaterial3d<StandardMaterial>>)>,
    >,
    graph_query: Query<
        (
            Entity,
            &Mesh3d,
            &MeshMaterial3d<renzora_shader::material::GraphMaterial>,
        ),
        Or<(
            Without<MeshVoxelSamples>,
            With<PendingVoxelBake>,
            Changed<Mesh3d>,
            Changed<MeshMaterial3d<renzora_shader::material::GraphMaterial>>,
        )>,
    >,
) {
    // `bevy::platform::time::Instant`, never `std`'s — std's panics on wasm.
    let start = bevy::platform::time::Instant::now();
    let mut budget = MAX_BAKES_PER_FRAME;
    let mut bakes = 0usize;
    let mut samples_emitted: u64 = 0;
    for (entity, mesh_handle, mat_handle) in &standard_query {
        if budget == 0 || !meshes.contains(&mesh_handle.0) {
            commands.entity(entity).try_insert(PendingVoxelBake);
            continue;
        }
        let mesh = meshes.get(&mesh_handle.0).expect("mesh checked above");
        let albedo = standard_materials
            .get(&mat_handle.0)
            .map(|m| m.base_color.to_linear())
            .unwrap_or(LinearRgba::WHITE);
        samples_emitted += bake_one(&mut commands, entity, mesh, albedo) as u64;
        bakes += 1;
        budget -= 1;
    }

    for (entity, mesh_handle, mat_handle) in &graph_query {
        if budget == 0 || !meshes.contains(&mesh_handle.0) {
            commands.entity(entity).try_insert(PendingVoxelBake);
            continue;
        }
        let mesh = meshes.get(&mesh_handle.0).expect("mesh checked above");
        let albedo = graph_materials
            .get(&mat_handle.0)
            .map(|m| m.base.base_color.to_linear())
            .unwrap_or(LinearRgba::WHITE);
        samples_emitted += bake_one(&mut commands, entity, mesh, albedo) as u64;
        bakes += 1;
        budget -= 1;
    }

    stats.record(start.elapsed(), bakes, samples_emitted);
}

/// Returns the number of sample positions emitted. Empty results are retained
/// too, so they do not consume the bake budget again every frame.
fn bake_one(
    commands: &mut Commands,
    entity: Entity,
    mesh: &Mesh,
    albedo: LinearRgba,
) -> usize {
    let local_positions = sample_mesh_surface(mesh, SAMPLE_SPACING);
    let n = local_positions.len();
    // `try_insert`: the mesh entity can be despawned (scene reload, deletion,
    // play-mode cleanup) between the query and this command flushing — skip it
    // gracefully rather than panicking on a stale entity.
    commands.entity(entity).try_insert(MeshVoxelSamples {
        local_positions,
        albedo,
    });
    commands.entity(entity).try_remove::<PendingVoxelBake>();
    n
}

/// Generate sample points across the mesh's surface, spaced roughly
/// `spacing` apart. Stratified per-triangle: each triangle gets a
/// number of samples proportional to its area, with deterministic
/// pseudo-random barycentric offsets so the bake is reproducible.
fn sample_mesh_surface(mesh: &Mesh, spacing: f32) -> Vec<Vec3> {
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return Vec::new();
    };
    let Some(indices) = mesh.indices() else {
        // Unindexed meshes — just sample every triangle as 3 sequential
        // verts.
        return sample_unindexed(positions, spacing);
    };

    let mut samples = Vec::new();
    let tri_iter = match indices {
        Indices::U16(v) => Box::new(v.chunks_exact(3).map(|c| {
            (c[0] as usize, c[1] as usize, c[2] as usize)
        })) as Box<dyn Iterator<Item = (usize, usize, usize)>>,
        Indices::U32(v) => Box::new(v.chunks_exact(3).map(|c| {
            (c[0] as usize, c[1] as usize, c[2] as usize)
        })) as Box<dyn Iterator<Item = (usize, usize, usize)>>,
    };

    let voxel_area = spacing * spacing;
    for (i0, i1, i2) in tri_iter {
        if i0 >= positions.len() || i1 >= positions.len() || i2 >= positions.len() {
            continue;
        }
        let p0 = Vec3::from(positions[i0]);
        let p1 = Vec3::from(positions[i1]);
        let p2 = Vec3::from(positions[i2]);
        let area = (p1 - p0).cross(p2 - p0).length() * 0.5;
        if area < 1e-8 {
            continue;
        }
        let n = ((area / voxel_area).ceil() as usize).clamp(1, 64);

        for sample_idx in 0..n {
            let p = barycentric_sample(p0, p1, p2, sample_idx as u32);
            samples.push(p);
            if samples.len() >= MAX_SAMPLES_PER_MESH {
                return samples;
            }
        }
    }
    samples
}

fn sample_unindexed(positions: &[[f32; 3]], spacing: f32) -> Vec<Vec3> {
    let mut samples = Vec::new();
    let voxel_area = spacing * spacing;
    for tri in positions.chunks_exact(3) {
        let p0 = Vec3::from(tri[0]);
        let p1 = Vec3::from(tri[1]);
        let p2 = Vec3::from(tri[2]);
        let area = (p1 - p0).cross(p2 - p0).length() * 0.5;
        if area < 1e-8 {
            continue;
        }
        let n = ((area / voxel_area).ceil() as usize).clamp(1, 64);
        for sample_idx in 0..n {
            let p = barycentric_sample(p0, p1, p2, sample_idx as u32);
            samples.push(p);
            if samples.len() >= MAX_SAMPLES_PER_MESH {
                return samples;
            }
        }
    }
    samples
}

fn barycentric_sample(p0: Vec3, p1: Vec3, p2: Vec3, seed: u32) -> Vec3 {
    let s = seed
        .wrapping_mul(2654435761)
        .wrapping_add(0x9E3779B9);
    let u = ((s ^ (s >> 16)) & 0xFFFF) as f32 / 65535.0;
    let v = ((s.wrapping_mul(1597334677) >> 8) & 0xFFFF) as f32 / 65535.0;
    let (mut u, mut v) = (u, v);
    if u + v > 1.0 {
        u = 1.0 - u;
        v = 1.0 - v;
    }
    let w = 1.0 - u - v;
    p0 * w + p1 * u + p2 * v
}

// ─── Render world ──────────────────────────────────────────────────

/// Flat per-frame GPU storage layout. Each entry is one sample:
/// (world_x, world_y, world_z, _pad, albedo_r, albedo_g, albedo_b, _pad).
#[derive(Clone, Copy, Pod, Zeroable, ShaderType)]
#[repr(C)]
pub struct GpuSample {
    pub world_pos: [f32; 4],
    pub albedo: [f32; 4],
}

#[derive(Resource, Default)]
pub struct GeometrySampleBuffer {
    pub buffer: Option<Buffer>,
    pub capacity_bytes: u64,
    pub count: u32,
}

#[derive(Resource)]
pub struct GeometryInjectPipeline {
    pub layout: BindGroupLayoutDescriptor,
    pub pipeline_id: CachedComputePipelineId,
}

impl FromWorld for GeometryInjectPipeline {
    fn from_world(world: &mut World) -> Self {
        // Matches the accum buffer in voxel_cache (5 u32 per voxel per
        // cascade — geometry inject loops over cascades in the shader).
        let accum_size = (VOXEL_RES * VOXEL_RES * VOXEL_RES * CASCADE_COUNT * 5 * 4) as u64;
        let accum_size_nz = std::num::NonZeroU64::new(accum_size).unwrap();

        let layout = BindGroupLayoutDescriptor::new(
            "voxel_geo_inject_layout",
            &BindGroupLayoutEntries::sequential(
                ShaderStages::COMPUTE,
                (
                    // 0: sample list (read-only storage)
                    storage_buffer_read_only_sized(false, None),
                    // 1: voxel accumulation buffer (atomic adds)
                    storage_buffer_sized(false, Some(accum_size_nz)),
                    // 2: voxel grid uniform
                    uniform_buffer::<VoxelGridUniform>(false),
                ),
            ),
        );

        let asset_server = world.resource::<AssetServer>();
        let shader = asset_server.load("embedded://renzora_lumen/voxel_geo_inject.wgsl");
        let pipeline_cache = world.resource::<PipelineCache>();
        let pipeline_id = pipeline_cache.queue_compute_pipeline(ComputePipelineDescriptor {
            label: Some("voxel_geo_inject_pipeline".into()),
            layout: vec![layout.clone()],
            shader,
            shader_defs: vec![],
            entry_point: Some("inject".into()),
            immediate_size: 0,
            zero_initialize_workgroup_memory: false,
        });

        Self { layout, pipeline_id }
    }
}

/// Refresh retained world-space samples, notifying preparation only when bytes change.
pub fn extract_geometry_samples(
    mut extracted: ResMut<ExtractedGeometrySamples>,
    query: Extract<Query<(&MeshVoxelSamples, &GlobalTransform)>>,
    cameras: Extract<Query<(&GlobalTransform, &VoxelCacheView), With<Camera3d>>>,
) {
    let _span = info_span!("geometry.extract_samples").entered();
    // Pick the camera that actually wants GI as the cull pivot — the
    // one with `VoxelCacheView.inject_active = true`. Used to be
    // `cameras.iter().next()` on all Camera3d, which was
    // non-deterministic: ECS archetype storage order could put a
    // preview/thumbnail camera at default position (0,0,0) first, the
    // 24m cull sphere then landed on empty space, and every mesh got
    // culled → cache empty. Symptom: SDF GI silently broken until the
    // user toggled some other camera, which shifted archetype order
    // and accidentally selected a useful pivot.
    //
    // No inject-active camera at all (GI off, or every viewport hidden) →
    // skip the flatten + upload entirely. Baked `MeshVoxelSamples` persist
    // on entities, so without this gate the editor kept re-uploading up to
    // 6.4 MB of samples every frame even when nothing consumed them.
    let camera_pos = cameras
        .iter()
        .find(|(_, v)| v.inject_active)
        .map(|(t, _)| t.translation());
    // Preserve the change tick when the exact upload payload is unchanged.
    let changed = refresh_geometry_samples(
        &mut extracted.bypass_change_detection().0,
        query.iter(),
        camera_pos,
    );
    if changed {
        extracted.set_changed();
    }
}

fn refresh_geometry_samples<'a>(
    samples: &mut Vec<GpuSample>,
    query: impl IntoIterator<Item = (&'a MeshVoxelSamples, &'a GlobalTransform)>,
    camera_pos: Option<Vec3>,
) -> bool {
    let Some(camera_pos) = camera_pos else {
        let changed = !samples.is_empty();
        samples.clear();
        return changed;
    };
    let cull_sq = CULL_RADIUS * CULL_RADIUS;
    let mut count = 0;
    let mut changed = false;
    for (mesh_samples, transform) in query {
        if count >= MAX_SAMPLES_PER_FRAME {
            break;
        }
        if mesh_samples.local_positions.is_empty() {
            continue;
        }

        // Cheap cull: entity origin → camera distance. Misses big
        // meshes whose origin is far from their geometry, but for a
        // city of separated buildings it's a 10-100× win.
        let entity_pos = transform.translation();
        if entity_pos.distance_squared(camera_pos) > cull_sq {
            continue;
        }

        let albedo = [
            mesh_samples.albedo.red,
            mesh_samples.albedo.green,
            mesh_samples.albedo.blue,
            0.0,
        ];
        let model = transform.to_matrix();
        let budget = MAX_SAMPLES_PER_FRAME - count;
        let mesh_count = mesh_samples.local_positions.len().min(budget);
        for &local in &mesh_samples.local_positions[..mesh_count] {
            let world = model.transform_point3(local);
            let sample = GpuSample {
                world_pos: [world.x, world.y, world.z, 0.0],
                albedo,
            };
            if let Some(previous) = samples.get_mut(count) {
                // Byte equality preserves signed zero and stable NaN payloads.
                if bytemuck::bytes_of(previous) != bytemuck::bytes_of(&sample) {
                    *previous = sample;
                    changed = true;
                }
            } else {
                samples.push(sample);
                changed = true;
            }
            count += 1;
        }
    }
    changed |= samples.len() != count;
    samples.truncate(count);
    changed
}

#[derive(Resource, Default)]
pub struct ExtractedGeometrySamples(pub Vec<GpuSample>);

pub fn prepare_geometry_sample_buffer(
    extracted: Option<Res<ExtractedGeometrySamples>>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut buffer: ResMut<GeometrySampleBuffer>,
) {
    let Some(extracted) = extracted else {
        buffer.count = 0;
        return;
    };
    if extracted.0.is_empty() {
        buffer.count = 0;
        return;
    }

    let bytes = bytemuck::cast_slice(&extracted.0);
    let needed = bytes.len() as u64;

    // Reallocate if we don't have a buffer yet or the existing one is
    // too small. Round up to the next power-of-two MB so we don't
    // bounce on tiny size changes.
    let needs_alloc = match buffer.buffer.as_ref() {
        None => true,
        Some(_) => buffer.capacity_bytes < needed,
    };
    if needs_alloc {
        let cap = needed.next_power_of_two().max(1 << 20); // ≥ 1 MB
        buffer.buffer = Some(render_device.create_buffer(&BufferDescriptor {
            label: Some("voxel_geometry_samples"),
            size: cap,
            usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        buffer.capacity_bytes = cap;
    }
    if needs_alloc || extracted.is_changed() {
        if let Some(buf) = buffer.buffer.as_ref() {
            render_queue.write_buffer(buf, 0, bytes);
        }
    }
    buffer.count = extracted.0.len() as u32;
}

/// Geometry inject pass. Bevy 0.19: was `GeometryInjectNode: ViewNode`; now a
/// render system in `LumenSystems::GeometryInject` (between Inject and Resolve).
pub fn geometry_inject_pass(
    world: &World,
    view: ViewQuery<&'static VoxelCacheView>,
    mut render_context: RenderContext,
) {
    let view = view.into_inner();
    {
        if !view.inject_active {
            return;
        }
        let buffer = world.resource::<GeometrySampleBuffer>();
        if buffer.count == 0 {
            return;
        }
        let Some(sample_buf) = buffer.buffer.as_ref() else {
            return;
        };
        let _span = info_span!("geometry.inject").entered();

        let pipeline = world.resource::<GeometryInjectPipeline>();
        let pipeline_cache = world.resource::<PipelineCache>();
        let Some(compute) = pipeline_cache.get_compute_pipeline(pipeline.pipeline_id) else {
            return;
        };
        let Some(resources) = world.get_resource::<VoxelCacheResources>() else {
            return;
        };

        let bg = render_context.render_device().create_bind_group(
            "voxel_geo_inject_bg",
            &pipeline_cache.get_bind_group_layout(&pipeline.layout),
            &BindGroupEntries::sequential((
                sample_buf.as_entire_binding(),
                resources.accum_buffer.as_entire_binding(),
                resources.uniform_buffer.as_entire_binding(),
            )),
        );

        let mut pass = render_context
            .command_encoder()
            .begin_compute_pass(&ComputePassDescriptor {
                label: Some("voxel_geo_inject"),
                timestamp_writes: None,
            });
        pass.set_pipeline(compute);
        pass.set_bind_group(0, &bg, &[]);
        // 64 threads per workgroup.
        let groups = buffer.count.div_ceil(64);
        pass.dispatch_workgroups(groups, 1, 1);
    }
}

#[derive(Default)]
pub struct GeometryVoxelizePlugin;

impl Plugin for GeometryVoxelizePlugin {
    fn build(&self, app: &mut App) {
        bevy::asset::embedded_asset!(app, "voxel_geo_inject.wgsl");
        app.init_resource::<LumenBakeStats>();
        app.add_systems(Update, (invalidate_baked_assets, bake_mesh_samples).chain());

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<GeometrySampleBuffer>();
            render_app.init_resource::<ExtractedGeometrySamples>();
            render_app.add_systems(bevy::render::ExtractSchedule, extract_geometry_samples);
            render_app.add_systems(
                Render,
                prepare_geometry_sample_buffer.in_set(RenderSystems::PrepareResources),
            );
            // Bevy 0.19: slots into `LumenSystems::GeometryInject`, which the
            // shared set ordering in `lib.rs` places between Inject and Resolve.
            render_app.add_systems(
                Core3d,
                geometry_inject_pass.in_set(crate::LumenSystems::GeometryInject),
            );
        }
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.init_resource::<GeometryInjectPipeline>();
        }
    }
}

#[cfg(test)]
mod retained_sample_tests {
    use super::*;

    #[test]
    fn same_id_asset_edits_rebake_and_empty_meshes_settle() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<renzora_shader::material::GraphMaterial>>()
            .add_message::<AssetEvent<Mesh>>()
            .add_message::<AssetEvent<StandardMaterial>>()
            .add_message::<AssetEvent<renzora_shader::material::GraphMaterial>>()
            .init_resource::<LumenBakeStats>()
            .add_systems(Update, (invalidate_baked_assets, bake_mesh_samples).chain());
        let mesh = app.world_mut().resource_mut::<Assets<Mesh>>()
            .add(Mesh::new(PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::default()));
        let material = app.world_mut().resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let entity = app.world_mut().spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material.clone()))).id();
        app.update();
        for _ in 0..1000 { app.update(); }
        assert_eq!(app.world().resource::<LumenBakeStats>().total_bakes, 1);
        assert!(app.world().get::<MeshVoxelSamples>(entity).unwrap().local_positions.is_empty());
        *app.world_mut().resource_mut::<Assets<Mesh>>().get_mut(&mesh).unwrap() = Mesh::from(Cuboid::new(1.0, 1.0, 1.0));
        app.world_mut().write_message(AssetEvent::<Mesh>::Modified { id: mesh.id() });
        app.update();
        assert!(!app.world().get::<MeshVoxelSamples>(entity).unwrap().local_positions.is_empty());
        app.world_mut().entity_mut(entity).insert(bevy::ecs::entity_disabling::Disabled);
        app.world_mut().resource_mut::<Assets<StandardMaterial>>().get_mut(&material).unwrap().base_color = Color::srgb(1.0, 0.0, 0.0);
        app.world_mut().write_message(AssetEvent::<StandardMaterial>::Modified { id: material.id() });
        app.update();
        assert!(app.world().get::<PendingVoxelBake>(entity).is_some());
        app.world_mut().entity_mut(entity).remove::<bevy::ecs::entity_disabling::Disabled>();
        app.update();
        assert_eq!(app.world().get::<MeshVoxelSamples>(entity).unwrap().albedo, LinearRgba::RED);
        let graph = app.world_mut()
            .resource_mut::<Assets<renzora_shader::material::GraphMaterial>>()
            .add(renzora_shader::material::GraphMaterial::default());
        app.world_mut().entity_mut(entity)
            .remove::<MeshMaterial3d<StandardMaterial>>()
            .insert(MeshMaterial3d(graph.clone()));
        app.update();
        app.world_mut().resource_mut::<Assets<renzora_shader::material::GraphMaterial>>()
            .get_mut(&graph).unwrap().base.base_color = Color::srgb(0.0, 1.0, 0.0);
        app.world_mut().write_message(AssetEvent::<renzora_shader::material::GraphMaterial>::Modified { id: graph.id() });
        app.update();
        assert_eq!(app.world().get::<MeshVoxelSamples>(entity).unwrap().albedo, LinearRgba::GREEN);
    }

    #[test]
    fn changed_meshes_beyond_bake_budget_are_not_forgotten() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<renzora_shader::material::GraphMaterial>>()
            .init_resource::<LumenBakeStats>()
            .add_systems(Update, bake_mesh_samples);
        let mesh = app.world_mut().resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(1.0, 1.0, 1.0));
        let material = app.world_mut().resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let entities: Vec<_> = (0..MAX_BAKES_PER_FRAME + 1).map(|_| {
            app.world_mut().spawn((
                Mesh3d(mesh.clone()), MeshMaterial3d(material.clone()),
                MeshVoxelSamples { local_positions: vec![Vec3::splat(99.0)], albedo: LinearRgba::BLACK },
            )).id()
        }).collect();
        for _ in 0..3 {
            app.update();
            assert!(app.world().resource::<LumenBakeStats>().bakes_last_frame <= MAX_BAKES_PER_FRAME);
        }
        for entity in entities {
            assert_ne!(app.world().get::<MeshVoxelSamples>(entity).unwrap().local_positions, vec![Vec3::splat(99.0)]);
        }
    }

    #[test]
    fn rolling_bake_statistics_match_reference_after_wrap_and_clone() {
        let mut stats = LumenBakeStats::default();
        let mut reference = Vec::new();
        for index in 0..1000u64 {
            let duration = std::time::Duration::from_nanos((index * 37) % 997);
            stats.record(duration, 1, 3);
            reference.push(duration);
            if reference.len() > 60 {
                reference.remove(0);
            }
            let sum: std::time::Duration = reference.iter().sum();
            assert_eq!(stats.avg_bake_dur, sum / reference.len() as u32);
            assert_eq!(stats.recent_total, sum);
            assert_eq!(stats.total_bakes, index + 1);
            assert_eq!(stats.total_samples_baked, (index + 1) * 3);
            assert_eq!(stats.last_bake_dur, duration);
            assert!(stats.recent_durs.len() <= 60);
            if index == 500 {
                stats = stats.clone();
            }
        }
        let capacity = stats.recent_durs.capacity();
        for _ in 0..1000 {
            stats.record(std::time::Duration::ZERO, 0, 0);
            assert_eq!(stats.recent_durs.capacity(), capacity);
        }
        assert_eq!(stats.avg_bake_dur, std::time::Duration::ZERO);
        assert!(stats.max_bake_dur > std::time::Duration::ZERO);
    }

    #[derive(Resource, Default)]
    struct UploadNotifications(usize);

    fn observe_payload(
        samples: Res<ExtractedGeometrySamples>,
        mut count: ResMut<UploadNotifications>,
    ) {
        if samples.is_changed() {
            count.0 += 1;
        }
    }

    #[test]
    fn extraction_notifies_only_for_payload_changes() {
        let mut main = bevy::render::MainWorld::default();
        let camera = main
            .spawn((
                Camera3d::default(),
                GlobalTransform::IDENTITY,
                VoxelCacheView {
                    inject_active: true,
                    debug_active: false,
                },
            ))
            .id();
        let mesh = main
            .spawn((
                MeshVoxelSamples {
                    local_positions: vec![Vec3::X],
                    albedo: LinearRgba::WHITE,
                },
                GlobalTransform::IDENTITY,
            ))
            .id();
        let mut app = App::new();
        app.insert_resource(main)
            .init_resource::<ExtractedGeometrySamples>()
            .init_resource::<UploadNotifications>()
            .add_systems(Update, (extract_geometry_samples, observe_payload).chain());
        app.update();
        assert_eq!(app.world().resource::<UploadNotifications>().0, 1);
        for _ in 0..1_000 {
            app.update();
        }
        assert_eq!(app.world().resource::<UploadNotifications>().0, 1);
        app.world_mut()
            .resource_mut::<bevy::render::MainWorld>()
            .entity_mut(mesh)
            .insert(GlobalTransform::from_translation(Vec3::Y));
        app.update();
        assert_eq!(app.world().resource::<UploadNotifications>().0, 2);
        app.world_mut()
            .resource_mut::<bevy::render::MainWorld>()
            .get_mut::<VoxelCacheView>(camera)
            .unwrap()
            .inject_active = false;
        app.update();
        assert!(app
            .world()
            .resource::<ExtractedGeometrySamples>()
            .0
            .is_empty());
        assert_eq!(app.world().resource::<UploadNotifications>().0, 3);
        app.update();
        assert_eq!(app.world().resource::<UploadNotifications>().0, 3);
        app.world_mut()
            .resource_mut::<bevy::render::MainWorld>()
            .get_mut::<VoxelCacheView>(camera)
            .unwrap()
            .inject_active = true;
        app.update();
        assert_eq!(app.world().resource::<UploadNotifications>().0, 4);
    }

    #[test]
    fn stable_samples_reuse_storage_and_edits_refresh_payload() {
        let mut mesh = MeshVoxelSamples {
            local_positions: vec![Vec3::ZERO, Vec3::X],
            albedo: LinearRgba::WHITE,
        };
        let mut transform = GlobalTransform::IDENTITY;
        let mut samples = Vec::new();
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        let storage = samples.as_ptr();
        for _ in 0..1_000 {
            assert!(!refresh_geometry_samples(
                &mut samples,
                [(&mesh, &transform)],
                Some(Vec3::ZERO)
            ));
            assert_eq!(samples.as_ptr(), storage);
        }
        transform = GlobalTransform::from_translation(Vec3::Y);
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert_eq!(samples[0].world_pos, [0.0, 1.0, 0.0, 0.0]);
        mesh.albedo.red = 0.25;
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert_eq!(samples[0].albedo[0], 0.25);
        mesh.local_positions.pop();
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert_eq!(samples.len(), 1);
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::splat(100.0))
        ));
        assert!(samples.is_empty());
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            None
        ));
        assert!(!refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            None
        ));
        assert_eq!(samples.as_ptr(), storage);
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert!(refresh_geometry_samples(&mut samples, [], Some(Vec3::ZERO)));
    }

    #[test]
    fn sample_budget_and_order_are_preserved() {
        let mesh = MeshVoxelSamples {
            local_positions: vec![Vec3::X; MAX_SAMPLES_PER_FRAME + 1],
            albedo: LinearRgba::WHITE,
        };
        let transform = GlobalTransform::IDENTITY;
        let mut samples = Vec::new();
        assert!(refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert_eq!(samples.len(), MAX_SAMPLES_PER_FRAME);
        assert!(!refresh_geometry_samples(
            &mut samples,
            [(&mesh, &transform)],
            Some(Vec3::ZERO)
        ));
        assert!(samples
            .iter()
            .all(|sample| sample.world_pos == [1.0, 0.0, 0.0, 0.0]));
    }
}
