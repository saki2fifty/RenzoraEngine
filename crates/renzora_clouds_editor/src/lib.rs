//! Editor-only cloud controls using the shared sky settings.

use bevy::prelude::*;

mod inspector;

/// Install the cloud inspector without the renderer.
#[derive(Default)]
pub struct CloudsEditorPlugin;

impl Plugin for CloudsEditorPlugin {
    fn build(&self, app: &mut App) {
        if renzora::builtin_plugin_enabled(app, "clouds") {
            inspector::register(app);
        }
    }
}

renzora::add!(CloudsEditorPlugin, Editor);
