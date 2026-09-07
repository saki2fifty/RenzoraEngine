//! Cloth physics distribution plugin.
//!
//! Wraps the vendored `bevy_silk` verlet cloth engine and registers it with the
//! Renzora runtime via `renzora::add!`. Built as a statically linked `rlib`;
//! generated runtime wiring installs it in builds that include cloth.
//!
//! Add a `bevy_silk::prelude::ClothBuilder` to any entity with a mesh to turn
//! it into cloth (see the `bevy_silk` docs for pinning / stick-generation).
//!
//! Cloth follows the world wind (`renzora::WindState`), so a flag and the grass
//! under it move together. `bevy_silk` has had a `Winds` resource all along;
//! nothing had ever written to it, which is why cloth used to hang dead still
//! in a scene full of moving foliage.

use bevy::prelude::*;
use bevy_silk::prelude::{Wind, Winds};
use renzora::WindState;

/// Index in [`Winds::wind_forces`] that the world wind owns.
///
/// Slot 0 rather than "the whole list", so a scene can still push extra
/// hand-authored forces (a scripted downdraft, a fan) and have them add on top
/// instead of being clobbered every frame.
const WORLD_WIND_SLOT: usize = 0;

/// Mirror [`WindState`] into `bevy_silk`'s wind resource.
///
/// A `ConstantWind` refreshed per frame, not a `SinWave`: the gust envelope is
/// already evaluated in `WindState`, and letting silk apply its own sine on top
/// would beat against it at some unrelated frequency — cloth would gust when
/// the grass beside it did not.
fn sync_cloth_wind(wind: Option<Res<WindState>>, mut winds: ResMut<Winds>) {
    let velocity = wind.as_deref().copied().unwrap_or_default().velocity();
    // Avoid false change ticks without missing external slot edits or removal.
    if matches!(winds.wind_forces.get(WORLD_WIND_SLOT),
        Some(Wind::ConstantWind { velocity: current }) if *current == velocity)
    {
        return;
    }
    let world = Wind::ConstantWind { velocity };
    match winds.wind_forces.get_mut(WORLD_WIND_SLOT) {
        Some(slot) => *slot = world,
        None => winds.wind_forces.push(world),
    }
}

/// Runtime-scope plugin that installs `bevy_silk`'s cloth simulation.
#[derive(Default)]
pub struct ClothPlugin;

impl Plugin for ClothPlugin {
    fn build(&self, app: &mut App) {
        info!("[runtime] ClothPlugin (bevy_silk verlet cloth)");
        app.add_plugins(bevy_silk::prelude::ClothPlugin)
            .init_resource::<Winds>()
            .add_systems(Update, sync_cloth_wind);
    }
}

renzora::add!(ClothPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_wind_is_quiet_and_live_changes_preserve_extra_forces() {
        let mut app = App::new();
        app.init_resource::<Winds>();
        let system = app.world_mut().register_system(sync_cloth_wind);
        app.world_mut().run_system(system).unwrap();
        app.world_mut()
            .resource_mut::<Winds>()
            .wind_forces
            .push(Wind::ConstantWind { velocity: Vec3::Y });
        for _ in 0..1000 {
            app.world_mut().clear_trackers();
            app.world_mut().run_system(system).unwrap();
            assert!(!app.world().is_resource_changed::<Winds>());
        }
        app.insert_resource(WindState {
            speed: 4.0,
            ..default()
        });
        app.world_mut().clear_trackers();
        app.world_mut().run_system(system).unwrap();
        assert!(app.world().is_resource_changed::<Winds>());
        assert_eq!(
            app.world().resource::<Winds>().current_velocity(0.0),
            Vec3::new(4.0, 1.0, 0.0)
        );
        {
            let mut wind = app.world_mut().resource_mut::<WindState>();
            wind.gust_strength = 0.5;
            wind.gust = 1.0;
        }
        app.world_mut().run_system(system).unwrap();
        assert_eq!(
            app.world().resource::<Winds>().current_velocity(0.0),
            Vec3::new(6.0, 1.0, 0.0)
        );
        app.world_mut().remove_resource::<WindState>();
        app.world_mut().run_system(system).unwrap();
        assert_eq!(
            app.world().resource::<Winds>().current_velocity(0.0),
            Vec3::Y
        );
        app.world_mut().resource_mut::<Winds>().wind_forces[0] = Wind::default();
        app.world_mut().run_system(system).unwrap();
        assert!(
            matches!(app.world().resource::<Winds>().wind_forces[0], Wind::ConstantWind { velocity } if velocity == Vec3::ZERO)
        );
        app.world_mut().resource_mut::<Winds>().wind_forces.clear();
        app.world_mut().run_system(system).unwrap();
        assert_eq!(app.world().resource::<Winds>().wind_forces.len(), 1);
    }
}
