//! Authored vignette settings shared by runtime rendering and editor controls.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Authored vignette settings, routed to cameras as a Bevy vignette.
#[derive(Component, Clone, Debug, Reflect, Serialize, Deserialize)]
#[reflect(Component, Serialize, Deserialize)]
// Existing scenes use this name; moving the definition must not rename data.
#[type_path = "vignette"]
pub struct VignetteSettings {
    pub enabled: bool,
    /// Strength of the darkening at the edges.
    pub intensity: f32,
    /// Radius at which the vignette starts.
    pub radius: f32,
    /// Falloff softness from the radius to the corners.
    pub smoothness: f32,
    /// Zero follows the aspect ratio; one is circular.
    pub roundness: f32,
    /// Vignette tint.
    pub color: Vec3,
    /// Compensates the darkening in the very corners.
    pub edge_compensation: f32,
}

impl Default for VignetteSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            intensity: 1.0,
            radius: 0.75,
            smoothness: 5.0,
            roundness: 1.0,
            color: Vec3::ZERO,
            edge_compensation: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_the_original_scene_type_name() {
        assert_eq!(VignetteSettings::type_path(), "vignette::VignetteSettings");
        let settings = VignetteSettings::default();
        let restored = VignetteSettings::from_reflect(&settings).expect("reflected settings");
        assert_eq!(restored.radius, 0.75);
        assert_eq!(restored.intensity, 1.0);
    }

    #[test]
    fn reads_pre_migration_authored_fields() {
        let settings: VignetteSettings = serde_json::from_str(
            r#"{"enabled":false,"intensity":2.5,"radius":0.4,"smoothness":3.0,"roundness":0.2,"color":[0.1,0.2,0.3],"edge_compensation":0.8}"#,
        ).expect("existing settings representation");
        assert!(!settings.enabled);
        assert_eq!(settings.intensity, 2.5);
        assert_eq!(settings.color, Vec3::new(0.1, 0.2, 0.3));
        assert_eq!(settings.edge_compensation, 0.8);
    }
}
