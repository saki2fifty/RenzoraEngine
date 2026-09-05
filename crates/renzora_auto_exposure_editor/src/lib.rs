//! The Auto Exposure inspector.
//!
//! Editor-only controls share their settings with runtime metering through the
//! contract crate; games do not link these inspector registrations.

use bevy::post_process::auto_exposure::AutoExposure;
use bevy::prelude::*;
use renzora::{AppEditorExt, AutoExposureSettings, InspectorEntry};

fn inspector_entry() -> InspectorEntry {
    InspectorEntry {
        type_id: "auto_exposure",
        display_name: "Auto Exposure",
        icon: "sun",
        category: "camera",
        has_fn: |world, entity| world.get::<AutoExposureSettings>(entity).is_some(),
        add_fn: Some(|world, entity| {
            world
                .entity_mut(entity)
                .insert(AutoExposureSettings::default());
        }),
        remove_fn: Some(|world, entity| {
            world
                .entity_mut(entity)
                .remove::<(AutoExposureSettings, AutoExposure)>();
        }),
        is_enabled_fn: Some(|world, entity| {
            world
                .get::<AutoExposureSettings>(entity)
                .map(|s| s.enabled)
                .unwrap_or(false)
        }),
        set_enabled_fn: Some(|world, entity, val| {
            if let Some(mut s) = world.get_mut::<AutoExposureSettings>(entity) {
                s.enabled = val;
            }
        }),
        // Declarative fields render natively (bevy_ui).
        fields: vec![
            renzora::float_field!(
                "Speed Brighten",
                AutoExposureSettings,
                speed_brighten,
                0.1,
                0.0,
                10.0
            ),
            renzora::float_field!(
                "Speed Darken",
                AutoExposureSettings,
                speed_darken,
                0.1,
                0.0,
                10.0
            ),
            renzora::float_field!(
                "Range Min (EV)",
                AutoExposureSettings,
                range_min,
                0.1,
                -16.0,
                8.0
            ),
            renzora::float_field!(
                "Range Max (EV)",
                AutoExposureSettings,
                range_max,
                0.1,
                -8.0,
                16.0
            ),
            renzora::float_field!(
                "Filter Low (%)",
                AutoExposureSettings,
                filter_low,
                0.01,
                0.0,
                0.5
            ),
            renzora::float_field!(
                "Filter High (%)",
                AutoExposureSettings,
                filter_high,
                0.01,
                0.5,
                1.0
            ),
            renzora::float_field!(
                "Anti-Jitter Band",
                AutoExposureSettings,
                exponential_transition_distance,
                0.05,
                0.0,
                5.0
            ),
            renzora::float_field!(
                "Keep Night Dark",
                AutoExposureSettings,
                keep_dark_strength,
                0.05,
                0.0,
                1.0
            ),
            renzora::float_field!(
                "Keep-Dark Pivot (EV)",
                AutoExposureSettings,
                keep_dark_pivot_ev,
                0.1,
                -8.0,
                16.0
            ),
        ],
    }
}

/// Install auto-exposure inspector controls in the editor only.
#[derive(Default)]
pub struct AutoExposureEditorPlugin;

impl Plugin for AutoExposureEditorPlugin {
    fn build(&self, app: &mut App) {
        if renzora::builtin_plugin_enabled(app, "auto_exposure") {
            app.register_inspector(inspector_entry());
        }
    }
}

renzora::add!(AutoExposureEditorPlugin, Editor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_inspector_fields_callbacks_and_disabled_startup() {
        let entry = inspector_entry();
        assert_eq!(entry.type_id, "auto_exposure");
        assert_eq!(entry.fields.len(), 9);
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        entry.add_fn.unwrap()(&mut world, entity);
        assert!((entry.has_fn)(&world, entity));
        entry.set_enabled_fn.unwrap()(&mut world, entity, false);
        assert!(!world.get::<AutoExposureSettings>(entity).unwrap().enabled);
        entry.remove_fn.unwrap()(&mut world, entity);
        assert!(!(entry.has_fn)(&world, entity));
        for enabled in [true, false] {
            let mut app = App::new();
            app.insert_resource(renzora::DisabledPlugins(if enabled {
                vec![]
            } else {
                vec!["auto_exposure".into()]
            }));
            app.add_plugins(AutoExposureEditorPlugin);
            assert_eq!(
                app.world()
                    .get_resource::<renzora::InspectorRegistry>()
                    .is_some_and(|registry| registry
                        .iter()
                        .any(|entry| entry.type_id == "auto_exposure")),
                enabled
            );
        }
    }
}
