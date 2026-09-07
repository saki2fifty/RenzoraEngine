use bevy::prelude::*;
use renzora::{ScriptAction, ScriptActionValue as V};
use renzora_engine::camera_script::{
    auto_init_camera_read_state, handle_camera_script_actions, update_camera_read_state,
    CameraReadState,
};

#[test]
fn camera_actions_target_clamp_and_ignore_invalid_requests() {
    let mut app = App::new();
    app.add_observer(handle_camera_script_actions);
    app.add_systems(
        Update,
        (auto_init_camera_read_state, update_camera_read_state).chain(),
    );
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            Projection::Perspective(PerspectiveProjection::default()),
        ))
        .id();
    let other = app
        .world_mut()
        .spawn(Projection::Orthographic(
            OrthographicProjection::default_3d(),
        ))
        .id();
    app.update();
    assert!(app.world().get::<CameraReadState>(camera).unwrap().fov > 0.0);
    for (value, expected) in [(V::Float(200.0), 170.0), (V::Int(1), 10.0)] {
        app.world_mut().trigger(ScriptAction {
            name: "set_camera_fov".into(),
            entity: other,
            target_entity: None,
            args: [
                ("degrees".into(), value),
                ("entity_id".into(), V::Int(camera.to_bits() as i64)),
            ]
            .into(),
        });
        app.update();
        assert!((app.world().get::<CameraReadState>(camera).unwrap().fov - expected).abs() < 0.001);
    }
    for (name, target, value) in [
        ("unrelated", camera, V::Float(80.0)),
        ("set_camera_fov", camera, V::String("bad".into())),
        ("set_camera_fov", other, V::Float(80.0)),
        ("set_camera_fov", Entity::PLACEHOLDER, V::Float(80.0)),
    ] {
        app.world_mut().trigger(ScriptAction {
            name: name.into(),
            entity: target,
            target_entity: None,
            args: [("degrees".into(), value)].into(),
        });
    }
    app.update();
    assert!((app.world().get::<CameraReadState>(camera).unwrap().fov - 10.0).abs() < 0.001);
    assert!(matches!(
        app.world().get::<Projection>(other),
        Some(Projection::Orthographic(_))
    ));
}

#[test]
fn vfs_disk_fallback_handles_text_binary_and_missing_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("data");
    let path = path.to_str().unwrap();
    let vfs = renzora_engine::Vfs::default();
    assert!(!vfs.has_archive());
    assert!(vfs.archive().is_none());
    assert!(vfs.archive_arc().is_none());
    assert!(!vfs.exists(path));
    assert!(vfs.read(path).is_none());
    std::fs::write(path, "hello").unwrap();
    assert!(vfs.exists(path));
    assert_eq!(vfs.read_string(path).as_deref(), Some("hello"));
    std::fs::write(path, [255]).unwrap();
    assert_eq!(vfs.read(path), Some(vec![255]));
    assert!(vfs.read_string(path).is_none());
    assert!(renzora_engine::Vfs::from_rpak_bytes(b"invalid").is_err());
}
