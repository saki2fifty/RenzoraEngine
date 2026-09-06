//! Shape Library — editor browser for shapes registered by the engine.
//!
//! [`ShapeLibraryPlugin`] adds icons and the browsing/spawning panel. Mesh
//! generators are shared with the runtime through `renzora_mesh_primitives`;
//! [`procedural_meshes`] preserves the original import path.

pub mod procedural_meshes;

use bevy::prelude::*;
use renzora::core::ShapeRegistry;

// ============================================================================
// Built-in shape icons
// ============================================================================

/// Add icons to shapes already registered by the engine (editor only).
fn add_shape_icons(registry: &mut ShapeRegistry) {
    // Phosphor icon names (kebab-case), resolved to glyphs by the native panel.
    let icons: &[(&str, &str)] = &[
        ("cube", "cube"),
        ("sphere", "globe"),
        ("cylinder", "cylinder"),
        ("plane", "square"),
        ("cone", "triangle"),
        ("torus", "circle"),
        ("capsule", "cylinder"),
        ("hemisphere", "globe"),
        ("wedge", "triangle"),
        ("stairs", "stairs"),
        ("arch", "circle"),
        ("half_cylinder", "cylinder"),
        ("quarter_pipe", "polygon"),
        ("corner", "polygon"),
        ("wall", "wall"),
        ("ramp", "triangle"),
        ("curved_wall", "wall"),
        ("doorway", "door"),
        ("window_wall", "frame-corners"),
        ("l_shape", "polygon"),
        ("t_shape", "polygon"),
        ("cross_shape", "plus"),
        ("spiral_stairs", "spiral"),
        ("pillar", "columns"),
        ("pipe", "pipe"),
        ("ring", "circle"),
        ("funnel", "triangle"),
        ("gutter", "cylinder"),
        ("prism", "hexagon"),
        ("pyramid", "diamond"),
    ];
    for (id, icon) in icons {
        if let Some(entry) = registry.get_mut(id) {
            entry.icon = icon;
        }
    }
}

mod native;

/// Adds icons to registered shapes and installs the shape browser panel.
#[derive(Default)]
pub struct ShapeLibraryPlugin;

impl Plugin for ShapeLibraryPlugin {
    fn build(&self, app: &mut App) {
        info!("[editor] ShapeLibraryPlugin");

        // Add icons to the shapes already registered by the engine
        add_shape_icons(&mut app.world_mut().resource_mut::<ShapeRegistry>());

        app.add_plugins(native::NativeShapeLibrary);
    }
}

renzora::add!(ShapeLibraryPlugin, Editor);
