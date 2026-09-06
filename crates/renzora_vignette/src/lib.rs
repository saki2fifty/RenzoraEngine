//! Route authored vignette settings to Bevy's built-in camera effect.

use bevy::{post_process::effect_stack::Vignette, prelude::*};
pub use renzora::VignetteSettings;

fn vignette_from(s: &VignetteSettings) -> Vignette {
    Vignette {
        intensity: s.intensity,
        radius: s.radius,
        smoothness: s.smoothness,
        roundness: s.roundness,
        color: Color::srgb(s.color.x, s.color.y, s.color.z),
        edge_compensation: s.edge_compensation,
        center: Vec2::new(0.5, 0.5),
    }
}

fn sync_vignette(
    mut commands: Commands,
    sources: Query<(Entity, Ref<VignetteSettings>)>,
    routing: Res<renzora::EffectRouting>,
    mut removed: RemovedComponents<VignetteSettings>,
) {
    let removed: bevy::platform::collections::HashSet<_> = removed.read().collect();
    for (target, source_list) in routing.iter() {
        // Only routes containing a removed source need to choose a fallback;
        // unrelated cameras retain their existing effect and change ticks.
        let routing_changed =
            routing.is_changed() || source_list.iter().any(|source| removed.contains(source));
        let mut found = false;
        for &src in source_list {
            if let Ok((_, settings)) = sources.get(src) {
                if !routing_changed && !settings.is_changed() {
                    found = true;
                    break;
                }
                if settings.enabled {
                    commands.entity(*target).insert(vignette_from(&settings));
                } else {
                    commands.entity(*target).remove::<Vignette>();
                }
                found = true;
                break;
            }
        }
        if !found && routing_changed {
            if let Ok(mut ec) = commands.get_entity(*target) {
                ec.remove::<Vignette>();
            }
        }
    }
}

/// Install vignette rendering without editor dependencies.
#[derive(Default)]
pub struct VignettePlugin;

impl Plugin for VignettePlugin {
    fn build(&self, app: &mut App) {
        if !renzora::builtin_plugin_enabled(app, "vignette") {
            return;
        }
        app.register_type::<VignetteSettings>();
        app.add_systems(Update, sync_vignette);
    }
}

renzora::add!(VignettePlugin, Runtime);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_authored_values_and_honors_enabled_setting() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins::default());
        let source = app
            .world_mut()
            .spawn(VignetteSettings {
                intensity: 2.0,
                radius: 0.4,
                ..default()
            })
            .id();
        let camera = app.world_mut().spawn_empty().id();
        app.insert_resource(renzora::EffectRouting {
            routes: vec![(camera, vec![source])],
        });
        app.add_plugins(VignettePlugin);
        app.update();
        let effect = app.world().get::<Vignette>(camera).expect("routed effect");
        assert_eq!(effect.intensity, 2.0);
        assert_eq!(effect.radius, 0.4);
        app.world_mut()
            .get_mut::<VignetteSettings>(source)
            .unwrap()
            .enabled = false;
        app.update();
        assert!(app.world().get::<Vignette>(camera).is_none());
    }

    #[test]
    fn removing_one_source_preserves_other_routes_and_selects_fallback() {
        let mut app = App::new();
        let first = app
            .world_mut()
            .spawn(VignetteSettings {
                intensity: 1.0,
                ..default()
            })
            .id();
        let fallback = app
            .world_mut()
            .spawn(VignetteSettings {
                intensity: 2.0,
                ..default()
            })
            .id();
        let other = app
            .world_mut()
            .spawn(VignetteSettings {
                intensity: 3.0,
                ..default()
            })
            .id();
        let a = app.world_mut().spawn_empty().id();
        let b = app.world_mut().spawn_empty().id();
        app.insert_resource(renzora::EffectRouting {
            routes: vec![(a, vec![first, fallback]), (b, vec![other])],
        });
        app.add_plugins(VignettePlugin);
        app.update();
        app.world_mut()
            .entity_mut(first)
            .remove::<VignetteSettings>();
        app.update();
        assert_eq!(app.world().get::<Vignette>(a).unwrap().intensity, 2.0);
        assert_eq!(app.world().get::<Vignette>(b).unwrap().intensity, 3.0);
        app.world_mut()
            .entity_mut(fallback)
            .remove::<VignetteSettings>();
        app.update();
        assert!(app.world().get::<Vignette>(a).is_none());
        assert_eq!(app.world().get::<Vignette>(b).unwrap().intensity, 3.0);
    }

    #[test]
    fn disabled_plugin_installs_no_routing_systems() {
        let mut app = App::new();
        app.insert_resource(renzora::DisabledPlugins(vec!["vignette".into()]));
        app.add_plugins(VignettePlugin);
        // No EffectRouting resource: a mistakenly installed system would fail.
        app.update();
        assert_eq!(
            app.world().resource::<renzora::PluginInventory>().entries[0].state,
            renzora::PluginState::Disabled
        );
    }
}
