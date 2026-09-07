//! Runtime asset-load progress — exposed to scripts so a boot scene can
//! drive a loading bar against actual load state.
//!
//! [`tick_asset_load_progress`] refreshes scene totals when mesh data or the
//! archive changes and counts how many entities still carry
//! `PendingMeshInstanceRehydrate` (i.e. haven't had their `Gltf` asset
//! finish loading). The pending paths are looked up in the rpak index to
//! compute byte-level totals; without an rpak the byte counts stay zero
//! and the script can fall back to the file-count ratio.
//!
//! The single [`AssetLoadProgress`] resource is the source of truth.
//! Scripts read it through the `asset_progress()` binding (the
//! scripting crate copies it into a thread-local before each script
//! tick — see `renzora_scripting::asset_progress_handler`).

use bevy::prelude::*;

// Only for `PendingMeshInstanceRehydrate`, which is itself `gltf`-gated.
#[cfg(feature = "gltf")]
use crate::scene_io;
use crate::Vfs;
use renzora::MeshInstanceData;

fn update_progress_path(target: &mut Option<String>, source: Option<&str>) {
    if target.as_deref() == source {
        return;
    }
    match source {
        Some(path) => {
            let text = target.get_or_insert_with(String::new);
            text.clear();
            text.push_str(path);
        }
        None => *target = None,
    }
}

/// Lifecycle state for the asset-load progress tracker.
#[derive(Default, Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadProgressState {
    /// Nothing pending — either we never started loading or everything
    /// from the last load completed and the tracker is at rest.
    #[default]
    Idle,
    /// At least one asset is still loading.
    Loading,
    /// Every tracked asset finished. Persists until another pending load
    /// starts; this is a state, not a one-frame completion event.
    Done,
}

/// Snapshot of asset-load progress, refreshed each frame by
/// [`tick_asset_load_progress`].
#[derive(Resource, Default, Clone, Debug)]
pub struct AssetLoadProgress {
    pub state: LoadProgressState,
    /// Total `MeshInstanceData` entities in the scene with a `model_path`.
    pub total_files: u32,
    /// Files whose asset has finished loading (no `PendingMeshInstanceRehydrate`).
    pub loaded_files: u32,
    /// Sum of compressed-size for every tracked file, looked up in the
    /// rpak index. Zero when no rpak is mounted (editor or `--project`
    /// runs) — script should fall back to file counts.
    pub total_bytes: u64,
    /// Sum of compressed-size for files that finished loading.
    pub loaded_bytes: u64,
    /// Path of the most recently observed pending file. Useful as
    /// "currently loading: X" UI text.
    pub current_path: Option<String>,
    /// Wall-clock seconds since tracking began, including time after completion.
    pub elapsed_secs: f32,
    /// Wall-clock seconds at which the tracking interval started. Used to compute
    /// `elapsed_secs`. Internal — scripts should read `elapsed_secs`.
    started_at: Option<f64>,
}

impl AssetLoadProgress {
    /// Best-effort fraction in `[0.0, 1.0]`. Prefers byte-based progress
    /// when an rpak is mounted (smoother across mixed-size assets);
    /// falls back to file-count when bytes are unavailable.
    pub fn fraction(&self) -> f32 {
        if self.total_bytes > 0 {
            (self.loaded_bytes as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
        } else if self.total_files > 0 {
            (self.loaded_files as f32 / self.total_files as f32).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, LoadProgressState::Idle)
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.state, LoadProgressState::Loading)
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, LoadProgressState::Done)
    }
}

/// Refresh [`AssetLoadProgress`] from current scene state.
///
/// Runs every `Update`. Recounts scene totals after mesh or archive changes;
/// pending markers are checked each time so asynchronous completions remain
/// visible without changing the loading state machine.
pub fn tick_asset_load_progress(
    instances: Query<&MeshInstanceData>,
    changed_instances: Query<(), Changed<MeshInstanceData>>,
    mut removed_instances: RemovedComponents<MeshInstanceData>,
    mut totals: Local<Option<(u32, u64, bool)>>,
    // Only models loaded through bevy_gltf are ever "pending" — see the `gltf`
    // feature. A project built from engine primitives has none.
    #[cfg(feature = "gltf")] pending: Query<
        &MeshInstanceData,
        With<scene_io::PendingMeshInstanceRehydrate>,
    >,
    vfs: Option<Res<Vfs>>,
    time: Res<Time>,
    mut progress: ResMut<AssetLoadProgress>,
) {
    // Count files. Only consider entities whose `model_path` is set —
    // primitives without an external mesh have nothing to load.
    let archive = vfs.as_ref().and_then(|v| v.archive());
    // Drain removals even when another invalidation already requires a recount.
    // Pending-marker changes affect loaded counts, not scene totals.
    let removed = removed_instances.read().count() != 0;
    let recount = totals.is_none()
        || removed
        || !changed_instances.is_empty()
        || vfs.as_ref().is_some_and(|v| v.is_changed())
        || totals
            .as_ref()
            .is_some_and(|(_, _, present)| *present != vfs.is_some());
    if recount {
        let mut total_files = 0;
        let mut total_bytes = 0;
        for data in instances.iter() {
            if let Some(path) = data.model_path.as_deref() {
                total_files += 1;
                if let Some(archive) = archive {
                    if let Some(entry) = archive.entry(path) {
                        total_bytes += entry.compressed_size;
                    }
                }
            }
        }
        *totals = Some((total_files, total_bytes, vfs.is_some()));
    }
    let (total_files, total_bytes, _) = totals.expect("totals initialized above");

    // `mut` is only exercised by the glTF-pending loop below; without models
    // nothing is ever pending, so the three stay at their initial values.
    #[cfg_attr(not(feature = "gltf"), allow(unused_mut))]
    let mut pending_files: u32 = 0;
    #[cfg_attr(not(feature = "gltf"), allow(unused_mut))]
    let mut pending_bytes: u64 = 0;
    #[cfg_attr(not(feature = "gltf"), allow(unused_mut))]
    let mut current: Option<&str> = None;
    #[cfg(feature = "gltf")]
    for data in pending.iter() {
        if let Some(path) = data.model_path.as_deref() {
            pending_files += 1;
            if let Some(archive) = archive {
                if let Some(entry) = archive.entry(path) {
                    pending_bytes += entry.compressed_size;
                }
            }
            if current.is_none() {
                current = Some(path);
            }
        }
    }

    let loaded_files = total_files.saturating_sub(pending_files);
    let loaded_bytes = total_bytes.saturating_sub(pending_bytes);

    // State machine: Idle ↔ Loading ↔ Done.
    let now_secs = time.elapsed_secs_f64();
    let next_state = match progress.state {
        LoadProgressState::Idle => {
            if total_files == 0 {
                LoadProgressState::Idle
            } else if pending_files > 0 {
                progress.started_at = Some(now_secs);
                LoadProgressState::Loading
            } else {
                progress.started_at = Some(now_secs);
                LoadProgressState::Done
            }
        }
        LoadProgressState::Loading => {
            if pending_files == 0 {
                LoadProgressState::Done
            } else {
                LoadProgressState::Loading
            }
        }
        LoadProgressState::Done => {
            if pending_files > 0 {
                progress.started_at = Some(now_secs);
                LoadProgressState::Loading
            } else {
                LoadProgressState::Done
            }
        }
    };

    let elapsed = progress
        .started_at
        .map(|s| (now_secs - s).max(0.0) as f32)
        .unwrap_or(0.0);

    update_progress_path(&mut progress.current_path, current);
    progress.state = next_state;
    progress.total_files = total_files;
    progress.loaded_files = loaded_files;
    progress.total_bytes = total_bytes;
    progress.loaded_bytes = loaded_bytes;
    progress.elapsed_secs = elapsed;
}

/// No-op stand-in when scripting is stripped — the bridge exists only so
/// scripts can call `asset_progress()`, and there are none.
#[cfg(not(feature = "scripting"))]
pub fn publish_asset_progress_to_bridge() {}

/// Mirror [`AssetLoadProgress`] into the scripting crate's
/// `AssetProgressBridge` so `asset_progress()` reads can find it without
/// the scripting crate depending on `renzora_engine`.
///
/// Runs every `Update` after [`tick_asset_load_progress`].
#[cfg(feature = "scripting")]
pub fn publish_asset_progress_to_bridge(
    progress: Res<AssetLoadProgress>,
    mut bridge: Option<ResMut<renzora_scripting::AssetProgressBridge>>,
) {
    let Some(ref mut bridge) = bridge else {
        return;
    };
    let state = match progress.state {
        LoadProgressState::Idle => "idle",
        LoadProgressState::Loading => "loading",
        LoadProgressState::Done => "done",
    };
    let fraction = progress.fraction();
    if bridge.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.state == state
            && snapshot.total_files == progress.total_files
            && snapshot.loaded_files == progress.loaded_files
            && snapshot.total_bytes == progress.total_bytes
            && snapshot.loaded_bytes == progress.loaded_bytes
            && snapshot.current_path == progress.current_path
            && snapshot.elapsed_secs == progress.elapsed_secs
            && snapshot.fraction == fraction
    }) {
        return;
    }
    let snapshot = bridge.snapshot.get_or_insert_with(Default::default);
    snapshot.state = state;
    snapshot.total_files = progress.total_files;
    snapshot.loaded_files = progress.loaded_files;
    snapshot.total_bytes = progress.total_bytes;
    snapshot.loaded_bytes = progress.loaded_bytes;
    update_progress_path(&mut snapshot.current_path, progress.current_path.as_deref());
    snapshot.elapsed_secs = progress.elapsed_secs;
    snapshot.fraction = fraction;
}

/// No-op stand-in when scripting is stripped; see
/// [`publish_asset_progress_to_bridge`].
#[cfg(not(feature = "scripting"))]
pub fn publish_scene_load_to_bridge() {}

/// Mirror [`crate::scene_io::SceneLoadState`] into the scripting crate's
/// `SceneLoadBridge` so `scene_load_state()` reads can find it.
///
/// Separate from [`publish_asset_progress_to_bridge`] because the two track
/// different things: this is how far through *spawning the scene* we are,
/// that one is how many of its **models** have finished loading. A scene
/// reaches `ready` while its meshes are still streaming in.
#[cfg(feature = "scripting")]
pub fn publish_scene_load_to_bridge(
    state: Option<Res<crate::scene_io::SceneLoadState>>,
    bridge: Option<ResMut<renzora_scripting::SceneLoadBridge>>,
) {
    use crate::scene_io::SceneLoadPhase;

    let (Some(state), Some(mut bridge)) = (state, bridge) else {
        return;
    };
    let phase = match state.phase {
        SceneLoadPhase::Idle => "idle",
        SceneLoadPhase::Loading => "loading",
        SceneLoadPhase::Ready => "ready",
        SceneLoadPhase::Failed => "failed",
    };
    if bridge.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.phase == phase
            && snapshot.current_path == state.current_path
            && snapshot.progress == state.progress
    }) {
        return;
    }
    let snapshot = bridge.snapshot.get_or_insert_with(Default::default);
    snapshot.phase = phase;
    update_progress_path(&mut snapshot.current_path, state.current_path.as_deref());
    snapshot.progress = state.progress;
}

#[cfg(all(test, feature = "gltf", feature = "scripting"))]
mod tests {
    use super::*;
    use renzora_scripting::{AssetProgressBridge, SceneLoadBridge};

    #[test]
    fn cached_totals_follow_edits_removals_and_pending_transitions() {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<AssetLoadProgress>()
            .add_systems(Update, tick_asset_load_progress);
        app.update();
        let entity = app
            .world_mut()
            .spawn(MeshInstanceData {
                model_path: Some("one.glb".into()),
            })
            .id();
        app.update();
        for _ in 0..1000 {
            app.update();
            let progress = app.world().resource::<AssetLoadProgress>();
            assert_eq!((progress.total_files, progress.loaded_files), (1, 1));
        }
        app.world_mut()
            .entity_mut(entity)
            .insert(scene_io::PendingMeshInstanceRehydrate(Handle::default()));
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().loaded_files, 0);
        assert!(app.world().resource::<AssetLoadProgress>().is_loading());
        app.world_mut()
            .get_mut::<MeshInstanceData>(entity)
            .unwrap()
            .model_path = None;
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_files, 0);
        app.world_mut()
            .get_mut::<MeshInstanceData>(entity)
            .unwrap()
            .model_path = Some("two.glb".into());
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_files, 1);
        app.world_mut()
            .entity_mut(entity)
            .remove::<MeshInstanceData>();
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_files, 0);
        app.world_mut().entity_mut(entity).insert(MeshInstanceData {
            model_path: Some("three.glb".into()),
        });
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_files, 1);
        app.world_mut().despawn(entity);
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_files, 0);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cached_bytes_follow_archive_insertion_replacement_and_removal() {
        let make_vfs = |bytes: Vec<u8>| {
            let mut packer = renzora_rpak::RpakPacker::new();
            packer.add_file("one.glb", bytes);
            Vfs::from_rpak_bytes(&packer.finish(0).unwrap()).unwrap()
        };
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<AssetLoadProgress>()
            .add_systems(Update, tick_asset_load_progress);
        app.world_mut().spawn(MeshInstanceData {
            model_path: Some("one.glb".into()),
        });
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_bytes, 0);
        for data in [vec![1; 17], vec![2; 91]] {
            let vfs = make_vfs(data);
            let expected = vfs
                .archive()
                .unwrap()
                .entry("one.glb")
                .unwrap()
                .compressed_size;
            app.insert_resource(vfs);
            app.update();
            assert_eq!(
                app.world().resource::<AssetLoadProgress>().total_bytes,
                expected
            );
            app.update();
            assert_eq!(
                app.world().resource::<AssetLoadProgress>().loaded_bytes,
                expected
            );
        }
        app.world_mut().remove_resource::<Vfs>();
        app.update();
        assert_eq!(app.world().resource::<AssetLoadProgress>().total_bytes, 0);
    }

    #[test]
    fn unchanged_bridges_stay_quiet_but_elapsed_and_scene_edits_publish() {
        use bevy::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.init_resource::<AssetLoadProgress>();
        world.init_resource::<AssetProgressBridge>();
        world.init_resource::<SceneLoadBridge>();
        world.insert_resource(scene_io::SceneLoadState {
            phase: scene_io::SceneLoadPhase::Ready,
            current_path: Some("scene.ron".into()),
            progress: 1.0,
        });
        world.run_system_once(publish_asset_progress_to_bridge).unwrap();
        world.run_system_once(publish_scene_load_to_bridge).unwrap();
        for _ in 0..1000 {
            world.clear_trackers();
            world.run_system_once(publish_asset_progress_to_bridge).unwrap();
            world.run_system_once(publish_scene_load_to_bridge).unwrap();
            assert!(!world.is_resource_changed::<AssetProgressBridge>());
            assert!(!world.is_resource_changed::<SceneLoadBridge>());
        }
        world.resource_mut::<AssetLoadProgress>().elapsed_secs = 3.0;
        world.resource_mut::<scene_io::SceneLoadState>().current_path = None;
        world.clear_trackers();
        world.run_system_once(publish_asset_progress_to_bridge).unwrap();
        world.run_system_once(publish_scene_load_to_bridge).unwrap();
        assert!(world.is_resource_changed::<AssetProgressBridge>());
        assert!(world.is_resource_changed::<SceneLoadBridge>());
        assert_eq!(world.resource::<AssetProgressBridge>().snapshot.as_ref().unwrap().elapsed_secs, 3.0);
        assert!(world.resource::<SceneLoadBridge>().snapshot.as_ref().unwrap().current_path.is_none());
    }

    #[test]
    fn progress_retains_paths_without_changing_lifecycle_or_elapsed_time() {
        let mut app = App::new();
        app.init_resource::<AssetLoadProgress>()
            .init_resource::<AssetProgressBridge>()
            .init_resource::<SceneLoadBridge>()
            .init_resource::<Time>()
            .insert_resource(scene_io::SceneLoadState {
                phase: scene_io::SceneLoadPhase::Loading,
                current_path: Some("scene.ron".into()),
                progress: 0.25,
            })
            .add_systems(
                Update,
                (
                    tick_asset_load_progress,
                    publish_asset_progress_to_bridge,
                    publish_scene_load_to_bridge,
                )
                    .chain(),
            );
        let entity = app
            .world_mut()
            .spawn((
                MeshInstanceData {
                    model_path: Some("model.glb".into()),
                },
                scene_io::PendingMeshInstanceRehydrate(Handle::default()),
            ))
            .id();
        app.update();
        let pointers = |world: &World| {
            (
                world
                    .resource::<AssetLoadProgress>()
                    .current_path
                    .as_ref()
                    .unwrap()
                    .as_ptr(),
                world
                    .resource::<AssetProgressBridge>()
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .current_path
                    .as_ref()
                    .unwrap()
                    .as_ptr(),
                world
                    .resource::<SceneLoadBridge>()
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .current_path
                    .as_ref()
                    .unwrap()
                    .as_ptr(),
            )
        };
        let storage = pointers(app.world());
        for _ in 0..1_000 {
            app.world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_millis(10));
            app.update();
            assert_eq!(storage, pointers(app.world()));
            let progress = app.world().resource::<AssetLoadProgress>();
            assert!(progress.is_loading());
            assert_eq!(progress.total_files, 1);
            assert_eq!(progress.loaded_files, 0);
        }
        assert!(app.world().resource::<AssetLoadProgress>().elapsed_secs >= 9.99);
        app.world_mut()
            .get_mut::<MeshInstanceData>(entity)
            .unwrap()
            .model_path = Some("other.glb".into());
        app.update();
        assert_eq!(
            app.world()
                .resource::<AssetLoadProgress>()
                .current_path
                .as_deref(),
            Some("other.glb")
        );
        app.world_mut()
            .entity_mut(entity)
            .remove::<scene_io::PendingMeshInstanceRehydrate>();
        app.update();
        assert!(app.world().resource::<AssetLoadProgress>().is_done());
        assert!(app
            .world()
            .resource::<AssetProgressBridge>()
            .snapshot
            .as_ref()
            .unwrap()
            .current_path
            .is_none());
        let elapsed = app.world().resource::<AssetLoadProgress>().elapsed_secs;
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(1));
        app.update();
        assert!(app.world().resource::<AssetLoadProgress>().is_done());
        assert!(app.world().resource::<AssetLoadProgress>().elapsed_secs > elapsed);
        app.world_mut()
            .resource_mut::<scene_io::SceneLoadState>()
            .current_path = None;
        app.update();
        assert!(app
            .world()
            .resource::<SceneLoadBridge>()
            .snapshot
            .as_ref()
            .unwrap()
            .current_path
            .is_none());
    }
}
