//! Restart-required engine-plugin builds.
//!
//! The pure manifest and generation layers are kept separate from the Bevy
//! adapter so path, staging, and process behavior can be tested without running
//! an editor. Native build preparation and startup confirmation remain separate
//! from production editor installation and user-interaction policy.

pub mod build_kit;
pub mod builder;
pub mod ecs;
pub mod generation;
pub mod installation;
mod lockfile;
mod legacy_overlay;
pub mod manifest;
mod native_overlay;
pub mod overlay;
pub mod packaging;
pub mod plan;
pub mod replacement;
pub mod startup;
mod working_copy;
mod watch;

pub use build_kit::{
    create_build_kit_manifest, load_and_validate_build_kit, BuildKit, BuildKitError, BuildKitFile,
    BuildKitManifest, BuildKitRequirement, BUILD_KIT_MANIFEST_SCHEMA,
};
pub use builder::{
    EngineBinaryTarget, EngineBuildEvent, EngineBuildJob, EngineBuildService,
    EngineBuildServiceError,
};
pub use ecs::{EnginePluginEcsConfig, EnginePluginEcsPlugin};
pub use generation::{
    load_candidate_generation, load_engine_generation, publish_generation,
    select_candidate_generation, stage_generation, stage_generation_with_support,
    GenerationArtifact, GenerationError, GenerationManifest, GenerationOutputs,
    GenerationSupportFile, PublishedEngineGeneration,
};
pub use manifest::{
    discover_engine_plugins, load_engine_plugin, EnginePluginDeclaration, ManifestError,
};
pub use overlay::{engine_plugin_source_hash, materialize_overlay, OverlayError, OverlayWorkspace};
pub use plan::{prepare_engine_build, EngineBuildPreparation, EngineBuildPreparationError};

renzora::add!(EnginePluginEcsPlugin, Editor);

mod restart;
