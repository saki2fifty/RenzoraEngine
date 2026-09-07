use super::*;

fn fonts() -> EmberFonts {
    EmberFonts {
        ui: default(),
        phosphor: default(),
        mono: default(),
        default_ui: default(),
        default_mono: default(),
    }
}

fn registry() -> ShapeRegistry {
    let mut registry = ShapeRegistry::default();
    registry.register(renzora::ShapeEntry {
        id: "cube",
        name: "Cube",
        icon: "",
        category: "Basic",
        create_mesh: |_| Handle::default(),
        default_color: Color::WHITE,
    });
    registry
}

#[test]
fn browser_builds_tiles_and_filters_search_without_losing_registry_entries() {
    let mut app = App::new();
    app.insert_resource(registry());
    app.add_plugins(crate::ShapeLibraryPlugin);
    assert_eq!(
        app.world()
            .resource::<ShapeRegistry>()
            .get("cube")
            .unwrap()
            .icon,
        "cube"
    );
    let fonts = fonts();
    let root = build(&mut app.world_mut().commands(), &fonts);
    app.world_mut().flush();
    assert_eq!(app.world().get::<Children>(root).unwrap().len(), 2);
    let search = app
        .world_mut()
        .query_filtered::<Entity, With<ShapesSearch>>()
        .single(app.world())
        .unwrap();
    app.world_mut()
        .get_mut::<EmberTextInput>(search)
        .unwrap()
        .value = "cube".into();
    let mut search_schedule = Schedule::default();
    search_schedule.add_systems(shape_search_sync);
    search_schedule.run(app.world_mut());
    assert_eq!(app.world().resource::<ShapesState>().search, "cube");
    search_schedule.run(app.world_mut());
    let token = shapes_token(&Rx::new(app.world()));
    let snapshot = shapes_snapshot(&Rx::new(app.world()));
    assert_eq!(snapshot.items.len(), 1);
    let tile = (snapshot.build)(&mut app.world_mut().commands(), &fonts, 0);
    app.world_mut().flush();
    assert_eq!(app.world().get::<ShapeTile>(tile).unwrap().id, "cube");
    assert_eq!(app.world().get::<Children>(tile).unwrap().len(), 2);
    app.world_mut().resource_mut::<ShapesState>().search = "sphere".into();
    assert_ne!(token, shapes_token(&Rx::new(app.world())));
    app.world_mut().resource_mut::<ShapesState>().search = "CUBE".into();
    assert_eq!(shapes_snapshot(&Rx::new(app.world())).items.len(), 1);
    app.world_mut().resource_mut::<ShapesState>().search = "no match".into();
    let empty = shapes_snapshot(&Rx::new(app.world()));
    assert_eq!(empty.items, vec![(u64::MAX, 0)]);
    let label = (empty.build)(&mut app.world_mut().commands(), &fonts, 0);
    app.world_mut().flush();
    assert_eq!(
        app.world().get::<Text>(label).unwrap().0,
        "No shapes match."
    );
    assert_eq!(app.world().resource::<ShapeRegistry>().iter().count(), 1);
    app.world_mut().remove_resource::<ShapeRegistry>();
    let absent = shapes_snapshot(&Rx::new(app.world()));
    assert!(absent.items.is_empty());
    let placeholder = (absent.build)(&mut app.world_mut().commands(), &fonts, 0);
    app.world_mut().flush();
    assert!(app.world().get::<Node>(placeholder).is_some());
}

#[test]
fn clicking_a_tile_queues_an_undoable_shape_at_the_origin() {
    let mut world = World::new();
    world.insert_resource(registry());
    world.init_resource::<Assets<Mesh>>();
    world.init_resource::<Assets<StandardMaterial>>();
    world.init_resource::<renzora_undo::UndoStacks>();
    world.init_resource::<EditorCommands>();
    world.init_resource::<ShapePress>();
    world.init_resource::<ButtonInput<MouseButton>>();
    world.spawn((
        Interaction::Pressed,
        ShapeTile {
            id: "cube",
            name: "Cube",
            color: Color::WHITE,
        },
    ));
    let mut schedule = Schedule::default();
    schedule.add_systems((shape_press, shape_drag_or_click).chain());
    world
        .resource_mut::<ButtonInput<MouseButton>>()
        .press(MouseButton::Left);
    world
        .resource_mut::<ButtonInput<MouseButton>>()
        .release(MouseButton::Left);
    schedule.run(&mut world);
    assert!(world.resource::<ShapePress>().0.is_none());
    let commands = world.resource::<EditorCommands>().drain();
    assert_eq!(commands.len(), 1);
    for command in commands {
        command(&mut world);
    }
    let (entity, name, transform, primitive) = world
        .query::<(Entity, &Name, &Transform, &renzora::MeshPrimitive)>()
        .single(&world)
        .unwrap();
    assert_eq!(name.as_str(), "Cube");
    assert_eq!(transform.translation, Vec3::ZERO);
    assert_eq!(primitive.0, "cube");
    let mut command = world
        .resource_mut::<renzora_undo::UndoStacks>()
        .pop_undo(&UndoContext::Scene)
        .unwrap();
    command.undo(&mut world);
    assert!(world.get_entity(entity).is_err());
}

#[test]
fn tile_hover_highlights_and_restores_background_and_border() {
    let mut app = App::new();
    app.add_plugins(renzora_ember::reactive::ReactivePlugin);
    let tile = shape_tile(
        &mut app.world_mut().commands(),
        &fonts(),
        "cube",
        "Cube",
        "cube",
        Color::WHITE,
    );
    app.world_mut().flush();
    for (interaction, background, outline) in [
        (Interaction::None, rgb(section_bg()), rgb(border())),
        (Interaction::Hovered, rgb(hover_bg()), rgb(accent())),
        (Interaction::Pressed, rgb(hover_bg()), rgb(accent())),
        (Interaction::None, rgb(section_bg()), rgb(border())),
    ] {
        *app.world_mut().get_mut::<Interaction>(tile).unwrap() = interaction;
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(tile).unwrap().0,
            background
        );
        assert_eq!(
            *app.world().get::<BorderColor>(tile).unwrap(),
            BorderColor::all(outline)
        );
    }
}

#[test]
fn tile_press_becomes_drag_only_after_threshold_and_release_clears_pending() {
    let mut world = World::new();
    world.init_resource::<ShapePress>();
    world.init_resource::<ShapeDragState>();
    world.init_resource::<ButtonInput<MouseButton>>();
    let mut window = Window::default();
    window.set_cursor_position(Some(Vec2::new(10.0, 10.0)));
    let window = world.spawn((window, PrimaryWindow)).id();
    let tile = world
        .spawn((
            Interaction::Pressed,
            ShapeTile {
                id: "cube",
                name: "Cube",
                color: Color::WHITE,
            },
        ))
        .id();
    let mut press_schedule = Schedule::default();
    press_schedule.add_systems(shape_press);
    let mut drag_schedule = Schedule::default();
    drag_schedule.add_systems(shape_drag_or_click);
    press_schedule.run(&mut world);
    drag_schedule.run(&mut world);
    assert!(world.resource::<ShapePress>().0.is_some());
    assert!(world.resource::<ShapeDragState>().dragging_shape.is_none());
    world
        .get_mut::<Window>(window)
        .unwrap()
        .set_cursor_position(Some(Vec2::new(30.0, 10.0)));
    drag_schedule.run(&mut world);
    assert!(world.resource::<ShapePress>().0.is_none());
    assert_eq!(
        world.resource::<ShapeDragState>().dragging_shape,
        Some("cube")
    );
    assert!(world.resource::<ShapeDragState>().native_drag);
    *world.get_mut::<Interaction>(tile).unwrap() = Interaction::Hovered;
    press_schedule.run(&mut world);
    assert!(world.resource::<ShapePress>().0.is_none());
    *world.get_mut::<Interaction>(tile).unwrap() = Interaction::Pressed;
    press_schedule.run(&mut world);
    world
        .resource_mut::<ButtonInput<MouseButton>>()
        .press(MouseButton::Left);
    world
        .resource_mut::<ButtonInput<MouseButton>>()
        .release(MouseButton::Left);
    drag_schedule.run(&mut world);
    assert!(world.resource::<ShapePress>().0.is_none());
}
