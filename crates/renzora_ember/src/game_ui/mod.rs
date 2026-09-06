#![allow(unused_variables, unused_assignments, dead_code)]

//! Renzora Game UI — bevy_ui game interface components.
//!
//! **Runtime** (this crate, always lean):
//! - `UiCanvas`, `UiWidget`, `UiWidgetType` — serializable marker components
//! - Widget data components (`ProgressBarData`, `SliderData`, etc.)
//! - Runtime systems that drive widget behavior
//! - `GameUiPlugin` — registers types for reflection + runtime systems
//!
//! All editor-only code (inspector registrations, the WYSIWYG canvas, the
//! render target, presets, view auto-switching, debug logging) lives in the
//! separate `renzora_game_ui_editor` crate, which depends on this one.

pub mod components;
pub mod script_extension;
pub mod shapes;
pub mod spawn;
pub mod systems;
pub mod script_canvas;
pub mod world_panel;
pub mod world_ui_mesh;

use bevy::ecs::entity_disabling::{DefaultQueryFilters, Disabled};
use bevy::ecs::query::Allow;
use bevy::prelude::*;

pub use components::{
    HtmlTemplatePath, HuiBuildOnSelf, UiCanvas, UiTheme, UiThemed, UiWidget, UiWidgetType,
};

#[derive(Default)]
pub struct GameUiPlugin;

impl Plugin for GameUiPlugin {
    fn build(&self, app: &mut App) {
        // ── Reflection registration ─────────────────────────────────────
        app.register_type::<components::UiCanvas>();
        app.register_type::<components::UiWidget>();
        app.register_type::<components::HtmlTemplatePath>();
        app.register_type::<components::HuiBuildOnSelf>();
        app.register_type::<components::UiWidgetPart>();
        // Single-entity primitive (replaces ProgressBar / HealthBar / LoadingScreen)
        app.register_type::<components::UiBarFill>();
        app.register_type::<components::ProgressDirection>();
        // Form inputs
        app.register_type::<components::SliderData>();
        app.register_type::<components::CheckboxData>();
        app.register_type::<components::ToggleData>();
        app.register_type::<components::RadioButtonData>();
        app.register_type::<components::DropdownData>();
        app.register_type::<components::TextInputData>();
        app.register_type::<components::NumberInputData>();
        // Layout / overlay primitives
        app.register_type::<components::ScrollViewData>();
        app.register_type::<components::TooltipData>();
        app.register_type::<components::ModalData>();
        app.register_type::<components::DraggableWindowData>();
        app.register_type::<components::SeparatorData>();
        app.register_type::<components::SeparatorDirection>();
        app.register_type::<components::ScrollbarData>();
        app.register_type::<components::ScrollbarOrientation>();
        app.register_type::<components::UiImagePath>();
        // Settings UI rows (used by editor settings panel)
        app.register_type::<components::KeybindRowData>();
        app.register_type::<components::SettingsRowData>();
        app.register_type::<components::SettingsControlType>();
        // Widget style components
        app.register_type::<components::UiFill>();
        app.register_type::<components::UiStroke>();
        app.register_type::<components::UiBorderRadius>();
        app.register_type::<components::UiBoxShadow>();
        app.register_type::<components::UiOpacity>();
        app.register_type::<components::UiClipContent>();
        app.register_type::<components::UiCursor>();
        app.register_type::<components::UiTextStyle>();
        app.register_type::<components::UiPadding>();
        // Interaction & animation
        app.register_type::<components::UiInteractionStyle>();
        app.register_type::<components::UiTransition>();
        app.register_type::<components::UiTween>();
        // Theming
        app.register_type::<components::UiTheme>();
        app.register_type::<components::UiThemed>();

        // ── Default theme resource ────────────────────────────────────
        app.init_resource::<components::UiTheme>();

        // ── Script actions (decoupled — observes ScriptAction events) ──
        app.add_observer(script_extension::handle_ui_script_actions);

        // ── Auto-layout on reparent ────────────────────────────────────
        // When a UI widget is dragged to a new parent in the hierarchy,
        // re-apply parent-aware positioning: Container parent → Relative
        // (flex flow), Canvas parent → Absolute (free placement). The
        // Changed-filtered system covers runtime drag-reparents; the
        // Insert observer covers the scene-load case (reflection inserts
        // bypass change detection).
        app.add_systems(Update, on_widget_reparented);
        app.add_observer(on_childof_inserted);

        // ── UI canvas invariant ────────────────────────────────────────
        // Widgets must stay under a `UiCanvas` (their scoping camera) and a
        // canvas must never nest under a widget. These healers re-establish
        // that after any reparent — drag, undo-restore, scene load, or script.
        // See the systems below for why the leak is otherwise invisible-yet-
        // destructive (an orphaned widget renders into the editor's own UI).
        // …and a canvas root must keep the geometry of the surface it *is* —
        // see `heal_canvas_root_geometry`, which is what repairs a scene that
        // already has a collapsed canvas saved in it.
        app.add_systems(
            Update,
            (
                heal_orphaned_ui_widgets,
                heal_nested_ui_canvases,
                heal_canvas_root_geometry,
            ),
        );

        // Visibility-mode binding: same dual-path setup as the reparent
        // logic. The Changed system handles inspector edits to the
        // mode dropdown; the observer applies the saved mode on scene
        // load when reflection inserts skip change-tick propagation.
        app.add_observer(on_canvas_inserted);
        app.add_observer(on_game_ui_node_inserted);

        // ── Shape primitives ────────────────────────────────────────────
        app.add_plugins(shapes::ShapesPlugin);

        // ── World-space panels ──────────────────────────────────────────
        // Templates rendered onto a quad in the 3D scene, clickable from a VR
        // wand or the mouse. Owns the `WorldUiPointers` consumer, so it's also
        // the one place that orders the contract's two sets.
        world_panel::register(app);
        world_ui_mesh::register(app);
        script_canvas::register(app);

        // ── Canvas scaler & visibility-mode ──────────────────────────────
        //
        // `update_ui_scale` adjusts the global `UiScale` to fit the 3D
        // viewport's render target. Useful in the shipped game (UI scales with
        // the window), but in the editor it would also scale the UI rendered to
        // our fixed 1280×720 editor render target — making a Node with
        // `width: Px(100)` show up as some other pixel count depending on the
        // editor window size. So we skip it in an editor session; UiScale stays
        // at the default 1.0 and the canvas tab renders 1:1 with what the user
        // authors.
        //
        // Runtime-gated on `EditorSession` (NOT `#[cfg]`): under the single
        // `--workspace` editor build the old `#[cfg(not(editor))]` compiled this
        // OUT of the shipped game too, so exported games never scaled their UI.
        let is_editor = app
            .world()
            .get_resource::<renzora::EditorSession>()
            .map(|s| s.0)
            .unwrap_or(false);
        if !is_editor {
            app.add_systems(Update, update_ui_scale);
        }
        app.add_systems(
            Update,
            (
                rehydrate_ui_images,
                sync_ui_zindex,
                apply_canvas_visibility_mode,
            ),
        );

        // ── Runtime widget systems ──────────────────────────────────────
        app.add_systems(
            Update,
            (
                systems::apply_bar_fill,
                systems::slider_system,
                systems::checkbox_system,
                systems::toggle_system,
                systems::radio_button_system,
                systems::tooltip_system,
                systems::dropdown_system,
                systems::dropdown_option_system,
                systems::modal_system,
                systems::draggable_window_system,
                systems::separator_system,
                systems::number_input_system,
                systems::scrollbar_system,
                systems::keybind_row_system,
                systems::settings_row_system,
                systems::interaction_style_system,
                systems::ui_theme_system,
                systems::ui_tween_system,
                systems::ensure_style_components,
                systems::apply_widget_style_system,
            ),
        );

        info!("[runtime] GameUiPlugin");
    }
}

// ── Canvas visibility_mode → Visibility ──────────────────────────────────
//
// `UiCanvas.visibility_mode` is the user-facing dropdown ("always",
// "play_only", "editor_only"). Until now it was a hint nothing read.
// This system writes the actual Bevy `Visibility` component from it
// whenever the canvas is freshly added or the dropdown changes.
//
// Runs in both editor and runtime — `PlayModeState` is optional, so in
// runtime builds (no PlayModeState resource) `in_play` defaults to true,
// making "play_only" canvases visible at runtime, "editor_only" hidden,
// and "always" always visible. Scripts can still override via
// `ui_show` / `ui_hide` afterward; the system only fires when the
// canvas component itself changes (`Changed<UiCanvas>`), not every frame.

fn apply_canvas_visibility_mode(
    play_mode: Option<Res<renzora::PlayModeState>>,
    mut canvases: Query<(&UiCanvas, &mut Visibility), Changed<UiCanvas>>,
) {
    let in_play = play_mode.is_none_or(|p| p.is_in_play_mode());
    for (canvas, mut vis) in &mut canvases {
        apply_canvas_visibility_to(in_play, canvas, &mut vis);
    }
}

fn apply_canvas_visibility_to(in_play: bool, canvas: &UiCanvas, vis: &mut Visibility) {
    let visible = match canvas.visibility_mode.as_str() {
        "always" => true,
        "play_only" => in_play,
        "editor_only" => !in_play,
        _ => true,
    };
    let target = if visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    if *vis != target {
        *vis = target;
    }
}

/// Lifecycle observer covering the scene-load case the `Changed`-filtered
/// system above misses. Reflection inserts (used by `DynamicScene::write_to_world`)
/// don't propagate Bevy's change ticks, so a saved canvas with
/// `visibility_mode: "play_only"` would render incorrectly in the editor
/// until the user touched it. The observer fires on insert and applies
/// the same logic.
fn on_canvas_inserted(
    trigger: On<Insert, UiCanvas>,
    play_mode: Option<Res<renzora::PlayModeState>>,
    mut canvases: Query<(&UiCanvas, &mut Visibility)>,
) {
    let entity = trigger.entity;
    let in_play = play_mode.is_none_or(|p| p.is_in_play_mode());
    if let Ok((canvas, mut vis)) = canvases.get_mut(entity) {
        apply_canvas_visibility_to(in_play, canvas, &mut vis);
    }
}

/// Give authored game-UI nodes a `ScriptComponent`, so `<input bind="Entity.var">`
/// has one to resolve to (see `markup::input_field::sync_inputs`).
///
/// This exists because nothing inserts a `ScriptComponent` automatically any
/// more. `renzora_scripting` used to blanket-apply it to every named entity, and
/// the editor's own chrome is by far the largest population of those — ~955 on
/// an empty scene — with each insert costing an archetype move; the observer was
/// first narrowed to skip `bevy_ui` nodes and then dropped entirely. Game UI is
/// the small, controlled population that actually wants the component, so it
/// opts in here rather than waiting for a blanket insert that no longer comes.
fn on_game_ui_node_inserted(
    trigger: On<Insert, (UiWidget, UiCanvas)>,
    mut commands: Commands,
    missing: Query<(), Without<renzora_scripting::ScriptComponent>>,
) {
    let entity = trigger.entity;
    if missing.get(entity).is_ok() {
        commands
            .entity(entity)
            .insert(renzora_scripting::ScriptComponent::new());
    }
}

// ── Reparent system ────────────────────────────────────────────────────────
//
// Fires when a `ChildOf` is inserted *or* replaced on a UI widget entity
// (drag in hierarchy → Replace; spawn → Insert; both surface as
// `Changed<ChildOf>`). Re-runs the parent-aware layout logic so the moved
// widget switches between Absolute (canvas root) and Relative (Container)
// automatically.
//
// Originally written as an `On<Insert, ChildOf>` observer, which missed
// the drag-in-hierarchy case because that fires `Replace` not `Insert`.
// `Changed` filter catches both.

fn on_widget_reparented(
    mut commands: Commands,
    changed: Query<Entity, (With<UiWidget>, Changed<ChildOf>)>,
) {
    for entity in &changed {
        commands.queue(move |world: &mut World| {
            crate::game_ui::spawn::reapply_layout_from_parent(world, entity);
        });
    }
}

/// Lifecycle observer covering the scene-load case the `Changed`-filtered
/// system above misses. `DynamicScene::write_to_world` inserts `ChildOf`
/// via reflection without propagating change ticks, so widgets loaded
/// from a saved scene wouldn't have their parent-aware layout applied
/// (Container parent → Relative, Canvas root → Absolute) until the user
/// touched them.
fn on_childof_inserted(
    trigger: On<Insert, ChildOf>,
    mut commands: Commands,
    widgets: Query<(), With<UiWidget>>,
) {
    let entity = trigger.entity;
    if widgets.get(entity).is_err() {
        return;
    }
    commands.queue(move |world: &mut World| {
        crate::game_ui::spawn::reapply_layout_from_parent(world, entity);
    });
}

// ── UI canvas invariant healers ─────────────────────────────────────────────
//
// A `UiWidget` renders on the right camera *only* because a `UiCanvas` ancestor
// carries `UiTargetCamera` (Bevy propagates it down the child chain). Break that
// chain — drag the widget to the scene root, or under a non-UI entity — and the
// widget becomes a bare root `Node` with no target camera, which Bevy hands to
// the `IsDefaultUiCamera` camera: in the editor that is the *chrome* camera, so
// the widget renders straight into the editor's own UI ("merges into the
// editor"). Worse, a bare root `Node` is indistinguishable from editor chrome,
// so it drops out of the hierarchy panel and can no longer be selected to
// delete — which is why deleting "doesn't work" once a widget has escaped.
//
// Rather than forbid the move, we restore the invariant: an orphaned widget is
// wrapped in a fresh root canvas (having a second canvas is fine), and a canvas
// that got nested under a widget is popped back to the root.
//
// These run as deferred systems, NOT `On<Remove, ChildOf>` observers: recursive
// despawn removes `ChildOf` from every child in a deleted subtree, and an
// observer would try to re-home entities that are about to vanish. A system sees
// the world only after despawn commands have flushed, so doomed widgets are
// already gone and never spuriously healed.

/// Wrap any `UiWidget` that has lost its `UiCanvas` ancestor in a fresh root
/// canvas. Covers a widget dragged to the root (no `ChildOf`) and a widget
/// reparented under a non-UI / non-canvas entity (`ChildOf` changed to a parent
/// whose chain has no canvas).
fn heal_orphaned_ui_widgets(
    mut commands: Commands,
    roots: Query<Entity, (With<UiWidget>, Without<ChildOf>)>,
    moved: Query<Entity, (With<UiWidget>, Changed<ChildOf>)>,
    child_of: Query<&ChildOf>,
    canvases: Query<(), With<UiCanvas>>,
    world_roots: Query<(), With<world_panel::WorldUiRoot>>,
) {
    // A widget is in-scope if it reaches either a `UiCanvas` OR a
    // `WorldUiRoot` — the latter is a world panel's camera-routed subtree, a
    // legitimate scope with no canvas above it. Missing the world-root case is
    // what made a panel spawn a stray `UiCanvas` around its built markup.
    let has_ui_scope_ancestor = |start: Entity| -> bool {
        let mut e = start;
        loop {
            if canvases.get(e).is_ok() || world_roots.get(e).is_ok() {
                return true;
            }
            match child_of.get(e) {
                Ok(c) => e = c.parent(),
                Err(_) => return false,
            }
        }
    };

    // Root widgets are always orphans; reparented widgets only when their new
    // chain reaches no scope. Dedup so a widget that is both root and freshly
    // changed isn't healed twice.
    let mut orphans: Vec<Entity> = roots.iter().collect();
    for e in &moved {
        if !has_ui_scope_ancestor(e) {
            orphans.push(e);
        }
    }
    orphans.sort();
    orphans.dedup();

    for widget in orphans {
        commands.queue(move |world: &mut World| {
            // Re-check against the live world: a despawn or an earlier heal this
            // frame may have already resolved (or removed) the widget.
            if world.get::<UiWidget>(widget).is_none() {
                return;
            }
            if crate::game_ui::spawn::find_ancestor_canvas(world, widget).is_some()
                || crate::game_ui::spawn::is_within_world_ui_root(world, widget)
            {
                return;
            }
            let canvas = crate::game_ui::spawn::spawn_root_canvas(world);
            world.entity_mut(widget).set_parent_in_place(canvas);
        });
    }
}

/// Pop any `UiCanvas` that got nested under a `UiWidget` back to the scene root.
/// Widgets belong to canvases, never the reverse; a canvas dragged onto a widget
/// inverts the model and re-scopes the canvas inside the widget's layout.
fn heal_nested_ui_canvases(
    mut commands: Commands,
    moved: Query<Entity, (With<UiCanvas>, Changed<ChildOf>)>,
    child_of: Query<&ChildOf>,
    widgets: Query<(), With<UiWidget>>,
) {
    for canvas in &moved {
        let mut cur = canvas;
        let mut nested_under_widget = false;
        while let Ok(c) = child_of.get(cur) {
            cur = c.parent();
            if widgets.get(cur).is_ok() {
                nested_under_widget = true;
                break;
            }
        }
        if nested_under_widget {
            commands.queue(move |world: &mut World| {
                if let Ok(mut em) = world.get_entity_mut(canvas) {
                    em.remove_parent_in_place();
                }
            });
        }
    }
}

/// Force every canvas root back to full-target geometry.
///
/// The canvas root's rect is not authorable — see [`canvas_root_node`] — but it
/// is an ordinary `Node`, so every tool that writes a `Node` could reach it. The
/// UI editor's move/resize did: the canvas root carries `UiWidget` once its
/// template has built, which put it in the canvas editor's hit-test alongside the
/// markup nodes it contains, and one drag rewrote it to
/// `left: 107.8%, top: 29.2%, width: 0%, height: 0%`.
///
/// A canvas at zero size is not a visible mistake. It has no border and no
/// background, so there is nothing on screen to look wrong — the template
/// underneath still loads, still builds its children, still reports a sensible
/// `content_size`, and simply lays all of it out inside a zero-by-zero box. The
/// UI editor renders an empty black rectangle and the file looks broken. That is
/// the whole reason this is an invariant re-established every frame rather than a
/// guard at the one call site that happened to violate it: an error you cannot
/// see is one you cannot be trusted to avoid.
///
/// Writing only on a mismatch matters — an unconditional assignment would dirty
/// `Node` every frame and re-run layout for the whole UI tree.
fn heal_canvas_root_geometry(mut canvases: Query<&mut Node, With<UiCanvas>>) {
    let want = components::canvas_root_node();
    for mut node in &mut canvases {
        if node.position_type != want.position_type
            || node.left != want.left
            || node.top != want.top
            || node.width != want.width
            || node.height != want.height
        {
            node.position_type = want.position_type;
            node.left = want.left;
            node.top = want.top;
            node.width = want.width;
            node.height = want.height;
        }
    }
}

// ── Canvas scaler ───────────────────────────────────────────────────────────

/// Scales `Val::Px` values (text size, padding, border-radius) uniformly so
/// they stay proportional to the viewport.
fn update_ui_scale(
    canvases: Query<&UiCanvas>,
    render_target: Option<Res<renzora::ViewportRenderTarget>>,
    images: Res<Assets<Image>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut ui_scale: ResMut<bevy::ui::UiScale>,
) {
    let (ref_w, ref_h) = canvases
        .iter()
        .next()
        .map(|c| (c.reference_width, c.reference_height))
        .unwrap_or((1280.0, 720.0));

    if ref_w <= 0.0 || ref_h <= 0.0 {
        return;
    }

    let actual = render_target
        .as_ref()
        .and_then(|rt| rt.image.as_ref())
        .and_then(|h| images.get(h))
        .map(|img| {
            let s = img.size();
            (s.x as f32, s.y as f32)
        });

    let (actual_w, actual_h) = match actual {
        Some(size) => size,
        None => {
            if let Ok(window) = windows.single() {
                (window.width(), window.height())
            } else {
                return;
            }
        }
    };

    if actual_w <= 0.0 || actual_h <= 0.0 {
        return;
    }

    let scale = (actual_w / ref_w).min(actual_h / ref_h);
    // A mutable dereference marks this global layout input changed. Keep the
    // inexpensive size check, but invalidate layout only when its result differs.
    if ui_scale.0 != scale {
        ui_scale.0 = scale;
    }
}


// ── Image rehydration ───────────────────────────────────────────────────────

/// Rehydrates `ImageNode` for UI image widgets after scene deserialization.
///
/// `ImageNode` contains a `Handle<Image>` which fails serialization and gets
/// stripped on save. `UiImagePath` stores the asset-relative path and survives.
/// This system re-loads the image and inserts `ImageNode` on any entity that
/// has `UiImagePath` but no `ImageNode`.
fn rehydrate_ui_images(
    mut commands: Commands,
    query: Query<
        (Entity, &components::UiImagePath),
        (Without<ImageNode>, Added<components::UiImagePath>),
    >,
    asset_server: Res<AssetServer>,
) {
    for (entity, img_path) in &query {
        let path = img_path.path.clone();
        let handle: Handle<Image> = asset_server.load(path);
        commands.entity(entity).try_insert(ImageNode::new(handle));
    }
}

// ── Z-index sync ────────────────────────────────────────────────────────────

#[derive(bevy::ecs::system::SystemParam)]
struct UiOrderChanges<'w, 's> {
    children: Query<'w, 's, Entity, Changed<Children>>,
    items: Query<
        'w,
        's,
        Entity,
        (
            Or<(With<UiCanvas>, With<UiWidget>)>,
            Or<(
                Added<UiCanvas>,
                Added<UiWidget>,
                Changed<ChildOf>,
                Changed<ZIndex>,
            )>,
        ),
    >,
    removed_canvases: RemovedComponents<'w, 's, UiCanvas>,
    removed_widgets: RemovedComponents<'w, 's, UiWidget>,
    removed_zindex: RemovedComponents<'w, 's, ZIndex>,
    disabled: Query<'w, 's, Entity, (Added<Disabled>, Allow<Disabled>)>,
    removed_disabled: RemovedComponents<'w, 's, Disabled>,
}

struct UiOrderPolicy {
    custom_disabling: bool,
}

impl FromWorld for UiOrderPolicy {
    fn from_world(world: &mut World) -> Self {
        let standard = world.register_component::<Disabled>();
        Self {
            custom_disabling: world
                .get_resource::<DefaultQueryFilters>()
                .is_some_and(|filters| filters.disabling_ids().any(|id| id != standard)),
        }
    }
}

impl UiOrderChanges<'_, '_> {
    fn collect_parents(
        &mut self,
        hierarchy: &Query<&ChildOf, Allow<Disabled>>,
        parents: &mut std::collections::HashSet<Entity>,
    ) {
        parents.clear();
        parents.extend(self.children.iter());
        for entity in self
            .items
            .iter()
            .chain(self.removed_canvases.read())
            .chain(self.removed_widgets.read())
            .chain(self.removed_zindex.read())
        {
            if let Ok(parent) = hierarchy.get(entity) {
                parents.insert(parent.parent());
            }
        }
        for entity in self.disabled.iter().chain(self.removed_disabled.read()) {
            // Enabling a parent affects its group; disabling a child affects
            // the remaining siblings even though ChildOf itself did not change.
            parents.insert(entity);
            if let Ok(parent) = hierarchy.get(entity) {
                parents.insert(parent.parent());
            }
        }
    }
}

/// Syncs `ZIndex` on UI canvas and widget entities so that items higher in the
/// hierarchy (top of the list) render on top — matching the layer order convention
/// used by most editors (Photoshop, Unity, etc.).
fn sync_ui_zindex(
    canvas_entities: Query<Entity, With<UiCanvas>>,
    canvas_data: Query<(&UiCanvas, Option<&GlobalZIndex>)>,
    widgets: Query<Entity, With<UiWidget>>,
    zindex_query: Query<Option<&ZIndex>>,
    children_query: Query<&Children>,
    child_of_query: Query<&ChildOf, Allow<Disabled>>,
    mut commands: Commands,
    mut changes: UiOrderChanges,
    mut processed_parents: Local<std::collections::HashSet<Entity>>,
    policy: Local<UiOrderPolicy>,
) {
    // Relationships already tell us which sibling groups changed. Marker and
    // output changes cover edits that do not alter the parent's child list.
    changes.collect_parents(&child_of_query, &mut processed_parents);
    if policy.custom_disabling {
        // Third-party disabling types are unknown to the typed change readers.
        // Keep the original eligibility scan for that configuration, rather
        // than silently leaving stale ordering after a custom marker changes.
        for entity in canvas_entities.iter().chain(widgets.iter()) {
            if let Ok(parent) = child_of_query.get(entity) {
                processed_parents.insert(parent.parent());
            }
        }
    }
    for &parent in processed_parents.iter() {
        let Ok(children) = children_query.get(parent) else {
            continue;
        };

        // Count only UI entities among siblings for correct reverse indexing.
        let ui_count = children
            .iter()
            .filter(|c| canvas_entities.contains(*c) || widgets.contains(*c))
            .count() as i32;

        let mut ui_idx = 0i32;
        for child in children.iter() {
            if canvas_entities.contains(child) || widgets.contains(child) {
                // First child (top of hierarchy) gets highest ZIndex → renders on top.
                let desired = ZIndex(ui_count - 1 - ui_idx);
                let current = zindex_query.get(child).ok().flatten().copied();
                if current != Some(desired) {
                    commands.entity(child).try_insert(desired);
                }
                ui_idx += 1;
            }
        }
    }

    // Root-level canvases (no parent) use GlobalZIndex from sort_order.
    for entity in &canvas_entities {
        if child_of_query.contains(entity) {
            continue;
        }
        if let Ok((canvas, current_gz)) = canvas_data.get(entity) {
            let desired = GlobalZIndex(canvas.sort_order);
            if current_gz.copied() != Some(desired) {
                commands.entity(entity).try_insert(desired);
            }
        }
    }
}
renzora::add!(GameUiPlugin);

#[cfg(test)]
mod invariant_tests {
    #[derive(Resource, Default)]
    struct ScaleChanges(u32);

    fn count_changes(scale: Res<bevy::ui::UiScale>, mut changes: ResMut<ScaleChanges>) {
        if scale.is_changed() {
            changes.0 += 1;
        }
    }

    #[test]
    fn stable_frames_do_not_invalidate_scale_but_resize_and_canvas_changes_do() {
        let mut app = App::new();
        app.init_resource::<Assets<Image>>()
            .init_resource::<bevy::ui::UiScale>()
            .init_resource::<ScaleChanges>()
            .add_systems(Update, (update_ui_scale, count_changes).chain());
        let window = app.world_mut().spawn((Window {
            resolution: bevy::window::WindowResolution::new(1280, 720),
            ..default()
        }, bevy::window::PrimaryWindow)).id();
        app.update();
        let initial = app.world().resource::<ScaleChanges>().0;
        for _ in 0..1000 {
            app.update();
        }
        assert_eq!(app.world().resource::<ScaleChanges>().0, initial);

        app.world_mut().get_mut::<Window>(window).unwrap().resolution.set(2560., 1440.);
        app.update();
        assert_eq!(app.world().resource::<bevy::ui::UiScale>().0, 2.0);
        assert_eq!(app.world().resource::<ScaleChanges>().0, initial + 1);

        let canvas = app.world_mut().spawn(UiCanvas {
            reference_width: 2560., reference_height: 1440., ..default()
        }).id();
        app.update();
        assert_eq!(app.world().resource::<bevy::ui::UiScale>().0, 1.0);
        app.world_mut().despawn(canvas);
        app.update();
        assert_eq!(app.world().resource::<bevy::ui::UiScale>().0, 2.0);
        assert_eq!(app.world().resource::<ScaleChanges>().0, initial + 3);
        eprintln!("UiScale measurement: 1000 stable frames, 0 scale-change notifications; 3 size changes, 3 notifications");
    }
    use super::*;
    use crate::game_ui::components::UiCanvas;

    /// Drive the two healers over a bare world once. `Schedule::run` applies the
    /// systems' deferred commands at the end, so a single call is enough to both
    /// detect the violation and repair it.
    fn run_healers(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems((heal_orphaned_ui_widgets, heal_nested_ui_canvases));
        schedule.run(world);
    }

    /// A widget dragged to the scene root (no parent) must be re-homed under a
    /// freshly spawned canvas — otherwise it renders into the editor's own UI.
    #[test]
    fn orphan_widget_at_root_is_wrapped_in_a_canvas() {
        let mut world = World::new();
        let widget = world.spawn((UiWidget::default(), Node::default())).id();

        run_healers(&mut world);

        let parent = world
            .get::<ChildOf>(widget)
            .map(|c| c.parent())
            .expect("orphaned widget must be reparented");
        assert!(
            world.get::<UiCanvas>(parent).is_some(),
            "the widget's new parent must be a UiCanvas"
        );
    }

    /// A widget reparented under a non-UI entity (a parent whose chain reaches
    /// no canvas) is just as orphaned as one at the root, and must be healed.
    #[test]
    fn widget_under_a_non_canvas_parent_is_rehomed() {
        let mut world = World::new();
        let empty = world.spawn(Name::new("Empty")).id();
        let widget = world
            .spawn((UiWidget::default(), Node::default(), ChildOf(empty)))
            .id();

        run_healers(&mut world);

        let parent = world
            .get::<ChildOf>(widget)
            .map(|c| c.parent())
            .expect("widget must have a parent");
        assert_ne!(parent, empty, "widget must be moved off the non-UI parent");
        assert!(
            world.get::<UiCanvas>(parent).is_some(),
            "widget must land under a UiCanvas"
        );
    }

    /// A widget already correctly under a canvas must be left untouched — the
    /// healer must not spawn spurious canvases for valid hierarchies.
    #[test]
    fn widget_already_under_a_canvas_is_left_alone() {
        let mut world = World::new();
        let canvas = world.spawn((UiCanvas::default(), Node::default())).id();
        let widget = world
            .spawn((UiWidget::default(), Node::default(), ChildOf(canvas)))
            .id();

        run_healers(&mut world);

        assert_eq!(
            world.get::<ChildOf>(widget).map(|c| c.parent()),
            Some(canvas),
            "a validly parented widget must not move"
        );
        let canvas_count = world
            .query_filtered::<Entity, With<UiCanvas>>()
            .iter(&world)
            .count();
        assert_eq!(canvas_count, 1, "no extra canvas should be spawned");
    }

    /// A canvas dragged so it becomes a child of a widget inverts the model
    /// (widgets belong to canvases, not the reverse) and must be popped back to
    /// the scene root, while the widget keeps its own canvas parent.
    #[test]
    fn canvas_nested_under_a_widget_is_popped_to_root() {
        let mut world = World::new();
        let root_canvas = world.spawn((UiCanvas::default(), Node::default())).id();
        let widget = world
            .spawn((UiWidget::default(), Node::default(), ChildOf(root_canvas)))
            .id();
        let nested_canvas = world
            .spawn((UiCanvas::default(), Node::default(), ChildOf(widget)))
            .id();

        run_healers(&mut world);

        assert!(
            world.get::<ChildOf>(nested_canvas).is_none(),
            "a canvas nested under a widget must be popped to the root"
        );
        assert_eq!(
            world.get::<ChildOf>(widget).map(|c| c.parent()),
            Some(root_canvas),
            "the widget must keep its own canvas parent"
        );
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    #[derive(Resource, Default)]
    struct ParentWork {
        parents: std::collections::HashSet<Entity>,
        total: usize,
    }

    fn count_parents(
        mut changes: UiOrderChanges,
        hierarchy: Query<&ChildOf, Allow<Disabled>>,
        mut work: ResMut<ParentWork>,
    ) {
        changes.collect_parents(&hierarchy, &mut work.parents);
        work.total += work.parents.len();
    }

    #[test]
    fn settled_order_skips_sibling_work_and_lifecycle_changes_reorder() {
        let mut app = App::new();
        app.init_resource::<ParentWork>()
            .add_systems(Update, (count_parents, sync_ui_zindex).chain());
        let first = app
            .world_mut()
            .spawn(UiCanvas {
                sort_order: 7,
                ..default()
            })
            .id();
        let second = app.world_mut().spawn(UiCanvas::default()).id();
        let a = app
            .world_mut()
            .spawn((UiWidget::default(), ChildOf(first)))
            .id();
        let decoration = app.world_mut().spawn(ChildOf(first)).id();
        let b = app
            .world_mut()
            .spawn((UiWidget::default(), ChildOf(first)))
            .id();
        let c = app
            .world_mut()
            .spawn((UiWidget::default(), ChildOf(second)))
            .id();
        for _ in 0..3 {
            app.update();
        }
        let initial = app.world().resource::<ParentWork>().total;
        for _ in 0..1_000 {
            app.update();
        }
        assert_eq!(app.world().resource::<ParentWork>().total, initial);
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(1)));
        assert_eq!(app.world().get::<ZIndex>(b), Some(&ZIndex(0)));
        assert_eq!(
            app.world().get::<GlobalZIndex>(first),
            Some(&GlobalZIndex(7))
        );
        app.world_mut().entity_mut(b).insert(Disabled);
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        app.world_mut().entity_mut(b).remove::<Disabled>();
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(1)));
        app.world_mut().entity_mut(first).insert(Disabled);
        app.world_mut().entity_mut(a).insert(ZIndex(99));
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(99)));
        app.world_mut().entity_mut(first).remove::<Disabled>();
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(1)));
        app.world_mut()
            .get_mut::<Children>(first)
            .unwrap()
            .swap(0, 2);
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        assert_eq!(app.world().get::<ZIndex>(b), Some(&ZIndex(1)));
        app.world_mut().entity_mut(b).insert(ChildOf(second));
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        assert_eq!(app.world().get::<ZIndex>(c), Some(&ZIndex(1)));
        assert_eq!(app.world().get::<ZIndex>(b), Some(&ZIndex(0)));
        app.world_mut()
            .entity_mut(decoration)
            .insert(UiWidget::default());
        app.update();
        assert_eq!(app.world().get::<ZIndex>(decoration), Some(&ZIndex(1)));
        app.world_mut().entity_mut(b).remove::<UiWidget>();
        app.update();
        assert_eq!(app.world().get::<ZIndex>(c), Some(&ZIndex(0)));
        app.world_mut().entity_mut(a).insert(ZIndex(99));
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        app.world_mut().entity_mut(a).remove::<ZIndex>();
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        app.world_mut().despawn(a);
        app.update();
        assert_eq!(app.world().get::<ZIndex>(decoration), Some(&ZIndex(0)));
        app.world_mut()
            .get_mut::<UiCanvas>(first)
            .unwrap()
            .sort_order = 8;
        app.update();
        assert_eq!(
            app.world().get::<GlobalZIndex>(first),
            Some(&GlobalZIndex(8))
        );
    }

    #[derive(Component)]
    struct CustomHidden;

    #[test]
    fn custom_disabling_keeps_original_sibling_eligibility() {
        let mut app = App::new();
        let marker = app.world_mut().register_component::<CustomHidden>();
        app.world_mut()
            .resource_mut::<DefaultQueryFilters>()
            .register_disabling_component(marker);
        app.add_systems(Update, sync_ui_zindex);
        let parent = app.world_mut().spawn_empty().id();
        let a = app
            .world_mut()
            .spawn((UiWidget::default(), ChildOf(parent)))
            .id();
        let b = app
            .world_mut()
            .spawn((UiWidget::default(), ChildOf(parent)))
            .id();
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(1)));
        app.world_mut().entity_mut(b).insert(CustomHidden);
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(0)));
        app.world_mut().entity_mut(b).remove::<CustomHidden>();
        app.update();
        assert_eq!(app.world().get::<ZIndex>(a), Some(&ZIndex(1)));
    }
}
