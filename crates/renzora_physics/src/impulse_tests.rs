use super::*;

fn action(entity: Entity, name: &str, x: f32, y: f32) -> renzora::ScriptAction {
    renzora::ScriptAction {
        entity,
        name: name.into(),
        target_entity: None,
        args: [
            ("x".into(), renzora::ScriptActionValue::Float(x)),
            ("y".into(), renzora::ScriptActionValue::Float(y)),
        ]
        .into(),
    }
}

#[cfg(feature = "avian3d")]
#[test]
fn impulse_3d_is_additive_mass_aware_and_respects_body_kind_and_locks() {
    use avian3d::prelude::*;
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        AssetPlugin::default(),
        TransformPlugin,
        PhysicsPlugins::default(),
        bevy::mesh::MeshPlugin,
    ))
    .add_observer(handle_physics_script_actions);
    let body = app
        .world_mut()
        .spawn((
            RigidBody::Dynamic,
            Mass(2.0),
            LinearVelocity(Vec3::new(3.0, 0.0, 0.0)),
        ))
        .id();
    let fixed = app.world_mut().spawn((RigidBody::Static, Mass(2.0))).id();
    let kinematic = app
        .world_mut()
        .spawn((RigidBody::Kinematic, Mass(2.0)))
        .id();
    app.finish();
    app.update();
    app.world_mut().entity_mut(body).insert((
        LinearVelocity(Vec3::new(3.0, 0.0, 0.0)),
        LockedAxes::new().lock_translation_y(),
    ));
    for e in [body, fixed, kinematic] {
        app.world_mut()
            .trigger(action(e, "apply_impulse", 4.0, 10.0));
        app.world_mut()
            .trigger(action(e, "apply_impulse", 2.0, 0.0));
    }
    app.world_mut().flush();
    assert_eq!(
        app.world().get::<LinearVelocity>(body).unwrap().0,
        Vec3::new(6.0, 0.0, 0.0)
    );
    assert_eq!(
        app.world().get::<LinearVelocity>(fixed).unwrap().0,
        Vec3::ZERO
    );
    assert_eq!(
        app.world().get::<LinearVelocity>(kinematic).unwrap().0,
        Vec3::ZERO
    );
    app.world_mut()
        .trigger(action(body, "set_velocity", 10.0, 0.0));
    app.world_mut()
        .trigger(action(body, "apply_impulse", 4.0, 0.0));
    app.world_mut().flush();
    assert_eq!(app.world().get::<LinearVelocity>(body).unwrap().0.x, 12.0);
    app.world_mut()
        .trigger(action(body, "apply_impulse", f32::NAN, 0.0));
    app.world_mut().flush();
    assert_eq!(app.world().get::<LinearVelocity>(body).unwrap().0.x, 12.0);
}

#[cfg(feature = "avian2d")]
#[test]
fn impulse_2d_uses_its_own_mass_and_velocity() {
    use avian2d::prelude::*;
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        AssetPlugin::default(),
        TransformPlugin,
        PhysicsPlugins::default(),
        bevy::mesh::MeshPlugin,
    ))
    .add_observer(handle_physics_script_actions);
    let body = app
        .world_mut()
        .spawn((RuntimePhysics2d, RigidBody::Dynamic, Mass(4.0)))
        .id();
    app.finish();
    app.update();
    app.world_mut()
        .entity_mut(body)
        .insert(LinearVelocity(Vec2::new(1.0, 0.0)));
    app.world_mut()
        .trigger(action(body, "apply_impulse", 8.0, 4.0));
    app.world_mut()
        .trigger(action(body, "apply_impulse", 4.0, 0.0));
    app.world_mut().flush();
    assert_eq!(
        app.world().get::<LinearVelocity>(body).unwrap().0,
        Vec2::new(4.0, 1.0)
    );
    #[cfg(feature = "avian3d")]
    assert!(app
        .world()
        .get::<avian3d::prelude::LinearVelocity>(body)
        .is_none());
}
