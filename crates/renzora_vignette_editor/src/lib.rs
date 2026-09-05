//! Inspector controls for the shared vignette component.

use bevy::{post_process::effect_stack::Vignette, prelude::*};
use renzora::{AppEditorExt, InspectorEntry, VignetteSettings};

fn inspector_entry() -> InspectorEntry {
    InspectorEntry {
        type_id: "vignette",
        display_name: "Vignette",
        icon: "aperture",
        category: "effects",
        has_fn: |world, entity| world.get::<VignetteSettings>(entity).is_some(),
        add_fn: Some(|world, entity| {
            world.entity_mut(entity).insert(VignetteSettings::default());
        }),
        remove_fn: Some(|world, entity| {
            world
                .entity_mut(entity)
                .remove::<(VignetteSettings, Vignette)>();
        }),
        is_enabled_fn: Some(|world, entity| {
            world
                .get::<VignetteSettings>(entity)
                .map(|s| s.enabled)
                .unwrap_or(false)
        }),
        set_enabled_fn: Some(|world, entity, val| {
            if let Some(mut s) = world.get_mut::<VignetteSettings>(entity) {
                s.enabled = val;
            }
        }),
        fields: vec![
            renzora::float_field!("Intensity", VignetteSettings, intensity, 0.01, 0.0, 5.0),
            renzora::float_field!("Radius", VignetteSettings, radius, 0.01, 0.0, 2.0),
            renzora::float_field!("Smoothness", VignetteSettings, smoothness, 0.05, 0.0, 20.0),
            renzora::float_field!("Roundness", VignetteSettings, roundness, 0.01, 0.0, 1.0),
            renzora::vec3_color_field!("Color", VignetteSettings, color),
            renzora::float_field!(
                "Edge Compensation",
                VignetteSettings,
                edge_compensation,
                0.01,
                0.0,
                2.0
            ),
        ],
    }
}

/// Register vignette inspector controls in the editor only.
#[derive(Default)]
pub struct VignetteEditorPlugin;

impl Plugin for VignetteEditorPlugin {
    fn build(&self, app: &mut App) {
        if renzora::builtin_plugin_enabled(app, "vignette") {
            app.register_inspector(inspector_entry());
        }
    }
}

renzora::add!(VignetteEditorPlugin, Editor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_registers_controls_only_when_enabled() {
        for enabled in [true, false] {
            let mut app = App::new();
            app.insert_resource(renzora::DisabledPlugins(if enabled {
                Vec::new()
            } else {
                vec!["vignette".into()]
            }));
            app.add_plugins(VignetteEditorPlugin);
            let registered = app
                .world()
                .get_resource::<renzora::InspectorRegistry>()
                .is_some_and(|registry| registry.iter().any(|entry| entry.type_id == "vignette"));
            assert_eq!(registered, enabled);
        }
    }

    #[test]
    fn inspector_callbacks_edit_the_shared_runtime_component() {
        let entry = inspector_entry();
        assert_eq!(entry.type_id, "vignette");
        assert_eq!(entry.fields.len(), 6);
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        entry.add_fn.expect("add control")(&mut world, entity);
        assert!((entry.has_fn)(&world, entity));
        assert!(world.get::<VignetteSettings>(entity).unwrap().enabled);
        entry.set_enabled_fn.expect("enable control")(&mut world, entity, false);
        assert!(!world.get::<VignetteSettings>(entity).unwrap().enabled);
        entry.remove_fn.expect("remove control")(&mut world, entity);
        assert!(!(entry.has_fn)(&world, entity));
    }
}
