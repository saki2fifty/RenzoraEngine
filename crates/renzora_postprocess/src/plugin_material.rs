//! Custom shaded materials for standalone plugins.
//!
//! A plugin cannot define a Bevy `Material`: that is a Rust type with a derived
//! `AsBindGroup`, and the plugin has no Bevy. So there is exactly one material
//! type here — [`PluginMaterial`] — and every plugin material is an *instance*
//! of it carrying its own shader handle and its own block of uniform bytes.
//!
//! ## Why one type with a fixed uniform size
//!
//! Bevy decides a material's bind-group layout **once per type**, not per
//! instance: `AsBindGroup::bind_group_layout_entries` is a static function. A
//! plugin's uniform is whatever struct it declared, so the layout cannot be
//! derived from it — it has to be reserved. Hence
//! [`MATERIAL_UNIFORM_CAP`](renzora_plugin::sys::MATERIAL_UNIFORM_CAP): one
//! uniform buffer of that size, and a plugin uses as much of it as its settings
//! component needs. Registration refuses anything larger rather than letting the
//! shader read past the buffer, which is undefined on the GPU rather than merely
//! wrong.
//!
//! The **shader**, unlike the layout, can vary per instance —
//! [`Material::specialize`] receives the pipeline descriptor and the material's
//! key, so each instance points the vertex and fragment stages at its own
//! module. That is the whole trick that makes one type serve every plugin.
//!
//! ## Where the uniform bytes come from
//!
//! The same place a post-process effect's do: a component the plugin declared.
//! `collect_material_settings` copies its bytes out of the world each frame and
//! into every material instance that names it, so the parameters are described
//! once — inspector-editable, scene-serialised, readable by the plugin's own
//! systems — instead of duplicated into a GPU-only struct.

use bevy::ecs::component::ComponentId;
use bevy::ecs::entity_disabling::DefaultQueryFilters;
use bevy::ecs::query::{FilteredAccess, QueryBuilder};
use bevy::platform::collections::{HashMap, HashSet};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroupError, BindGroupLayout, BindGroupLayoutEntry, BindingResources,
    OwnedBindingResource, RenderPipelineDescriptor, ShaderStages, SpecializedMeshPipelineError,
    UnpreparedBindGroup,
};
use bevy::render::renderer::RenderDevice;
use bevy::shader::{Shader, ShaderRef};

use renzora_plugin::host::{CustomMaterialApplier, PendingMaterials};
use renzora_plugin::sys::MATERIAL_UNIFORM_CAP;

/// One plugin-defined material.
///
/// `uniform` is a fixed-size block regardless of how much the plugin declared —
/// see the module doc on why the layout cannot follow the settings struct.
#[derive(Asset, TypePath, Clone, Debug)]
pub struct PluginMaterial {
    /// This instance's WGSL module. Applied in `specialize`, which is what lets
    /// one Rust type render every plugin's shader.
    pub shader: Handle<Shader>,
    /// Size of the settings component, so the per-frame copy reads exactly that
    /// many bytes. Reading the full uniform capacity instead would run off the
    /// end of any component smaller than it.
    pub settings_size: usize,
    /// Raw uniform bytes, refreshed each frame from the settings component.
    pub uniform: [u8; MATERIAL_UNIFORM_CAP as usize],
    pub alpha_mode: AlphaMode,
    /// Component the bytes come from — read by `collect_material_settings`,
    /// never by the GPU.
    pub settings: ComponentId,
    /// Bound from `@group(3) @binding(1)` upward, each followed by its sampler.
    pub textures: Vec<Handle<Image>>,
}

/// Per-instance pipeline key: the shader to specialize to.
///
/// `AsBindGroup::Data` is what Bevy carries from the main world into
/// specialization, so the handle rides across here rather than being looked up
/// again in the render world.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PluginMaterialKey {
    shader: Handle<Shader>,
}

impl bevy::render::render_resource::AsBindGroup for PluginMaterial {
    type Data = PluginMaterialKey;
    /// The uploaded images, plus Bevy's fallback for the slots a material did
    /// not fill — see [`Self::unprepared_bind_group`] for why every slot must be
    /// filled with something.
    type Param = (
        bevy::ecs::system::lifetimeless::SRes<
            bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>,
        >,
        bevy::ecs::system::lifetimeless::SRes<bevy::render::texture::FallbackImage>,
    );

    fn label() -> &'static str {
        "plugin_material"
    }

    /// Carries the shader handle into specialization — that is what lets one
    /// Rust type render every plugin's module.
    ///
    /// Texture count is deliberately *not* in the key. The layout declares all
    /// [`MAX_MATERIAL_TEXTURES`](renzora_plugin::sys::MAX_MATERIAL_TEXTURES)
    /// slots whatever a material actually binds, so two materials with different
    /// texture counts have identical layouts and can share a pipeline.
    fn bind_group_data(&self) -> Self::Data {
        PluginMaterialKey {
            shader: self.shader.clone(),
        }
    }

    fn unprepared_bind_group(
        &self,
        _layout: &BindGroupLayout,
        render_device: &RenderDevice,
        param: &mut bevy::ecs::system::SystemParamItem<'_, '_, Self::Param>,
        _force_no_bindless: bool,
    ) -> Result<UnpreparedBindGroup, AsBindGroupError> {
        use bevy::render::render_resource::{BufferInitDescriptor, BufferUsages};
        let buffer = render_device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("plugin_material_uniform"),
            contents: &self.uniform,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });
        let mut bindings = vec![(0, OwnedBindingResource::Buffer(buffer))];

        // **Every slot the layout declares must be bound, including the ones
        // this material does not use.** wgpu requires the bind group and its
        // layout to agree exactly; a material with one texture against a layout
        // of four is a validation error and a hard render-thread abort, not a
        // tail that is quietly ignored. So the unused slots get Bevy's fallback
        // image — the same one its own optional-texture materials bind.
        //
        // Texture then sampler, from binding 1 upward. `RetryNextUpdate` rather
        // than an error when an image has not reached the GPU yet: that is the
        // normal state for the first frames after creation, and failing outright
        // would drop the material permanently.
        let (images, fallback) = param;
        for i in 0..renzora_plugin::sys::MAX_MATERIAL_TEXTURES {
            let gpu = match self.textures.get(i) {
                Some(handle) => match images.get(handle) {
                    Some(gpu) => gpu,
                    None => return Err(AsBindGroupError::RetryNextUpdate),
                },
                // Always `d2`: `add_image` only creates `TextureDimension::D2`.
                None => &fallback.d2,
            };
            let base = 1 + i as u32 * 2;
            bindings.push((
                base,
                OwnedBindingResource::TextureView(
                    bevy::render::render_resource::TextureViewDimension::D2,
                    gpu.texture_view.clone(),
                ),
            ));
            bindings.push((
                base + 1,
                OwnedBindingResource::Sampler(
                    bevy::render::render_resource::SamplerBindingType::Filtering,
                    gpu.sampler.clone(),
                ),
            ));
        }
        Ok(UnpreparedBindGroup {
            bindings: BindingResources(bindings),
        })
    }

    fn bind_group_layout_entries(
        _render_device: &RenderDevice,
        _force_no_bindless: bool,
    ) -> Vec<BindGroupLayoutEntry> {
        use bevy::render::render_resource::binding_types::{
            sampler, texture_2d, uniform_buffer_sized,
        };
        use bevy::render::render_resource::{
            BindGroupLayoutEntries, SamplerBindingType, TextureSampleType,
        };
        // The layout is fixed for the shared material type, so it always
        // declares the maximum — the same trade the uniform cap makes. A
        // material binding fewer textures does *not* leave the tail unbound;
        // `unprepared_bind_group` fills the rest with the fallback image,
        // because wgpu rejects a bind group whose count differs from its layout.
        let mut entries = BindGroupLayoutEntries::single(
            ShaderStages::VERTEX_FRAGMENT,
            uniform_buffer_sized(false, core::num::NonZeroU64::new(MATERIAL_UNIFORM_CAP)),
        )
        .to_vec();
        for i in 0..renzora_plugin::sys::MAX_MATERIAL_TEXTURES {
            let base = 1 + i as u32 * 2;
            let mut t = texture_2d(TextureSampleType::Float { filterable: true })
                .build(base, ShaderStages::VERTEX_FRAGMENT);
            t.binding = base;
            entries.push(t);
            let mut sm = sampler(SamplerBindingType::Filtering)
                .build(base + 1, ShaderStages::VERTEX_FRAGMENT);
            sm.binding = base + 1;
            entries.push(sm);
        }
        entries
    }
}

impl Material for PluginMaterial {
    /// Bevy's own mesh vertex shader, deliberately.
    ///
    /// A plugin supplies a fragment shader only. Writing a vertex stage would
    /// mean hand-rolling the skinning, morph targets and instance-indexed model
    /// transform out of `@group(0)`/`@group(1)` — version-fragile work that has
    /// nothing to do with the material's own look, and that every plugin would
    /// have to redo. Almost every custom material is a fragment.
    fn vertex_shader() -> ShaderRef {
        ShaderRef::Default
    }

    /// Replaced per instance in `specialize`, which is what lets one Rust type
    /// serve every plugin's shader.
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Default
    }

    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }

    fn specialize(
        _pipeline: &bevy::pbr::MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        key: bevy::pbr::MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // **Only the main pass.** Bevy calls this same function for the prepass
        // and the shadow pass too, and their vertex stages emit a different
        // `VertexOutput` — the prepass puts UVs at location 0 where the forward
        // path puts world position. Overriding the fragment there hands the
        // shader a struct that does not match what feeds it, which wgpu rejects
        // while creating the pipeline: an unrecoverable abort, not a bad frame.
        // Bevy's own source flags this hazard in `PrepassPipelineSpecializer`.
        //
        // The pipeline key cannot tell us which pass this is, so the label has
        // to: `MeshPipelineKey::DEPTH_PREPASS` is set on the *main* pass key as
        // well, to signal that a prepass exists, so testing the bits identifies
        // both. Matched positively — an unrecognised pass keeps Bevy's shader,
        // which is the safe direction for whatever it adds next.
        //
        // Leaving the other passes alone is also what we want on the merits.
        // The prepass still runs Bevy's depth/normal/motion-vector fragment for
        // this material, so SSAO, TAA, motion blur and DOF see plugin geometry
        // like any other. Opting out of the prepass entirely would have fixed
        // the crash and quietly broken all four.
        let main_pass = descriptor
            .label
            .as_deref()
            .is_some_and(|label| label.ends_with("_mesh_pipeline"));
        if !main_pass {
            return Ok(());
        }

        // Fragment only — the vertex stage stays Bevy's, so a plugin's shader
        // is just its `@fragment fn fragment(in: VertexOutput)`.
        //
        // Unlike a post-process shader, this one is compiled through Bevy's
        // normal pipeline, so naga_oil is available and
        // `#import bevy_pbr::forward_io::VertexOutput` works. That import is in
        // fact required: `VertexOutput` is what the vertex stage above hands
        // over, and its layout is Bevy's to define.
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment.shader = key.bind_group_data.shader.clone();
        }
        Ok(())
    }
}

/// Build the real material assets from what plugins registered.
///
/// Runs in `finish`, like the post-process bridge and for the same reason: by
/// then every plugin's `build` has run and the loader has mapped the cdylibs.
pub fn build_plugin_materials(app: &mut App) {
    let pending = app
        .world_mut()
        .remove_resource::<PendingMaterials>()
        .map(|p| p.0)
        .unwrap_or_default();
    if pending.is_empty() {
        return;
    }

    let mut created: Vec<(usize, Handle<PluginMaterial>)> = Vec::new();
    {
        let world = app.world_mut();
        let Some(mut materials) = world.get_resource_mut::<Assets<PluginMaterial>>() else {
            warn!("[plugin] materials ignored — this build has no renderer");
            return;
        };
        for m in &pending {
            let handle = materials.add(PluginMaterial {
                shader: m.shader.clone(),
                settings_size: (m.settings_size as usize).min(MATERIAL_UNIFORM_CAP as usize),
                uniform: [0; MATERIAL_UNIFORM_CAP as usize],
                textures: m.textures.clone(),
                alpha_mode: match m.alpha_mode {
                    renzora_plugin::sys::AlphaMode::Mask => AlphaMode::Mask(0.5),
                    renzora_plugin::sys::AlphaMode::Blend => AlphaMode::Blend,
                    _ => AlphaMode::Opaque,
                },
                settings: m.settings,
            });
            created.push((m.slot, handle));
        }
    }

    // Keep the built assets indexed by the slot the plugin already holds. They
    // live here rather than in `PluginAssets` because that store is typed to
    // `StandardMaterial` and this crate is the only one that can name
    // `PluginMaterial`.
    let mut table = BuiltPluginMaterials::default();
    for (slot, handle) in created {
        if table.0.len() <= slot {
            table.0.resize(slot + 1, None);
        }
        table.0[slot] = Some(handle);
    }
    app.insert_resource(table);
    app.insert_resource(CustomMaterialApplier(apply_custom_material));
}

/// Plugin materials, indexed by the asset-handle slot the plugin was given.
#[derive(Resource, Default)]
pub struct BuiltPluginMaterials(pub Vec<Option<Handle<PluginMaterial>>>);

/// Attach a plugin material to a spawned mesh.
///
/// Registered as the [`CustomMaterialApplier`] so `spawn_mesh` can finish a
/// spawn without `renzora_plugin` ever naming [`PluginMaterial`].
fn apply_custom_material(world: &mut World, entity: Entity, slot: usize) {
    let handle = world
        .get_resource::<BuiltPluginMaterials>()
        .and_then(|t| t.0.get(slot).cloned().flatten());
    let Some(handle) = handle else {
        error!("[plugin] spawn_mesh named custom material slot {slot}, which was never built");
        return;
    };
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(MeshMaterial3d(handle));
    }
}

/// Copy each material's settings component out of the world into its uniform.
///
/// The same shape `collect_effect_settings` uses for post-process: find the
/// first entity carrying the component and take its bytes. A material is a
/// global look, not a per-entity one, so one source is the right model — the
/// same assumption the effect path already makes.
pub fn collect_material_settings(world: &mut World) {
    if !world.contains_resource::<Assets<PluginMaterial>>() {
        return;
    }
    world.init_resource::<MaterialSettingQueries>();
    world.resource_scope(|world, mut cache: Mut<MaterialSettingQueries>| {
        world.resource_scope(|world, mut materials: Mut<Assets<PluginMaterial>>| {
            let cache = &mut *cache;
            cache.wanted.clear();
            cache.wanted.extend(materials.ids());
            cache.used.clear();
            for id in &cache.wanted {
                let Some(mut material) = materials.get_mut(*id) else {
                    continue;
                };
                let component = material.settings;
                let size = material.settings_size;
                // Material fields are public; do not trust a manually supplied
                // size to read beyond either the component or the uniform.
                if size == 0
                    || size > material.uniform.len()
                    || world
                        .components()
                        .get_info(component)
                        .is_none_or(|info| size > info.layout().size())
                {
                    continue;
                }
                cache.used.insert(component);
                let query = cache.queries.entry(component).or_insert_with(|| {
                    // Match the dynamic equivalent of Allow<T> for all default
                    // exclusions: the old world scan includes disabled entities.
                    let mut allowed = FilteredAccess::default();
                    if let Some(filters) = world.get_resource::<DefaultQueryFilters>() {
                        for id in filters.disabling_ids() {
                            allowed.access_mut().add_archetypal(id);
                        }
                    }
                    let mut builder = QueryBuilder::<Entity>::new(world);
                    builder.with_id(component);
                    builder.extend_access(allowed);
                    builder.build()
                });
                query.update_archetypes(world);
                // World::iter_entities visits archetypes, then their rows.
                // Query iteration may instead merge tables (notably for sparse
                // components), changing which global settings entity wins.
                let entity = query
                    .matched_archetypes()
                    .find_map(|id| world.archetypes()[id].entities().first().map(|e| e.id()));
                let Some(pointer) = entity
                    .and_then(|entity| world.get_entity(entity).ok()?.get_by_id(component).ok())
                else {
                    continue;
                };
                // SAFETY: the component exists, its registered layout covers
                // size, and world is immutably borrowed while the bytes are
                // read. The separately scoped asset cannot alias this storage.
                let bytes = unsafe { std::slice::from_raw_parts(pointer.as_ptr(), size) };
                // AssetMut only emits Modified on a mutable dereference.
                if material.uniform[..size] != *bytes {
                    material.uniform[..size].copy_from_slice(bytes);
                }
            }
            cache
                .queries
                .retain(|component, _| cache.used.contains(component));
        });
    });
}

#[derive(Resource, Default)]
struct MaterialSettingQueries {
    queries: HashMap<ComponentId, QueryState<Entity>>,
    wanted: Vec<AssetId<PluginMaterial>>,
    used: HashSet<ComponentId>,
}

/// Installs the material type and its per-frame uniform refresh.
pub struct PluginMaterialPlugin;

impl Plugin for PluginMaterialPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<PluginMaterial>::default())
            .add_systems(PostUpdate, collect_material_settings);
    }

    fn finish(&self, app: &mut App) {
        build_plugin_materials(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component)]
    #[repr(transparent)]
    struct Settings(u32);

    #[derive(Component)]
    #[component(storage = "SparseSet")]
    struct SparseMarker;

    fn material(component: ComponentId) -> PluginMaterial {
        PluginMaterial {
            shader: Handle::default(),
            settings_size: 4,
            uniform: [0; MATERIAL_UNIFORM_CAP as usize],
            alpha_mode: AlphaMode::Opaque,
            settings: component,
            textures: Vec::new(),
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::asset::AssetPlugin::default()))
            .init_asset::<PluginMaterial>()
            .add_systems(Update, collect_material_settings);
        app
    }

    #[test]
    fn unused_material_skips_unrelated_entities_and_cache_tracks_lifecycle() {
        let mut app = app();
        let component = app.world_mut().register_component::<Settings>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<PluginMaterial>>()
            .add(material(component));
        for _ in 0..10_000 {
            app.world_mut().spawn_empty();
        }
        let mut old_visits = 0;
        let found = app.world().iter_entities().find_map(|entity| {
            old_visits += 1;
            entity.get::<Settings>()
        });
        assert!(found.is_none());
        assert!(old_visits >= 10_000);
        app.update();
        let cache = app.world().resource::<MaterialSettingQueries>();
        assert_eq!(cache.queries[&component].matched_archetypes().count(), 0);
        let capacity = cache.wanted.capacity();
        for _ in 0..100 {
            app.update();
        }
        assert_eq!(
            app.world()
                .resource::<MaterialSettingQueries>()
                .wanted
                .capacity(),
            capacity
        );
        // Create a matching archetype only after the empty query is cached.
        let entity = app.world_mut().spawn(Settings(23)).id();
        app.update();
        assert_eq!(
            &app.world()
                .resource::<Assets<PluginMaterial>>()
                .get(&handle)
                .expect("material exists")
                .uniform[..4],
            &23u32.to_ne_bytes()
        );
        app.world_mut().despawn(entity);
        app.update();
        // No source keeps the last uniform, as the previous collector did.
        assert_eq!(
            &app.world()
                .resource::<Assets<PluginMaterial>>()
                .get(&handle)
                .expect("material exists")
                .uniform[..4],
            &23u32.to_ne_bytes()
        );
        app.world_mut()
            .resource_mut::<Assets<PluginMaterial>>()
            .remove(handle.id());
        app.update();
        assert!(app
            .world()
            .resource::<MaterialSettingQueries>()
            .queries
            .is_empty());
        eprintln!("unused material: old scan visited {old_visits} entities; cached query matched zero archetypes across 100 stable updates");
    }

    #[test]
    fn source_order_matches_world_scan_and_unchanged_assets_stay_quiet() {
        let mut app = app();
        let component = app.world_mut().register_component::<Settings>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<PluginMaterial>>()
            .add(material(component));
        let first = app.world_mut().spawn((Settings(11), SparseMarker)).id();
        let second = app.world_mut().spawn(Settings(22)).id();
        for step in 0..4 {
            match step {
                1 => {
                    app.world_mut().entity_mut(first).remove::<SparseMarker>();
                }
                2 => {
                    app.world_mut().entity_mut(second).insert(SparseMarker);
                }
                3 => {
                    app.world_mut().despawn(first);
                }
                _ => {}
            }
            let expected = app
                .world()
                .iter_entities()
                .find_map(|e| e.get::<Settings>().map(|s| s.0))
                .expect("settings source");
            app.update();
            assert_eq!(
                &app.world()
                    .resource::<Assets<PluginMaterial>>()
                    .get(&handle)
                    .expect("material exists")
                    .uniform[..4],
                &expected.to_ne_bytes()
            );
        }
        app.world_mut()
            .resource_mut::<Messages<AssetEvent<PluginMaterial>>>()
            .clear();
        app.update();
        let changes = app
            .world_mut()
            .resource_mut::<Messages<AssetEvent<PluginMaterial>>>()
            .drain()
            .filter(|event| matches!(event, AssetEvent::Modified { .. }))
            .count();
        assert_eq!(changes, 0);
        app.world_mut()
            .entity_mut(second)
            .get_mut::<Settings>()
            .expect("surviving source")
            .0 = 99;
        app.update();
        assert_eq!(
            &app.world()
                .resource::<Assets<PluginMaterial>>()
                .get(&handle)
                .expect("material exists")
                .uniform[..4],
            &99u32.to_ne_bytes()
        );
        let changes = app
            .world_mut()
            .resource_mut::<Messages<AssetEvent<PluginMaterial>>>()
            .drain()
            .filter(|event| matches!(event, AssetEvent::Modified { .. }))
            .count();
        assert_eq!(changes, 1);
    }

    #[test]
    fn disabled_settings_keep_the_world_scan_behavior() {
        use bevy::ecs::entity_disabling::Disabled;
        let mut app = app();
        let component = app.world_mut().register_component::<Settings>();
        let custom = app.world_mut().register_component::<SparseMarker>();
        app.world_mut()
            .resource_mut::<DefaultQueryFilters>()
            .register_disabling_component(custom);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<PluginMaterial>>()
            .add(material(component));
        app.world_mut()
            .spawn((Settings(77), Disabled, SparseMarker));
        app.update();
        assert_eq!(
            &app.world()
                .resource::<Assets<PluginMaterial>>()
                .get(&handle)
                .expect("material exists")
                .uniform[..4],
            &77u32.to_ne_bytes()
        );
    }

    #[test]
    fn shared_sources_update_all_materials_and_invalid_sizes_are_skipped() {
        let mut app = app();
        let component = app.world_mut().register_component::<Settings>();
        app.world_mut().spawn(Settings(42));
        let mut assets = app.world_mut().resource_mut::<Assets<PluginMaterial>>();
        let first = assets.add(material(component));
        let second = assets.add(material(component));
        let mut invalid = material(component);
        invalid.settings_size = 5;
        let invalid = assets.add(invalid);
        let mut empty = material(component);
        empty.settings_size = 0;
        let empty = assets.add(empty);
        app.update();
        let assets = app.world().resource::<Assets<PluginMaterial>>();
        for handle in [&first, &second] {
            assert_eq!(
                &assets.get(handle).expect("material exists").uniform[..4],
                &42u32.to_ne_bytes()
            );
        }
        for handle in [&invalid, &empty] {
            assert!(assets
                .get(handle)
                .expect("material exists")
                .uniform
                .iter()
                .all(|byte| *byte == 0));
        }
        assert_eq!(
            app.world()
                .resource::<MaterialSettingQueries>()
                .queries
                .len(),
            1
        );
    }
}
