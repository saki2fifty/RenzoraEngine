//! Editor-side wiring of the shared `BuildService`.
//!
//! Phase 4 requires that loose plugins and Rust scripts submit to the
//! SAME `Arc<BuildService>` so the cache root, worker pool, supervisor,
//! staging, and supersession are exactly one instance. The editor
//! constructs the service once and hands the `Arc` to both consumers
//! through Bevy resources; this module provides the two thin
//! resource types that carry that shared service through the
//! generated `RustScriptPlugin` installation.

use std::sync::Arc;

use bevy::prelude::Resource;
use renzora_compiler_cache::BuildService;

/// The `Arc<BuildService>` the editor hands to `renzora_rust_script`.
///
/// Constructed once by the editor entry point and installed as a
/// Bevy resource BEFORE `renzora_runtime::add_engine_plugins` runs.
/// The generated `RustScriptPlugin` installation reads this resource
/// during `build` and promotes it into a `RustScriptBuildService`
/// the lifecycle systems submit through.
///
/// In a runtime build with no editor (a shipped game), the resource
/// is absent and the plugin's lifecycle systems are no-ops — the
/// runtime does not watch source, so there is nothing to compile.
#[derive(Resource, Clone)]
pub struct RustScriptSharedService(pub Arc<BuildService>);

/// The `Arc<BuildService>` the script lifecycle actually submits
/// to. Created by `RustScriptPlugin::build` from the
/// [`RustScriptSharedService`] the editor installed. Tests that
/// drive the production lifecycle directly install this resource
/// (with a real `BuildService` they constructed) so the production
/// `lifecycle::watch` and `lifecycle::activate` systems are the
/// ones under test.
#[derive(Resource, Clone)]
pub struct RustScriptBuildService(pub Arc<BuildService>);

impl std::ops::Deref for RustScriptBuildService {
    type Target = BuildService;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
