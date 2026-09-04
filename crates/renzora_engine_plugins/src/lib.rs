//! Restart-required engine-plugin builds.
//!
//! The pure manifest and generation layers are kept separate from the Bevy
//! adapter so path, staging, and process behavior can be tested without running
//! an editor. Later Phase 5 commits add those layers without changing this
//! explicit declaration contract.

pub mod build_kit;
pub mod builder;
pub mod ecs;
pub mod generation;
pub mod manifest;
pub mod overlay;
pub mod plan;

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
    load_candidate_generation, publish_generation, select_candidate_generation, stage_generation,
    GenerationArtifact, GenerationError, GenerationManifest, GenerationOutputs,
    PublishedEngineGeneration,
};
pub use manifest::{
    discover_engine_plugins, load_engine_plugin, EnginePluginDeclaration, ManifestError,
};
pub use overlay::{engine_plugin_source_hash, materialize_overlay, OverlayError, OverlayWorkspace};
pub use plan::{prepare_engine_build, EngineBuildPreparation, EngineBuildPreparationError};
