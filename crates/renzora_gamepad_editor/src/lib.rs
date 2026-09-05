//! Gamepad debug panel — visualizes controller input (sticks, triggers, buttons).

pub mod native;
mod state;

use bevy::prelude::*;

use state::{update_gamepad_debug_state, GamepadDebugState};

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Plugin that registers the gamepad debug panel and its update system.
#[derive(Default)]
pub struct GamepadEditorPlugin;

impl Plugin for GamepadEditorPlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "gamepad") {
            return;
        }
        info!("[editor] GamepadEditorPlugin");
        app.init_resource::<GamepadDebugState>();
        use renzora::SplashState;
        app.add_systems(
            Update,
            (update_gamepad_debug_state, hide_gamepad_entities)
                .run_if(in_state(SplashState::Editor)),
        );
        // Bevy-native (ember) gamepad panel for the bevy_ui shell.
        native::register_native_gamepad(app);
    }
}

/// Bevy spawns one entity (with a `Name`) per connected gamepad, which would
/// otherwise appear as a loose entity in the scene hierarchy — and get picked up
/// by the scene saver. Tag each with `HideInHierarchy` so it's treated as
/// editor-internal, the same as the VR controller wands. Runs continuously so a
/// pad plugged in mid-session is caught; `Without` makes it a no-op once tagged.
fn hide_gamepad_entities(
    mut commands: Commands,
    pads: Query<
        Entity,
        (
            With<bevy::input::gamepad::Gamepad>,
            Without<renzora::HideInHierarchy>,
        ),
    >,
) {
    for e in &pads {
        commands.entity(e).try_insert(renzora::HideInHierarchy);
    }
}

renzora::add!(GamepadEditorPlugin, Editor);

#[cfg(test)]
mod migration_tests {
    use super::*;
    use bevy::{asset::AssetApp, shader::Shader, ui_render::prelude::UiMaterial};

    #[test]
    fn registers_existing_panel_and_resolves_the_embedded_stick_shader() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()));
        app.init_asset::<Shader>();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(GamepadEditorPlugin);
        let panels = app.world().resource::<renzora::ShellPanelRegistry>();
        assert_eq!(panels.panels["gamepad"].title, "Gamepad");
        assert_eq!(panels.panels["gamepad"].category, "Input");
        assert!(app.world().contains_resource::<GamepadDebugState>());
        let bevy::shader::ShaderRef::Path(path) = native::StickMaterial::fragment_shader() else {
            panic!("stick material must use an embedded shader path");
        };
        let server = app.world().resource::<AssetServer>();
        let source = server.get_source(path.source()).expect("embedded source");
        let mut reader = bevy::tasks::block_on(source.reader().read(path.path()))
            .expect("actual embedded shader resolves");
        let mut bytes = Vec::new();
        bevy::tasks::block_on(reader.read_to_end(&mut bytes)).expect("read shader");
        assert_eq!(bytes, include_bytes!("stick.wgsl"));
    }

    #[test]
    fn disabling_gamepad_does_not_register_the_panel_or_state() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["gamepad".into()]));
        app.add_plugins(GamepadEditorPlugin);
        assert!(!app.world().contains_resource::<GamepadDebugState>());
        assert!(!app
            .world()
            .contains_resource::<renzora::ShellPanelRegistry>());
        assert_eq!(
            app.world().resource::<renzora::PluginInventory>().entries[0].state,
            renzora::PluginState::Disabled
        );
    }
}
