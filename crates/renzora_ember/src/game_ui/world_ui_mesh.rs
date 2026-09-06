//! Unity-style world-space UI: emit the laid-out UI tree as batched 3D geometry.
//!
//! A texture-mode [`WorldUiPanel`] renders its UI to an offscreen image on a quad
//! (RTT — flat, fixed-resolution). Mesh mode instead walks the *already
//! laid-out* UI tree — bevy_ui has computed each node's [`ComputedNode`] rect and
//! [`UiGlobalTransform`] — and emits geometry directly into the 3D scene on the
//! panel's plane. That's how Unity's world-space Canvas turns UI into scene
//! geometry.
//!
//! - **Background rects** → a vertex-coloured quad mesh on the panel entity.
//! - **Text** → each text node is rendered by [`build_text_mesh`] — the *same*
//!   crisp SDF glyph-mesh generator the standalone 3D-Text entity uses — as a
//!   child mesh placed at the node's position. Reusing that proven builder (rather
//!   than re-deriving glyph geometry from bevy's atlas here) keeps world-UI text
//!   pixel-identical to 3D text and sidesteps a class of glyph-placement bugs.
//!
//! ## Only rebuild on change
//!
//! Settled canvases skip the tree walk. Dirty canvases still hash their geometry:
//! recreating meshes/materials/textures is expensive — and doing it
//! every frame both tanks the FPS and (worse) despawns text meshes while the
//! render world may still reference them, drawing freed vertex buffers as garbage
//! triangles. So each panel stores a hash of the geometry it last built; a frame
//! whose walk produces the same hash touches nothing. Geometry is rebuilt only
//! when the layout actually changes (text edited, theme swapped, panel resized).
//!
//! Milestone: rects + SDF text. Images, borders and rounded corners follow.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use bevy::asset::RenderAssetUsages;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};
use bevy::camera::visibility::NoFrustumCulling;
use bevy::text::{
    Font, FontAtlasSet, FontCx, FontSize, LayoutCx, RemSize, ScaleCx, TextColor, TextFont,
    TextPipeline,
};
use bevy::ui::{BackgroundColor, ComputedNode, UiGlobalTransform};

use renzora::text_mesh::{build_text_mesh, SdfTextMaterial, WORLD_UNITS_PER_PX};

use super::components::UiCanvas;
use super::world_panel::{canvas_resolution, canvas_size, WorldUiPanelLive, WorldUiPanelOwner};

/// Marks a child entity holding emitted text geometry, so it can be torn down and
/// rebuilt when the panel's content changes.
#[derive(Component)]
pub struct WorldUiTextGeom;

/// The content hash of the geometry a panel last emitted. While the walk keeps
/// producing this same hash, the emitter leaves everything as-is — no per-frame
/// mesh/texture churn (see the module docs). `sync_world_ui_panels` removes it on
/// any panel change to force a clean rebuild (e.g. a live texture→mesh switch).
#[derive(Component)]
pub(crate) struct WorldUiMeshBuilt(u64);

pub(crate) fn register(app: &mut App) {
    // Register the shared SDF material + shader (idempotent — text3d may also call
    // it; the guard dedupes since both link the same rlib type).
    renzora::text_mesh::ensure_sdf_material(app);
    app.add_systems(Update, emit_world_ui_meshes);
}

/// An accumulating vertex buffer for one mesh (the background rects).
#[derive(Default)]
struct Buf {
    positions: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl Buf {
    fn clear(&mut self) {
        self.positions.clear();
        self.colors.clear();
        self.normals.clear();
        self.uvs.clear();
        self.indices.clear();
    }

    /// Quad centred at `(cx, cy, z)`, half-extents `(hw, hh)`, colour `col`.
    fn quad(&mut self, cx: f32, cy: f32, z: f32, hw: f32, hh: f32, col: [f32; 4]) {
        let base = self.positions.len() as u32;
        self.positions.push([cx - hw, cy - hh, z]);
        self.positions.push([cx + hw, cy - hh, z]);
        self.positions.push([cx + hw, cy + hh, z]);
        self.positions.push([cx - hw, cy + hh, z]);
        for _ in 0..4 {
            self.colors.push(col);
            self.normals.push([0.0, 0.0, 1.0]);
            self.uvs.push([0.0, 0.0]);
        }
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    fn into_mesh(self) -> Mesh {
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, self.colors);
        mesh.insert_indices(Indices::U32(self.indices));
        mesh
    }
}

/// A text node collected during the walk, built into a mesh only on rebuild.
struct TextNode {
    /// Panel-local centre of the node (world units, pre-child-scale).
    center: Vec2,
    entity: Entity,
    size_px: f32,
    color: LinearRgba,
}

#[derive(Default)]
struct WorldUiMeshScratch {
    rects: Buf,
    texts: Vec<TextNode>,
    queue: std::collections::VecDeque<Entity>,
}

fn hash_font(h: &mut impl Hasher, font: &bevy::text::FontSource) {
    std::mem::discriminant(font).hash(h);
    match font {
        bevy::text::FontSource::Handle(handle) => handle.id().hash(h),
        bevy::text::FontSource::Family(name) => name.hash(h),
        _ => {}
    }
}

fn hash_text_style(
    h: &mut impl Hasher,
    center: Vec2,
    size: f32,
    color: LinearRgba,
    font: &bevy::text::FontSource,
) {
    hash_font(h, font);
    for value in [
        center.x,
        center.y,
        size,
        color.red,
        color.green,
        color.blue,
        color.alpha,
    ] {
        hash_f32(h, value);
    }
}

/// Fold a float into the running content hash by its exact bit pattern.
fn hash_f32(h: &mut impl Hasher, f: f32) {
    h.write_u32(f.to_bits());
}

fn hash_text_content(h: &mut impl Hasher, text: &str) {
    // Hash the byte slice in bulk, with Hash's string boundary marker.
    text.hash(h);
}

/// Bevy's text pipeline resources, bundled so the emitter stays under the 16
/// system-param cap. [`build_text_mesh`] drives all of these to lay out and
/// rasterize a string (see its signature).
#[derive(SystemParam)]
struct TextCtx<'w> {
    pipeline: ResMut<'w, TextPipeline>,
    fonts: Res<'w, Assets<Font>>,
    atlas_set: ResMut<'w, FontAtlasSet>,
    font_cx: ResMut<'w, FontCx>,
    layout_cx: ResMut<'w, LayoutCx>,
    scale_cx: ResMut<'w, ScaleCx>,
    rem: Res<'w, RemSize>,
}

#[derive(Default)]
struct MeshInvalidation {
    owners: HashMap<Entity, HashSet<Entity>>,
    members: HashMap<Entity, Vec<Entity>>,
    dirty: HashSet<Entity>,
    visiting: Vec<Entity>,
}

impl MeshInvalidation {
    fn changed(&mut self, entity: Entity) {
        if let Some(owners) = self.owners.get(&entity) {
            self.dirty.extend(owners.iter().copied());
        }
    }

    fn forget(&mut self, owner: Entity) {
        if let Some(members) = self.members.remove(&owner) {
            for entity in members {
                if let Some(owners) = self.owners.get_mut(&entity) {
                    owners.remove(&owner);
                    if owners.is_empty() { self.owners.remove(&entity); }
                }
            }
        }
        self.dirty.remove(&owner);
    }

    fn finish(&mut self, owner: Entity) {
        self.dirty.remove(&owner);
        if self.members.get(&owner).is_some_and(|members| *members == self.visiting) { return; }
        // Text/layout edits do not rebuild membership tables. Only actual tree
        // changes retire the old routing, keeping animated canvases inexpensive.
        let members = std::mem::take(&mut self.visiting);
        self.forget(owner);
        for &entity in &members { self.owners.entry(entity).or_default().insert(owner); }
        self.members.insert(owner, members);
    }
}

#[derive(SystemParam)]
struct MeshChanges<'w, 's> {
    filters: Res<'w, bevy::ecs::entity_disabling::DefaultQueryFilters>,
    changed: Query<
        'w,
        's,
        Entity,
        (
            Allow<bevy::ecs::entity_disabling::Disabled>,
            Or<(
                Changed<ComputedNode>,
                Changed<UiGlobalTransform>,
                Changed<BackgroundColor>,
                Changed<Text>,
                Changed<TextFont>,
                Changed<TextColor>,
                Changed<Children>,
                Changed<bevy::ecs::entity_disabling::Disabled>,
            )>,
        ),
    >,
    layout: RemovedComponents<'w, 's, ComputedNode>,
    transform: RemovedComponents<'w, 's, UiGlobalTransform>,
    background: RemovedComponents<'w, 's, BackgroundColor>,
    text: RemovedComponents<'w, 's, Text>,
    font: RemovedComponents<'w, 's, TextFont>,
    color: RemovedComponents<'w, 's, TextColor>,
    children: RemovedComponents<'w, 's, Children>,
    disabled: RemovedComponents<'w, 's, bevy::ecs::entity_disabling::Disabled>,
    state: Local<'s, MeshInvalidation>,
}

#[allow(clippy::too_many_arguments)]
fn emit_world_ui_meshes(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut sdf_materials: ResMut<Assets<SdfTextMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut tcx: TextCtx,
    font_revision: Res<renzora::text_mesh::FontAssetRevision>,
    panels: Query<(
        Entity,
        Ref<UiCanvas>,
        Option<Ref<WorldUiPanelLive>>,
        Option<&WorldUiMeshBuilt>,
    )>,
    children: Query<&Children>,
    text_children: Query<(), With<WorldUiTextGeom>>,
    nodes: Query<(
        &ComputedNode,
        &UiGlobalTransform,
        Option<&BackgroundColor>,
        Option<&Text>,
        Option<&TextFont>,
        Option<&TextColor>,
    )>,
    mut rect_mat: Local<Option<Handle<StandardMaterial>>>,
    mut scratch: Local<WorldUiMeshScratch>,
    mut changes: MeshChanges,
    #[cfg(test)] mut measured: Option<ResMut<MeshWorkMeasured>>,
) {
    let MeshChanges {
        filters,
        changed,
        layout,
        transform,
        background,
        text,
        font,
        color,
        children: removed_children,
        disabled,
        state,
    } = &mut changes;
    // Unknown third-party disabling markers need the original eligibility walk.
    let custom_filters = filters.disabling_ids().count() > 1;
    for entity in changed
        .iter()
        .chain(layout.read())
        .chain(transform.read())
        .chain(background.read())
        .chain(text.read())
        .chain(font.read())
        .chain(color.read())
        .chain(removed_children.read())
        .chain(disabled.read())
    {
        state.changed(entity);
    }
    // Retire memberships when a canvas disappears or is temporarily excluded.
    let retired: Vec<_> = state
        .members
        .keys()
        .filter(|entity| panels.get(**entity).is_err())
        .copied()
        .collect();
    for owner in retired {
        state.forget(owner);
    }
    for (entity, canvas, live, built) in &panels {
        if !canvas.is_world() || !canvas.is_mesh_mode() {
            state.forget(entity);
            continue;
        }
        // Mesh mode but no template/root (e.g. the template was cleared) → drop any
        // text this canvas had emitted so stale glyphs don't sit on the dark surface
        // `sync_world_ui_canvases` now shows. The canvas's own dark quad comes from
        // there; here we only clean up our child geometry.
        let Some(live) = live else {
            state.forget(entity);
            if let Ok(ch) = children.get(entity) {
                for c in ch.iter() {
                    if text_children.get(c).is_ok() {
                        commands.entity(c).despawn();
                    }
                }
            }
            if built.is_some() {
                commands.entity(entity).remove::<WorldUiMeshBuilt>();
            }
            continue;
        };
        if !custom_filters
            && built.is_some()
            && state.members.contains_key(&entity)
            && !state.dirty.contains(&entity)
            && !canvas.is_changed()
            && !live.is_changed()
            && !font_revision.is_changed()
            && !tcx.rem.is_changed()
        {
            continue;
        }
        state.visiting.clear();
        let panel_size = canvas_size(&canvas);
        let res = canvas_resolution(&canvas).as_vec2();
        if res.x <= 0.0 || res.y <= 0.0 {
            continue;
        }
        let scale = panel_size / res; // world units per UI px
                                      // px (y-down, origin top-left) → panel-local world (centred, y-up).
        let to_local = |px: Vec2| {
            Vec2::new(
                (px.x - res.x * 0.5) * scale.x,
                -(px.y - res.y * 0.5) * scale.y,
            )
        };

        // One shared high-water allocation, not another buffer per canvas.
        let WorldUiMeshScratch {
            rects,
            texts,
            queue,
        } = &mut *scratch;
        rects.clear();
        texts.clear();
        queue.clear();

        // Content hash accumulated over the DETERMINISTIC breadth-first walk, so
        // an unchanged layout hashes identically frame to frame.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hash_f32(&mut hasher, panel_size.x);
        hash_f32(&mut hasher, panel_size.y);
        hash_f32(&mut hasher, res.x);
        hash_f32(&mut hasher, res.y);

        // Breadth-first from the panel root: parents before children.
        queue.push_back(live.ui_root);
        let mut order = 0u32;
        while let Some(n) = queue.pop_front() {
            state.visiting.push(n);
            #[cfg(test)]
            if let Some(measured) = measured.as_mut() {
                measured.visits += 1;
            }
            if let Ok((cn, gt, bg, text, tf, tc)) = nodes.get(n) {
                let center = gt.translation;
                // Skip any node bevy hasn't finished laying out — a non-finite
                // transform would emit garbage triangles reaching to infinity.
                if center.is_finite() {
                    let c = to_local(center);
                    if let Some(bg) = bg {
                        let lin = bg.0.to_linear();
                        if lin.alpha > 0.0 && cn.size.x > 0.0 && cn.size.y > 0.0 {
                            let col = [lin.red, lin.green, lin.blue, lin.alpha];
                            rects.quad(
                                c.x,
                                c.y,
                                order as f32 * 0.0005,
                                cn.size.x * 0.5 * scale.x,
                                cn.size.y * 0.5 * scale.y,
                                col,
                            );
                            hasher.write_u32(order);
                            for v in [c.x, c.y, cn.size.x, cn.size.y] {
                                hash_f32(&mut hasher, v);
                            }
                            for v in col {
                                hash_f32(&mut hasher, v);
                            }
                        }
                    }
                    if let (Some(text), Some(tf)) = (text, tf) {
                        let s = text.0.trim();
                        if !s.is_empty() {
                            let size_px = match tf.font_size {
                                FontSize::Px(p) => p,
                                _ => tcx.rem.0,
                            };
                            let color = tc.map(|c| c.0.to_linear()).unwrap_or(LinearRgba::WHITE);
                            hash_text_content(&mut hasher, s);
                            hash_text_style(&mut hasher, c, size_px, color, &tf.font);
                            texts.push(TextNode {
                                center: c,
                                entity: n,
                                size_px,
                                color,
                            });
                        }
                    }
                }
            }
            if let Ok(ch) = children.get(n) {
                for c in ch.iter() {
                    queue.push_back(c);
                }
            }
            order += 1;
        }

        state.finish(entity);
        // Nothing changed since the last build → leave all geometry untouched.
        if !texts.is_empty() {
            font_revision.0.hash(&mut hasher);
        }
        let hash = hasher.finish();
        if built.map(|b| b.0) == Some(hash) {
            continue;
        }

        // ── Background mesh on the panel entity ──
        if !rects.is_empty() {
            if rect_mat.is_none() {
                *rect_mat = Some(materials.add(StandardMaterial {
                    base_color: Color::WHITE,
                    unlit: true,
                    // OPAQUE on purpose: the background is one mesh with a single
                    // transparent-sort position (the panel centre), so if it were
                    // blended, any label farther than that centre would sort behind
                    // it and get painted over — text vanished on the panel's far
                    // side at an angle. Opaque writes depth in the opaque pass; the
                    // blended text then depth-tests in front and always shows.
                    alpha_mode: AlphaMode::Opaque,
                    cull_mode: None,
                    ..default()
                }));
            }
            let h = meshes.add(std::mem::take(rects).into_mesh());
            commands.entity(entity).insert((
                Mesh3d(h),
                MeshMaterial3d(rect_mat.as_ref().unwrap().clone()),
            ));
        } else {
            // Empty authored geometry must replace the previous output too.
            // The no-template fallback belongs to sync_world_ui_canvases and
            // returns above; text remains independently owned by child meshes.
            commands
                .entity(entity)
                .remove::<(Mesh3d, MeshMaterial3d<StandardMaterial>)>();
        }

        // ── Rebuild text child meshes via the shared 3D-text builder ──
        if let Ok(ch) = children.get(entity) {
            for c in ch.iter() {
                if text_children.get(c).is_ok() {
                    commands.entity(c).despawn();
                }
            }
        }
        // `build_text_mesh` emits at a fixed WORLD_UNITS_PER_PX; the child's scale
        // maps that onto this panel's px→world scale (so a 24px label is 24px tall
        // on the panel, per axis). A font still loading yields None — leave the
        // hash unset so we retry next frame instead of freezing on missing text.
        let mut all_ready = true;
        let child_scale = Vec3::new(
            scale.x / WORLD_UNITS_PER_PX,
            scale.y / WORLD_UNITS_PER_PX,
            1.0,
        );
        // Put ALL text in a z-band above every rect (rects top out at
        // `order * 0.0005`), so a deep node's blended background can't sort in
        // front of a shallow node's label and make it vanish at some angles.
        let text_base_z = (order as f32 + 2.0) * 0.0005;
        for (i, t) in texts.iter().enumerate() {
            // These queries cannot change during this system. Resolve borrowed
            // text only on rebuild instead of cloning every label every frame.
            let Ok((_, _, _, Some(text), Some(font), _)) = nodes.get(t.entity) else {
                all_ready = false;
                continue;
            };
            let built = build_text_mesh(
                &mut tcx.pipeline,
                &tcx.fonts,
                &mut tcx.atlas_set,
                &mut images,
                &mut tcx.font_cx,
                &mut tcx.layout_cx,
                &mut tcx.scale_cx,
                tcx.rem.0,
                font.font.clone(),
                text.0.trim(),
                t.size_px,
            );
            let Some((mesh, strip)) = built else {
                all_ready = false;
                continue;
            };
            let mesh_h = meshes.add(mesh);
            let mat = sdf_materials.add(SdfTextMaterial {
                color: t.color,
                atlas: strip,
            });
            // Tiny per-label bump breaks ties between overlapping text runs.
            let z = text_base_z + i as f32 * 0.0001;
            commands.spawn((
                Mesh3d(mesh_h),
                MeshMaterial3d(mat),
                Transform::from_translation(Vec3::new(t.center.x, t.center.y, z))
                    .with_scale(child_scale),
                Visibility::default(),
                // Glyph meshes are tiny and the AABB of scaled generated geometry
                // is culling them in and out with the camera — just never cull.
                NoFrustumCulling,
                // Generated chrome: keep it out of the scene hierarchy AND out of
                // the selection outline (which would trace every glyph as stray
                // dotted lines — see renzora_gizmo::selection_visuals).
                renzora::HideInHierarchy,
                WorldUiTextGeom,
                WorldUiPanelOwner(entity),
                ChildOf(entity),
            ));
        }

        if all_ready {
            commands.entity(entity).insert(WorldUiMeshBuilt(hash));
        } else {
            // A font wasn't ready — force a rebuild next frame.
            commands.entity(entity).remove::<WorldUiMeshBuilt>();
        }
    }
}

#[cfg(test)]
#[derive(Resource, Default)]
struct MeshWorkMeasured { visits: usize }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hashing_processes_long_strings_in_bulk() {
        #[derive(Default)]
        struct CountedHash {
            inner: std::collections::hash_map::DefaultHasher,
            writes: usize,
        }
        impl Hasher for CountedHash {
            fn finish(&self) -> u64 {
                self.inner.finish()
            }
            fn write(&mut self, bytes: &[u8]) {
                self.writes += 1;
                self.inner.write(bytes);
            }
        }
        let text = "world-space text ".repeat(256);
        let mut old = CountedHash::default();
        for byte in text.as_bytes() {
            old.write_u8(*byte);
        }
        let mut new = CountedHash::default();
        hash_text_content(&mut new, &text);
        assert_eq!(old.writes, text.len());
        assert_eq!(new.writes, 2);
        let mut edited = CountedHash::default();
        hash_text_content(&mut edited, &(text + "!"));
        assert_ne!(new.finish(), edited.finish());
    }

    #[test]
    fn scratch_rectangles_retain_all_five_allocations() {
        let mut buf = Buf::default();
        buf.quad(1.0, 2.0, 3.0, 4.0, 5.0, [1.0; 4]);
        let storage = (
            buf.positions.as_ptr(),
            buf.colors.as_ptr(),
            buf.normals.as_ptr(),
            buf.uvs.as_ptr(),
            buf.indices.as_ptr(),
        );
        for _ in 0..1_000 {
            buf.clear();
            assert!(buf.is_empty());
            buf.quad(1.0, 2.0, 3.0, 4.0, 5.0, [1.0; 4]);
            assert_eq!(
                storage,
                (
                    buf.positions.as_ptr(),
                    buf.colors.as_ptr(),
                    buf.normals.as_ptr(),
                    buf.uvs.as_ptr(),
                    buf.indices.as_ptr()
                )
            );
            assert_eq!(buf.positions.len(), 4);
            assert_eq!(buf.indices, [0, 1, 2, 0, 2, 3]);
        }
    }

    #[test]
    fn font_and_alpha_participate_in_text_hash() {
        let hash = |font: bevy::text::FontSource, alpha| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            hash_text_style(
                &mut h,
                Vec2::ZERO,
                24.0,
                LinearRgba::new(1.0, 1.0, 1.0, alpha),
                &font,
            );
            h.finish()
        };
        let initial = hash(bevy::text::FontSource::Serif, 1.0);
        assert_eq!(initial, hash(bevy::text::FontSource::Serif, 1.0));
        assert_ne!(initial, hash(bevy::text::FontSource::SansSerif, 1.0));
        assert_ne!(initial, hash(bevy::text::FontSource::Serif, 0.5));
        assert_ne!(
            hash(bevy::text::FontSource::Family("first".into()), 1.0),
            hash(bevy::text::FontSource::Family("second".into()), 1.0)
        );
    }

    fn mesh_app() -> App {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<SdfTextMaterial>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<Assets<Font>>()
            .init_resource::<TextPipeline>()
            .init_resource::<FontAtlasSet>()
            .init_resource::<FontCx>()
            .init_resource::<LayoutCx>()
            .init_resource::<ScaleCx>()
            .init_resource::<RemSize>()
            .add_systems(Update, emit_world_ui_meshes);
        renzora::text_mesh::ensure_font_asset_tracking(&mut app);
        app
    }

    #[test]
    fn scene_shaped_ui_workload() {
        let mut app = mesh_app();
        app.init_resource::<MeshWorkMeasured>();
        let root = app.world_mut().spawn_empty().id();
        for _ in 0..1_024 {
            app.world_mut().spawn((
                ComputedNode {
                    size: Vec2::splat(10.0),
                    ..default()
                },
                UiGlobalTransform::default(),
                BackgroundColor(Color::WHITE),
                ChildOf(root),
            ));
        }
        let canvas = app
            .world_mut()
            .spawn((
                UiCanvas {
                    render_space: "world".into(),
                    render_mode: "mesh".into(),
                    ..default()
                },
                WorldUiPanelLive {
                    camera: Entity::PLACEHOLDER,
                    ui_root: root,
                    image: Handle::default(),
                },
            ))
            .id();
        app.update();
        app.update();
        app.world_mut().resource_mut::<MeshWorkMeasured>().visits = 0;
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            app.update();
        }
        eprintln!(
            "UI scene: 1024 nodes, 1000 settled frames: {:?}, {} node visits",
            started.elapsed(),
            app.world().resource::<MeshWorkMeasured>().visits
        );
        assert_eq!(app.world().resource::<MeshWorkMeasured>().visits, 0);
        assert!(app.world().get::<Mesh3d>(canvas).is_some());
    }

    #[test]
    fn dirty_canvases_follow_membership_removal_and_reactivation() {
        use bevy::ecs::entity_disabling::Disabled;
        let mut app = mesh_app();
        app.init_resource::<MeshWorkMeasured>();
        let roots = [
            app.world_mut().spawn_empty().id(),
            app.world_mut().spawn_empty().id(),
        ];
        let canvases = roots.map(|root| {
            app.world_mut()
                .spawn((
                    UiCanvas {
                        render_space: "world".into(),
                        render_mode: "mesh".into(),
                        ..default()
                    },
                    WorldUiPanelLive {
                        camera: Entity::PLACEHOLDER,
                        ui_root: root,
                        image: Handle::default(),
                    },
                ))
                .id()
        });
        let leaf = app
            .world_mut()
            .spawn((
                ComputedNode {
                    size: Vec2::splat(10.0),
                    ..default()
                },
                UiGlobalTransform::default(),
                BackgroundColor(Color::WHITE),
                ChildOf(roots[0]),
            ))
            .id();
        app.update();
        app.update();
        assert!(app.world().get::<Mesh3d>(canvases[0]).is_some());
        assert!(app.world().get::<Mesh3d>(canvases[1]).is_none());
        app.world_mut().entity_mut(leaf).insert(ChildOf(roots[1]));
        app.update();
        assert!(app.world().get::<Mesh3d>(canvases[0]).is_none());
        assert!(app.world().get::<Mesh3d>(canvases[1]).is_some());
        app.world_mut().entity_mut(leaf).insert(Disabled);
        app.update();
        assert!(app.world().get::<Mesh3d>(canvases[1]).is_none());
        app.world_mut().entity_mut(leaf).remove::<Disabled>();
        app.update();
        assert!(app.world().get::<Mesh3d>(canvases[1]).is_some());
        app.world_mut().entity_mut(leaf).remove::<ComputedNode>();
        app.update();
        assert!(app.world().get::<Mesh3d>(canvases[1]).is_none());
        // Unrelated UI churn must not walk either canvas.
        app.update();
        app.world_mut().resource_mut::<MeshWorkMeasured>().visits = 0;
        app.world_mut()
            .spawn((ComputedNode::default(), UiGlobalTransform::default()));
        app.update();
        assert_eq!(app.world().resource::<MeshWorkMeasured>().visits, 0);
    }

    #[test]
    fn custom_disabling_keeps_mesh_eligibility() {
        #[derive(Component)]
        struct Hidden;
        let mut app = mesh_app();
        let marker = app.world_mut().register_component::<Hidden>();
        app.world_mut()
            .resource_mut::<bevy::ecs::entity_disabling::DefaultQueryFilters>()
            .register_disabling_component(marker);
        let root = app
            .world_mut()
            .spawn((
                ComputedNode {
                    size: Vec2::splat(10.0),
                    ..default()
                },
                UiGlobalTransform::default(),
                BackgroundColor(Color::WHITE),
            ))
            .id();
        let canvas = app
            .world_mut()
            .spawn((
                UiCanvas {
                    render_space: "world".into(),
                    render_mode: "mesh".into(),
                    ..default()
                },
                WorldUiPanelLive {
                    camera: Entity::PLACEHOLDER,
                    ui_root: root,
                    image: Handle::default(),
                },
            ))
            .id();
        app.update();
        app.update();
        assert!(app.world().get::<Mesh3d>(canvas).is_some());
        app.world_mut().entity_mut(root).insert(Hidden);
        app.update();
        assert!(app.world().get::<Mesh3d>(canvas).is_none());
        app.world_mut().entity_mut(root).remove::<Hidden>();
        app.update();
        assert!(app.world().get::<Mesh3d>(canvas).is_some());
    }

    #[test]
    fn settled_canvas_keeps_mesh_and_layout_edit_rebuilds() {
        let mut app = mesh_app();
        let root = app
            .world_mut()
            .spawn((
                ComputedNode {
                    size: Vec2::splat(100.0),
                    ..default()
                },
                UiGlobalTransform::default(),
                BackgroundColor(Color::WHITE),
            ))
            .id();
        let canvas = app
            .world_mut()
            .spawn((
                UiCanvas {
                    render_space: "world".into(),
                    render_mode: "mesh".into(),
                    ..default()
                },
                WorldUiPanelLive {
                    camera: Entity::PLACEHOLDER,
                    ui_root: root,
                    image: Handle::default(),
                },
            ))
            .id();
        app.update();
        let first = app.world().get::<Mesh3d>(canvas).unwrap().0.clone();
        for _ in 0..1_000 {
            app.update();
            assert_eq!(app.world().get::<Mesh3d>(canvas).unwrap().0, first);
        }
        app.world_mut()
            .get_mut::<ComputedNode>(root)
            .unwrap()
            .size
            .x = 200.0;
        app.update();
        assert_ne!(app.world().get::<Mesh3d>(canvas).unwrap().0, first);

        // Each route to an empty background must retire the previous mesh, and
        // restoring it must work even after the empty hash has settled.
        for empty_kind in 0..3 {
            match empty_kind {
                0 => {
                    app.world_mut().entity_mut(root).remove::<BackgroundColor>();
                }
                1 => {
                    app.world_mut()
                        .entity_mut(root)
                        .insert(BackgroundColor(Color::NONE));
                }
                _ => {
                    app.world_mut().get_mut::<ComputedNode>(root).unwrap().size = Vec2::ZERO;
                }
            }
            for _ in 0..3 {
                app.update();
                assert!(app.world().get::<Mesh3d>(canvas).is_none());
                assert!(app
                    .world()
                    .get::<MeshMaterial3d<StandardMaterial>>(canvas)
                    .is_none());
                assert!(app.world().get::<WorldUiMeshBuilt>(canvas).is_some());
            }
            app.world_mut()
                .entity_mut(root)
                .insert(BackgroundColor(Color::WHITE));
            app.world_mut().get_mut::<ComputedNode>(root).unwrap().size = Vec2::splat(100.0);
            app.update();
            assert!(app.world().get::<Mesh3d>(canvas).is_some());
            assert!(app
                .world()
                .get::<MeshMaterial3d<StandardMaterial>>(canvas)
                .is_some());
        }

        // A canvas without a template keeps the fallback supplied by the panel
        // synchronizer, even when the laid-out root has no backgrounds.
        let fallback = app.world().get::<Mesh3d>(canvas).unwrap().0.clone();
        app.world_mut()
            .entity_mut(canvas)
            .remove::<WorldUiPanelLive>();
        app.world_mut().entity_mut(root).remove::<BackgroundColor>();
        app.update();
        assert_eq!(app.world().get::<Mesh3d>(canvas).unwrap().0, fallback);
        assert!(app.world().get::<WorldUiMeshBuilt>(canvas).is_none());

        // Replace actual font bytes at the same asset ID. Both the collection
        // and the mesh hash must advance, not merely the authored font source.
        app.add_message::<AssetEvent<Font>>();
        app.add_systems(PostUpdate, bevy::text::load_font_assets_into_font_collection);
        let font = app.world_mut().resource_mut::<Assets<Font>>().add(Font::from_bytes(
            include_bytes!("../../../../assets/fonts/NotoSans-Regular.ttf").to_vec(),
        ));
        app.world_mut().entity_mut(root).insert((Text::new("WWWiii"), TextFont {
            font: bevy::text::FontSource::Handle(font.clone()),
            ..default()
        }));
        app.world_mut().entity_mut(canvas).insert(WorldUiPanelLive {
            camera: Entity::PLACEHOLDER, ui_root: root, image: Handle::default(),
        });
        app.update();
        app.update();
        let text_mesh = |app: &App| {
            app.world().get::<Children>(canvas).unwrap().iter().find_map(|child| {
                app.world().get::<WorldUiTextGeom>(child)?;
                Some(app.world().get::<Mesh3d>(child).unwrap().0.clone())
            }).unwrap()
        };
        let old_mesh = text_mesh(&app);
        let positions = |app: &App, handle: &Handle<Mesh>| {
            app.world().resource::<Assets<Mesh>>().get(handle).unwrap()
                .attribute(Mesh::ATTRIBUTE_POSITION).unwrap().as_float3().unwrap().to_vec()
        };
        let old_positions = positions(&app, &old_mesh);
        for _ in 0..3 { app.update(); }
        assert_eq!(text_mesh(&app), old_mesh);
        app.world_mut().resource_mut::<Assets<Font>>().insert(font.id(), Font::from_bytes(
            include_bytes!("../../../../assets/fonts/JetBrainsMono-Regular.ttf").to_vec(),
        )).unwrap();
        app.world_mut().write_message(AssetEvent::<Font>::Modified { id: font.id() });
        app.update();
        app.update();
        let new_mesh = text_mesh(&app);
        assert_ne!(new_mesh, old_mesh);
        assert_ne!(positions(&app, &new_mesh), old_positions);
        for _ in 0..3 { app.update(); }
        assert_eq!(text_mesh(&app), new_mesh);
    }
}
