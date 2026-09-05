//! Editor-only pool water controls using shared scene data.

use bevy::prelude::*;
use renzora::pool_water::PoolWater;

mod inspector;

/// Install pool water inspector controls in the editor only.
#[derive(Default)]
pub struct PoolWaterEditorPlugin;

impl Plugin for PoolWaterEditorPlugin {
    fn build(&self, app: &mut App) {
        if renzora::builtin_plugin_enabled(app, "pool_water") {
            inspector::register(app);
        }
    }
}

renzora::add!(PoolWaterEditorPlugin, Editor);
