//! Shared authored 3D-text data for rendering and inspector controls.

use bevy::prelude::*;

/// Text rendered as flat SDF quads or extruded glyph geometry.
#[derive(Component, Reflect, Clone)]
#[reflect(Component, Default)]
#[type_path = "text3d"]
pub struct Text3d {
    /// The text to display.
    pub text: String,
    /// Asset-relative font path; empty selects the embedded fallback.
    pub font: String,
    /// `flat` or `mesh`.
    pub mode: String,
    /// Glyph rasterization / em size in pixels.
    pub size: f32,
    /// Extrusion depth in world units, used in mesh mode.
    pub depth: f32,
    /// sRGB text color.
    pub color: [f32; 3],
}

impl Default for Text3d {
    fn default() -> Self {
        Self {
            text: "3D Text".into(),
            font: String::new(),
            mode: "flat".into(),
            size: 100.0,
            depth: 0.1,
            color: [1.0, 1.0, 1.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_reflected_scene_identity_and_values() {
        assert_eq!(Text3d::type_path(), "text3d::Text3d");
        let source = Text3d {
            text: "Saved text".into(),
            mode: "mesh".into(),
            depth: 0.3,
            ..default()
        };
        let restored = Text3d::from_reflect(&source).unwrap();
        assert_eq!(restored.text, source.text);
        assert_eq!(restored.mode, "mesh");
        assert_eq!(restored.depth, 0.3);
    }
}
