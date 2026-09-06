//! Thread-local handlers for script `get` calls.
//!
//! The execution system sets these before calling a script, providing
//! closures that read reflected component data from the world.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::Resource;

use crate::command::PropertyValue;

/// Snapshot of asset-load progress, decoupled from `renzora_engine` so this
/// crate doesn't pull engine types into its public API. The engine's
/// `tick_asset_load_progress` system writes one of these into
/// [`AssetProgressBridge`] every frame, and the script execution loop
/// stashes it for `asset_progress()` reads.
#[derive(Clone, Debug, Default)]
pub struct AssetProgressSnapshot {
    /// Lifecycle state encoded as a string the script can match on:
    /// `"idle"`, `"loading"`, or `"done"`.
    pub state: &'static str,
    pub total_files: u32,
    pub loaded_files: u32,
    pub total_bytes: u64,
    pub loaded_bytes: u64,
    pub current_path: Option<String>,
    pub elapsed_secs: f32,
    /// Best-effort `[0.0, 1.0]` fraction. Mirrors `AssetLoadProgress::fraction`.
    pub fraction: f32,
}

/// Bevy resource that decouples `renzora_engine`'s asset-load tracker from
/// this crate's script execution loop. `renzora_engine` depends on
/// `renzora_scripting`, not the other way around, so the engine writes into
/// this bridge and scripting reads from it without the dep cycle.
#[derive(Resource, Default, Clone, Debug)]
pub struct AssetProgressBridge {
    pub snapshot: Option<AssetProgressSnapshot>,
}

/// Snapshot of scene-load state, decoupled from `renzora_engine` for the same
/// reason as [`AssetProgressSnapshot`] — the dependency runs engine → scripting.
///
/// Answers "which scene, how far through spawning", which is a different
/// question from [`AssetProgressSnapshot`]'s "how many models have finished".
/// A scene can be fully spawned (`ready`) while its models are still streaming.
#[derive(Clone, Debug, Default)]
pub struct SceneLoadSnapshot {
    /// `"idle"`, `"loading"`, `"ready"` or `"failed"`.
    pub phase: &'static str,
    pub current_path: Option<String>,
    pub progress: f32,
}

/// Bridge resource for [`SceneLoadSnapshot`]; see [`AssetProgressBridge`].
#[derive(Resource, Default, Clone, Debug)]
pub struct SceneLoadBridge {
    pub snapshot: Option<SceneLoadSnapshot>,
}

/// Signature for the get-field handler: (entity_name, component_type, field_path) → Option<PropertyValue>.
type GetFn = Box<dyn Fn(Option<&str>, &str, &str) -> Option<PropertyValue>>;

/// Signature for get-component handler: (entity_name, component_type) → Option<HashMap<field, value>>.
type GetComponentFn = Box<dyn Fn(Option<&str>, &str) -> Option<HashMap<String, PropertyValue>>>;

/// Signature for get-components handler: (entity_name) → Vec<component_type_name>.
type GetComponentsFn = Box<dyn Fn(Option<&str>) -> Vec<String>>;

thread_local! {
    static GET_HANDLER: RefCell<Option<GetFn>> = RefCell::new(None);
    static GET_COMPONENT_HANDLER: RefCell<Option<GetComponentFn>> = RefCell::new(None);
    static GET_COMPONENTS_HANDLER: RefCell<Option<GetComponentsFn>> = RefCell::new(None);
    /// Latest asset-load progress, refreshed by the execution system before
    /// each script tick and cleared after. `None` when no progress data is
    /// available (e.g. running outside the standard scene-load pipeline).
    static ASSET_PROGRESS: RefCell<Option<Arc<AssetProgressSnapshot>>> = const { RefCell::new(None) };
    /// Latest scene-load state, refreshed alongside [`ASSET_PROGRESS`].
    static SCENE_LOAD: RefCell<Option<Arc<SceneLoadSnapshot>>> = const { RefCell::new(None) };
}

/// Set the get-field handler for the current script execution.
pub fn set_get_handler(handler: GetFn) {
    GET_HANDLER.with(|h| *h.borrow_mut() = Some(handler));
}

/// Set the get-component handler for the current script execution.
pub fn set_get_component_handler(handler: GetComponentFn) {
    GET_COMPONENT_HANDLER.with(|h| *h.borrow_mut() = Some(handler));
}

/// Set the get-components handler for the current script execution.
pub fn set_get_components_handler(handler: GetComponentsFn) {
    GET_COMPONENTS_HANDLER.with(|h| *h.borrow_mut() = Some(handler));
}

/// Clear all handlers after script execution.
pub fn clear_get_handler() {
    GET_HANDLER.with(|h| *h.borrow_mut() = None);
    GET_COMPONENT_HANDLER.with(|h| *h.borrow_mut() = None);
    GET_COMPONENTS_HANDLER.with(|h| *h.borrow_mut() = None);
    ASSET_PROGRESS.with(|p| *p.borrow_mut() = None);
    SCENE_LOAD.with(|p| *p.borrow_mut() = None);
}

/// Stash the current asset-load progress for the script that's about to run.
pub fn set_asset_progress(snapshot: AssetProgressSnapshot) {
    ASSET_PROGRESS.with(|p| *p.borrow_mut() = Some(Arc::new(snapshot)));
}

/// Read the asset-load progress snapshot stashed for this script tick.
pub fn call_asset_progress() -> Option<AssetProgressSnapshot> {
    ASSET_PROGRESS.with(|p| p.borrow().as_deref().cloned())
}

/// Stash the current scene-load state for the script that's about to run.
pub fn set_scene_load(snapshot: SceneLoadSnapshot) {
    SCENE_LOAD.with(|p| *p.borrow_mut() = Some(Arc::new(snapshot)));
}

/// Read the scene-load snapshot stashed for this script tick.
pub fn call_scene_load() -> Option<SceneLoadSnapshot> {
    SCENE_LOAD.with(|p| p.borrow().as_deref().cloned())
}

pub(crate) fn set_shared_load_snapshots(
    asset: Option<&Arc<AssetProgressSnapshot>>,
    scene: Option<&Arc<SceneLoadSnapshot>>,
) {
    // Reset absent values too: a custom backend may have installed its own
    // snapshot during the previous entry. Never leak that state to the next.
    ASSET_PROGRESS.with(|p| *p.borrow_mut() = asset.cloned());
    SCENE_LOAD.with(|p| *p.borrow_mut() = scene.cloned());
}

/// Read a single field from a component.
pub fn call_get(
    entity_name: Option<&str>,
    component_type: &str,
    field_path: &str,
) -> Option<PropertyValue> {
    GET_HANDLER.with(|h| {
        let borrow = h.borrow();
        borrow
            .as_ref()
            .and_then(|f| f(entity_name, component_type, field_path))
    })
}

/// Read all fields from a component as a HashMap.
pub fn call_get_component(
    entity_name: Option<&str>,
    component_type: &str,
) -> Option<HashMap<String, PropertyValue>> {
    GET_COMPONENT_HANDLER.with(|h| {
        let borrow = h.borrow();
        borrow.as_ref().and_then(|f| f(entity_name, component_type))
    })
}

/// List all reflected component type names on an entity.
pub fn call_get_components(entity_name: Option<&str>) -> Vec<String> {
    GET_COMPONENTS_HANDLER.with(|h| {
        let borrow = h.borrow();
        borrow.as_ref().map(|f| f(entity_name)).unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_snapshots_share_storage_but_public_reads_remain_owned() {
        let asset = Arc::new(AssetProgressSnapshot {
            current_path: Some("models/example.glb".into()),
            ..Default::default()
        });
        let scene = Arc::new(SceneLoadSnapshot {
            current_path: Some("scenes/example.ron".into()),
            ..Default::default()
        });
        for _ in 0..1000 {
            set_shared_load_snapshots(Some(&asset), Some(&scene));
            ASSET_PROGRESS
                .with(|value| assert!(Arc::ptr_eq(value.borrow().as_ref().unwrap(), &asset)));
            SCENE_LOAD.with(|value| assert!(Arc::ptr_eq(value.borrow().as_ref().unwrap(), &scene)));
            let mut owned = call_asset_progress().unwrap();
            owned.current_path.as_mut().unwrap().clear();
            assert_eq!(
                call_asset_progress().unwrap().current_path,
                asset.current_path
            );
            clear_get_handler();
            assert!(call_asset_progress().is_none());
            assert!(call_scene_load().is_none());
        }
        set_asset_progress(AssetProgressSnapshot::default());
        set_scene_load(SceneLoadSnapshot::default());
        set_shared_load_snapshots(None, None);
        assert!(call_asset_progress().is_none());
        assert!(call_scene_load().is_none());
        assert_eq!(Arc::strong_count(&asset), 1);
        assert_eq!(Arc::strong_count(&scene), 1);
    }
}
