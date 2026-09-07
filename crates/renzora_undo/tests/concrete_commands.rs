use bevy::prelude::*;
use renzora_undo::{RenameCmd, SetHierarchyOrderCmd, TransformCmd, UndoCommand};

#[test]
fn transform_rename_and_order_commands_round_trip_and_tolerate_deleted_entities() {
    let mut world = World::new();
    let entity = world
        .spawn((Name::new("before"), Transform::default()))
        .id();
    let mut transform = TransformCmd {
        entity,
        old: Transform::default(),
        new: Transform::from_xyz(1.0, 2.0, 3.0),
    };
    let mut rename = RenameCmd {
        entity,
        old: "before".into(),
        new: "after".into(),
    };
    let mut order = SetHierarchyOrderCmd {
        entity,
        old: None,
        new: Some(7),
    };
    assert_eq!(transform.label(), "Transform");
    assert_eq!(rename.label(), "Rename");
    assert_eq!(order.label(), "Reorder");
    transform.execute(&mut world);
    rename.execute(&mut world);
    order.execute(&mut world);
    assert_eq!(
        world.get::<Transform>(entity).unwrap().translation,
        Vec3::new(1.0, 2.0, 3.0)
    );
    assert_eq!(world.get::<Name>(entity).unwrap().as_str(), "after");
    assert_eq!(
        world
            .get::<renzora_editor_framework::HierarchyOrder>(entity)
            .unwrap()
            .0,
        7
    );
    transform.undo(&mut world);
    rename.undo(&mut world);
    order.undo(&mut world);
    assert_eq!(
        world.get::<Transform>(entity).unwrap().translation,
        Vec3::ZERO
    );
    assert_eq!(world.get::<Name>(entity).unwrap().as_str(), "before");
    assert!(world
        .get::<renzora_editor_framework::HierarchyOrder>(entity)
        .is_none());
    world.despawn(entity);
    for command in [
        &mut transform as &mut dyn UndoCommand,
        &mut rename,
        &mut order,
    ] {
        command.execute(&mut world);
        command.undo(&mut world);
    }
}
