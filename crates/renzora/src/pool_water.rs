//! Shared authored pool water settings.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Interactive water attached to a pool container mesh.
#[derive(Component, Clone, Debug, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default)]
#[type_path = "pool_water"]
pub struct PoolWater {
    /// Distance below the top face.
    pub water_level: f32,
    /// Index of refraction.
    pub ior: f32,
    /// Minimum Fresnel reflectance.
    pub fresnel_min: f32,
    /// Caustic brightness multiplier.
    pub caustic_intensity: f32,
    /// Deep water absorption color.
    pub deep_color: [f32; 3],
    /// Shallow water tint.
    pub shallow_color: [f32; 3],
    /// Foam color.
    pub foam_color: [f32; 3],
    /// Simulation damping; higher values preserve ripples longer.
    pub damping: f32,
    /// Wave propagation speed.
    pub wave_speed: f32,
    /// Simulation height to world-unit scale.
    pub height_scale: f32,
    /// Square simulation texture resolution.
    pub sim_resolution: u32,
    /// Water mesh subdivisions.
    pub mesh_subdivisions: u32,
    /// Sun specular power.
    pub specular_power: f32,
    /// Refraction UV distortion strength.
    pub refraction_strength: f32,
    /// Maximum absorption depth in world units.
    pub max_depth: f32,
    /// Red absorption coefficient.
    pub absorption_r: f32,
    /// Green absorption coefficient.
    pub absorption_g: f32,
    /// Blue absorption coefficient.
    pub absorption_b: f32,
    /// Shoreline foam depth threshold.
    pub foam_depth: f32,
}

impl Default for PoolWater {
    fn default() -> Self {
        Self {
            water_level: 0.05,
            ior: 1.333,
            fresnel_min: 0.02,
            caustic_intensity: 0.25,
            deep_color: [0.005, 0.02, 0.08],
            shallow_color: [0.04, 0.22, 0.28],
            foam_color: [0.9, 0.92, 0.95],
            damping: 0.995,
            wave_speed: 2.0,
            height_scale: 0.3,
            sim_resolution: 256,
            mesh_subdivisions: 200,
            specular_power: 5000.0,
            refraction_strength: 0.03,
            max_depth: 5.0,
            absorption_r: 3.0,
            absorption_g: 1.0,
            absorption_b: 0.4,
            foam_depth: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_scene_name_and_authored_values() {
        assert_eq!(PoolWater::type_path(), "pool_water::PoolWater");
        let authored = PoolWater {
            water_level: 0.2,
            damping: 0.97,
            ..default()
        };
        let reflected = PoolWater::from_reflect(&authored).unwrap();
        assert_eq!(reflected.water_level, 0.2);
        assert_eq!(reflected.damping, 0.97);
        let encoded = serde_json::to_value(&authored).unwrap();
        assert_eq!(encoded.as_object().unwrap().len(), 19);
        let restored: PoolWater = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.sim_resolution, 256);
        assert_eq!(restored.water_level, authored.water_level);
    }
}
