//! Integration tests for the in-flight build generation state machine
//! (correction A and B).
//!
//! Tests drive the production `ScriptWatcher` state machine and
//! `PendingRetires` Bevy resource. They do not maintain a parallel
//! `InFlightBuild` struct.

use renzora_identity::CanonicalId;

fn init_task_pool() {
    let _ = bevy::tasks::AsyncComputeTaskPool::get_or_init(|| {
        bevy::tasks::TaskPoolBuilder::new()
            .num_threads(1)
            .build()
    });
}

#[test]
fn pending_task_survives_multiple_finish_frames() {
    init_task_pool();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("a.rs"),
        "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n",
    )
    .unwrap();

    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs").unwrap();

    let mut world = bevy::prelude::World::new();
    world.insert_resource(renzora_rust_script::watch::ScriptWatcher::default());
    world.insert_resource(renzora_rust_script::PendingRetires::default());
    world.insert_resource(renzora_rust_script::LoadedScripts::default());

    // A task that remains Pending forever.
    let task: bevy::tasks::Task<Result<std::path::PathBuf, String>> =
        bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            std::future::pending::<Result<std::path::PathBuf, String>>().await
        });
    {
        let mut watcher = world.resource_mut::<renzora_rust_script::watch::ScriptWatcher>();
        renzora_rust_script::watch::insert_pending_for_test(
            &mut watcher,
            id.clone(),
            task,
            root.join("a.rs"),
        );
    }

    renzora_rust_script::watch::finish(&mut world);
    assert!(
        renzora_rust_script::watch::has_pending_for_test(
            world.resource::<renzora_rust_script::watch::ScriptWatcher>(),
            &id,
        ),
        "Pending task must stay in watcher.building across finish() calls (correction A)"
    );

    renzora_rust_script::watch::finish(&mut world);
    assert!(
        renzora_rust_script::watch::has_pending_for_test(
            world.resource::<renzora_rust_script::watch::ScriptWatcher>(),
            &id,
        ),
        "Pending task must still be in watcher.building after a second finish() (correction A)"
    );
}

#[test]
fn retire_queue_is_resource_owned_and_dedupes() {
    let mut resource = renzora_rust_script::PendingRetires::default();
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs").unwrap();
    resource.enqueue(id.clone());
    resource.enqueue(id.clone());
    resource.enqueue(id.clone());
    let drained = resource.take();
    assert_eq!(drained.len(), 1, "dedup: three enqueues → one retire");
    assert!(drained.contains(&id));
    assert!(resource.take().is_empty());
}

#[test]
fn retire_does_not_require_sdk() {
    let mut world = bevy::prelude::World::new();
    let mut loaded = renzora_rust_script::LoadedScripts::default();
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs").unwrap();
    let f: renzora_rust_script::ScriptFn = |_world: &mut bevy::prelude::World, _e| {};
    loaded.insert_borrowed(id.clone(), f);
    assert!(loaded.is_loaded(&id));

    world.insert_resource(loaded);
    world.insert_resource(renzora_rust_script::PendingRetires::default());
    world.insert_resource(renzora_rust_script::watch::ScriptWatcher::default());
    {
        let mut pending = world.resource_mut::<renzora_rust_script::PendingRetires>();
        pending.enqueue(id.clone());
    }

    renzora_rust_script::watch::finish(&mut world);
    assert!(
        !world
            .resource::<renzora_rust_script::LoadedScripts>()
            .is_loaded(&id),
        "Retire must succeed without an SDK"
    );
}

#[test]
fn edit_during_compilation_sets_pending_dirty() {
    init_task_pool();
    let mut watcher = renzora_rust_script::watch::ScriptWatcher::default();
    let id = CanonicalId::from_rooted(renzora_identity::RootKind::Project, "a.rs").unwrap();

    let task: bevy::tasks::Task<Result<std::path::PathBuf, String>> =
        bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            std::future::pending::<Result<std::path::PathBuf, String>>().await
        });
    renzora_rust_script::watch::insert_pending_for_test(
        &mut watcher,
        id.clone(),
        task,
        std::path::PathBuf::from("/tmp/a.rs"),
    );
    renzora_rust_script::watch::set_pending_dirty_for_test(&mut watcher, &id);
    renzora_rust_script::watch::set_pending_dirty_for_test(&mut watcher, &id);
    assert!(
        renzora_rust_script::watch::pending_dirty_for_test(&watcher, &id),
        "two edits while in-flight both register as pending_dirty"
    );
}
