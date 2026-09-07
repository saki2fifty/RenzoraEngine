use bevy::light::SunDisk;
use bevy::prelude::*;
use renzora_lighting::{LightingPlugin, Sun};

#[test]
fn sun_updates_light_and_disk_at_day_and_night() {
    let mut app = App::new();
    app.add_plugins(LightingPlugin);
    let entity = app
        .world_mut()
        .spawn((
            Sun::default(),
            DirectionalLight::default(),
            Transform::default(),
        ))
        .id();
    app.update();
    let sun = app.world().get::<Sun>(entity).unwrap();
    let light = app.world().get::<DirectionalLight>(entity).unwrap();
    assert_eq!(light.illuminance, sun.illuminance);
    assert_eq!(
        app.world().get::<SunDisk>(entity).unwrap().angular_size,
        sun.angular_diameter.to_radians()
    );
    app.world_mut().get_mut::<Sun>(entity).unwrap().elevation = -10.0;
    app.update();
    let light = app.world().get::<DirectionalLight>(entity).unwrap();
    assert_eq!(light.illuminance, 0.0);
    assert!(!light.shadow_maps_enabled);
    assert!(!light.contact_shadows_enabled);
    assert_eq!(app.world().get::<SunDisk>(entity).unwrap().intensity, 0.0);
    assert_eq!(
        app.world().get::<Sun>(entity).unwrap().illuminance,
        40_000.0
    );
}
