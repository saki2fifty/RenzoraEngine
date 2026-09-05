//! The Night Stars inspector.
//!
//! Editor-only controls for the shared starfield settings.

use bevy::prelude::*;
use renzora::{AppEditorExt, InspectorEntry, NightStarsData};

fn inspector_entry() -> InspectorEntry {
    InspectorEntry {
        type_id: "night_stars",
        display_name: "Night Stars",
        icon: "moon-stars",
        category: "rendering",
        has_fn: |world, entity| world.get::<NightStarsData>(entity).is_some(),
        add_fn: Some(|world, entity| {
            world.entity_mut(entity).insert(NightStarsData::default());
        }),
        remove_fn: Some(|world, entity| {
            world.entity_mut(entity).remove::<NightStarsData>();
        }),
        is_enabled_fn: Some(|world, entity| {
            world
                .get::<NightStarsData>(entity)
                .map(|s| s.enabled)
                .unwrap_or(false)
        }),
        set_enabled_fn: Some(|world, entity, val| {
            if let Some(mut s) = world.get_mut::<NightStarsData>(entity) {
                s.enabled = val;
            }
        }),
        fields: vec![
            renzora::float_field!("Density", NightStarsData, density, 0.01, 0.0, 1.0),
            renzora::float_field!("Brightness", NightStarsData, brightness, 0.05, 0.0, 10.0),
            renzora::float_field!("Star Size", NightStarsData, star_size, 0.05, 0.2, 5.0),
            renzora::float_field!(
                "Twinkle Speed",
                NightStarsData,
                twinkle_speed,
                0.05,
                0.0,
                10.0
            ),
            renzora::float_field!(
                "Twinkle Amount",
                NightStarsData,
                twinkle_amount,
                0.01,
                0.0,
                1.0
            ),
            renzora::float_field!("Horizon Fade", NightStarsData, horizon_fade, 0.01, 0.0, 1.0),
            renzora::tuple_color_field!("Color", NightStarsData, color),
        ],
    }
}

/// Install starfield inspector controls in the editor only.
#[derive(Default)]
pub struct NightStarsEditorPlugin;

impl Plugin for NightStarsEditorPlugin {
    fn build(&self, app: &mut App) {
        if renzora::builtin_plugin_enabled(app, "night_stars") {
            app.register_inspector(inspector_entry());
        }
    }
}

renzora::add!(NightStarsEditorPlugin, Editor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_fields_and_shared_component_callbacks() {
        let entry = inspector_entry();
        assert_eq!(entry.fields.len(), 7);
        assert_eq!(entry.type_id, "night_stars");
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        entry.add_fn.unwrap()(&mut world, entity);
        assert!((entry.has_fn)(&world, entity));
        entry.set_enabled_fn.unwrap()(&mut world, entity, false);
        assert!(!world.get::<NightStarsData>(entity).unwrap().enabled);
        entry.remove_fn.unwrap()(&mut world, entity);
        assert!(!(entry.has_fn)(&world, entity));
    }
}
