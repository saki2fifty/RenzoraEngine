//! World-space text using flat SDF quads or extruded glyph geometry.

use bevy::prelude::*;
pub use renzora::text3d::Text3d;
pub use renzora::text_mesh::build_text_mesh;

pub mod outline;
pub mod systems;

pub(crate) const DEFAULT_FONT: &[u8] = include_bytes!("embedded/NotoSans-Regular.ttf");

#[derive(Resource)]
pub(crate) struct DefaultFont(pub Handle<bevy::text::Font>);

fn register_default_font(mut commands: Commands, mut fonts: ResMut<Assets<bevy::text::Font>>) {
    let handle = fonts.add(bevy::text::Font::from_bytes(DEFAULT_FONT.to_vec()));
    commands.insert_resource(DefaultFont(handle));
}

/// Install both 3D-text rendering modes without inspector dependencies.
#[derive(Default)]
pub struct Text3dPlugin;

impl Plugin for Text3dPlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "text3d") {
            return;
        }
        renzora::text_mesh::ensure_sdf_material(app);
        app.add_systems(Startup, register_default_font);
        app.add_systems(Update, (systems::rebuild_text3d, systems::cleanup_text3d));
        app.register_type::<Text3d>();
    }
}

renzora::add!(Text3dPlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::AssetPlugin;
    use bevy::render::sync_world::SyncWorldPlugin;
    use bevy::text::TextPlugin;
    use renzora::text_mesh::SdfTextMaterial;

    #[test]
    fn disabled_startup_does_not_install_rendering_or_font_assets() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["text3d".into()]));
        app.add_plugins(Text3dPlugin);
        assert!(!app.is_plugin_added::<MaterialPlugin<SdfTextMaterial>>());
        app.update();
        assert!(!app.world().contains_resource::<DefaultFont>());
    }

    #[test]
    fn embedded_font_renders_both_modes_and_removes_generated_geometry() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            SyncWorldPlugin,
            TextPlugin,
        ));
        app.init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(Text3dPlugin);
        let entity = app.world_mut().spawn(Text3d::default()).id();
        for _ in 0..4 {
            app.update();
        }
        assert!(app.world().get::<Mesh3d>(entity).is_some());
        assert!(app
            .world()
            .get::<MeshMaterial3d<SdfTextMaterial>>(entity)
            .is_some());
        app.world_mut().get_mut::<Text3d>(entity).unwrap().mode = "mesh".into();
        app.update();
        assert!(app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(entity)
            .is_some());
        assert!(app
            .world()
            .get::<MeshMaterial3d<SdfTextMaterial>>(entity)
            .is_none());
        let handle = &app.world().get::<Mesh3d>(entity).unwrap().0;
        let mesh = app.world().resource::<Assets<Mesh>>().get(handle).unwrap();
        assert!(mesh.count_vertices() > 0);
        app.world_mut().entity_mut(entity).remove::<Text3d>();
        app.update();
        assert!(app.world().get::<Mesh3d>(entity).is_none());
        assert!(app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(entity)
            .is_none());
    }
}
