//! Splines — control-point paths with Catmull-Rom evaluation.
//!
//! The interesting thing about this plugin is how little is in it. `SplinePath`
//! and its curve maths live in the **contract crate** (`renzora::spline`), and
//! all that remains here is registering the type for reflection.
//!
//! That split is deliberate and worth more than it looks. A spline is not a
//! feature so much as a *shape other things read*: a road builder, a camera
//! rail, a fence generator, a patrol path, a particle emitter track. Every one
//! of those is a plausible separate plugin, and they can only cooperate if they
//! agree on one `SplinePath` — one `TypeId`, one scene representation, one
//! definition of where `t = 1.7` falls on the curve.
//!
//! Had the type stayed here, each of those plugins would have had to define its
//! own, and none could read another's: a path authored by one would be invisible
//! to the next, and the editor's gizmo overlay — which draws control points and
//! the smooth curve for anything carrying a `SplinePath` — would only work for
//! whichever one happened to be linked. In the contract crate it is an
//! interchange format instead, and a plugin that spawns one gets the editing UI
//! for free.

use bevy::prelude::*;

/// Re-exported so this plugin reads as owning the concept even though the type
/// itself is shared, and so a dependent can name it without also naming the
/// contract crate.
pub use renzora::SplinePath;

#[derive(Default)]
/// Register the shared spline component in both editor and game worlds.
pub struct SplinePlugin;

impl Plugin for SplinePlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "spline") {
            return;
        }
        info!("[runtime] SplinePlugin");
        // The registration is the whole job: it is what lets a `SplinePath`
        // round-trip through a scene file and show up in the inspector.
        app.register_type::<SplinePath>();
    }
}

renzora::add!(SplinePlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;

    fn app_without_automatic_reflection() -> App {
        let app = App::new();
        // App startup auto-registers reflected types in this Bevy feature set.
        // Clear that baseline so this test observes the plugin's own work.
        *app.world().resource::<AppTypeRegistry>().write() = bevy::reflect::TypeRegistry::empty();
        app
    }

    #[test]
    fn registers_the_original_shared_type_and_preserves_curve_values() {
        let mut app = app_without_automatic_reflection();
        app.insert_resource(renzora::DisabledPlugins::default());
        app.add_plugins(SplinePlugin);
        let registry = app.world().resource::<AppTypeRegistry>().read();
        assert!(registry.get(TypeId::of::<renzora::SplinePath>()).is_some());
        let path = SplinePath::with_points([Vec3::ZERO, Vec3::X]);
        let restored = SplinePath::from_reflect(&path).expect("shared reflection value");
        assert_eq!(restored.control_points, path.control_points);
        assert_eq!(restored.sample(0.5), Vec3::X * 0.5);
        let row = &app.world().resource::<renzora::PluginInventory>().entries[0];
        assert_eq!(row.id, "spline");
        assert_eq!(row.kind, renzora::PluginKind::Builtin);
        assert_eq!(row.state, renzora::PluginState::Loaded);
    }

    #[test]
    fn existing_disable_preference_prevents_registration() {
        let mut app = app_without_automatic_reflection();
        app.insert_resource(renzora::DisabledPlugins(vec!["spline".into()]));
        app.add_plugins(SplinePlugin);
        let registry = app.world().resource::<AppTypeRegistry>().read();
        assert!(registry.get(TypeId::of::<SplinePath>()).is_none());
        assert_eq!(
            app.world().resource::<renzora::PluginInventory>().entries[0].state,
            renzora::PluginState::Disabled
        );
    }
}
