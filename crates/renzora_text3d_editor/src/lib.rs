//! Editor-only authoring controls for shared 3D-text data.

use bevy::prelude::*;
use renzora::text3d::Text3d;
use renzora::{AppEditorExt, EntityPreset, FieldDef, FieldType, FieldValue, InspectorEntry};

fn inspector_entry() -> InspectorEntry {
    InspectorEntry {
        type_id: "text3d",
        display_name: "3D Text",
        icon: "text-t",
        category: "basic",
        has_fn: |w, e| w.get::<Text3d>(e).is_some(),
        add_fn: Some(|w, e| {
            w.entity_mut(e).insert(Text3d::default());
        }),
        remove_fn: Some(|w, e| {
            w.entity_mut(e).remove::<Text3d>();
        }),
        is_enabled_fn: None,
        set_enabled_fn: None,
        fields: vec![
            renzora::string_field!("Text", Text3d, text),
            FieldDef {
                name: "Font",
                field_type: FieldType::Asset {
                    extensions: vec!["ttf".into(), "otf".into()],
                },
                get_fn: |w, e| {
                    w.get::<Text3d>(e)
                        .map(|t| FieldValue::Asset((!t.font.is_empty()).then(|| t.font.clone())))
                },
                set_fn: |w, e, v| {
                    if let (FieldValue::Asset(p), Some(mut t)) = (v, w.get_mut::<Text3d>(e)) {
                        t.font = p.unwrap_or_default();
                    }
                },
            },
            FieldDef {
                name: "Mode",
                field_type: FieldType::Enum {
                    options: &["flat", "mesh"],
                },
                get_fn: |w, e| w.get::<Text3d>(e).map(|t| FieldValue::Enum(t.mode.clone())),
                set_fn: |w, e, v| {
                    if let (FieldValue::Enum(s), Some(mut t)) = (v, w.get_mut::<Text3d>(e)) {
                        t.mode = s;
                    }
                },
            },
            renzora::float_field!("Size", Text3d, size, 1.0, 1.0, 2000.0),
            renzora::float_field!("Depth", Text3d, depth, 0.01, 0.0, 5.0),
            FieldDef {
                name: "Color",
                field_type: FieldType::Color,
                get_fn: |w, e| w.get::<Text3d>(e).map(|t| FieldValue::Color(t.color)),
                set_fn: |w, e, v| {
                    if let (FieldValue::Color(c), Some(mut t)) = (v, w.get_mut::<Text3d>(e)) {
                        t.color = c;
                    }
                },
            },
        ],
    }
}

/// Install the 3D-text preset and inspector without renderer dependencies.
#[derive(Default)]
pub struct Text3dEditorPlugin;

impl Plugin for Text3dEditorPlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "text3d") {
            return;
        }
        app.register_inspector(inspector_entry());
        app.register_entity_preset(EntityPreset {
            id: "text3d",
            display_name: "3D Text",
            icon: "text-t",
            category: "basic",
            spawn_fn: |world| {
                world
                    .spawn((
                        Name::new("3D Text"),
                        Text3d::default(),
                        Transform::default(),
                        Visibility::default(),
                    ))
                    .id()
            },
        });
    }
}

renzora::add!(Text3dEditorPlugin, Editor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_text_font_mode_and_component_callbacks() {
        let entry = inspector_entry();
        assert_eq!(entry.fields.len(), 6);
        let mut world = World::new();
        let entity = world.spawn_empty().id();
        entry.add_fn.unwrap()(&mut world, entity);
        for (name, value) in [
            ("Text", FieldValue::String("Saved text".into())),
            ("Font", FieldValue::Asset(Some("fonts/custom.ttf".into()))),
            ("Mode", FieldValue::Enum("mesh".into())),
        ] {
            let field = entry
                .fields
                .iter()
                .find(|field| field.name == name)
                .unwrap();
            (field.set_fn)(&mut world, entity, value);
        }
        let text = world.get::<Text3d>(entity).unwrap();
        assert_eq!(text.text, "Saved text");
        assert_eq!(text.font, "fonts/custom.ttf");
        assert_eq!(text.mode, "mesh");
        entry.remove_fn.unwrap()(&mut world, entity);
        assert!(!(entry.has_fn)(&world, entity));
    }
}
