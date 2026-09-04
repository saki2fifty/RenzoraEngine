//! Restart-required engine-plugin builds.
//!
//! The pure manifest and generation layers are kept separate from the Bevy
//! adapter so path, staging, and process behavior can be tested without running
//! an editor. Later Phase 5 commits add those layers without changing this
//! explicit declaration contract.

pub mod manifest;

pub use manifest::{
    discover_engine_plugins, load_engine_plugin, EnginePluginDeclaration, ManifestError,
};
