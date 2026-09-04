//! Pure preparation of validated Tier 2 inputs into one build job.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use renzora::{EnginePluginGenerationStamp, ENGINE_PLUGIN_GENERATION_SCHEMA};

use crate::{
    discover_engine_plugins, engine_plugin_source_hash, load_and_validate_build_kit,
    materialize_overlay, BuildKitError, BuildKitRequirement, EngineBinaryTarget, EngineBuildJob,
    ManifestError, OverlayError,
};

/// Deployment-specific inputs needed to prepare an installed-editor build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineBuildPreparation {
    /// Project `plugins/` directory.
    pub plugins_root: PathBuf,
    /// Installed, version-matched build kit.
    pub build_kit_root: PathBuf,
    /// Per-user Tier 2 cache root.
    pub cache_root: PathBuf,
    /// Cargo executable selected for the approved toolchain.
    pub cargo: PathBuf,
    /// Exact kit identity required by the running editor.
    pub requirement: BuildKitRequirement,
    /// Canonical engine feature set.
    pub features: BTreeSet<String>,
    /// Runtime executable target.
    pub runtime: EngineBinaryTarget,
    /// Editor executable target.
    pub editor: EngineBinaryTarget,
}

/// Failure before Cargo is started.
#[derive(Debug, thiserror::Error)]
pub enum EngineBuildPreparationError {
    /// Plugin discovery or declaration validation failed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// Installed build-kit validation failed.
    #[error(transparent)]
    BuildKit(#[from] BuildKitError),
    /// Isolated overlay creation failed.
    #[error(transparent)]
    Overlay(#[from] OverlayError),
    /// A required immutable input could not be read.
    #[error("could not read build input {path}: {source}")]
    Input {
        /// Input path.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
}

/// Validate, hash, and snapshot all source inputs. This performs filesystem
/// work and belongs on a worker thread, never directly in an ECS system.
pub fn prepare_engine_build(
    preparation: &EngineBuildPreparation,
) -> Result<EngineBuildJob, EngineBuildPreparationError> {
    let declarations = discover_engine_plugins(&preparation.plugins_root)?;
    let kit = load_and_validate_build_kit(&preparation.build_kit_root, &preparation.requirement)?;
    let overlay = materialize_overlay(&kit, &declarations, &preparation.cache_root)?;
    let lockfile = overlay.root.join("Cargo.lock");
    let lockfile_bytes =
        fs::read(&lockfile).map_err(|source| EngineBuildPreparationError::Input {
            path: lockfile,
            source,
        })?;
    let plugins = declarations
        .iter()
        .map(|declaration| {
            engine_plugin_source_hash(declaration)
                .map(|hash| (declaration.manifest.id.clone(), hash))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let features = preparation.features.iter().cloned().collect::<Vec<_>>();
    let stamp = EnginePluginGenerationStamp {
        schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
        engine_build: kit.manifest.engine_build.clone(),
        build_kit_hash: kit.manifest.content_hash.clone(),
        target: kit.manifest.target.clone(),
        profile: kit.manifest.profile.clone(),
        toolchain_hash: blake3::hash(kit.manifest.toolchain_stamp.as_bytes())
            .to_hex()
            .to_string(),
        lockfile_hash: blake3::hash(&lockfile_bytes).to_hex().to_string(),
        integration_hash: overlay.integration_hash,
        features,
        plugins,
    };
    Ok(EngineBuildJob {
        workspace: overlay.root,
        cache_root: preparation.cache_root.clone(),
        cargo: preparation.cargo.clone(),
        target: kit.manifest.target,
        profile: kit.manifest.profile,
        features: preparation.features.clone(),
        runtime: Some(preparation.runtime.clone()),
        editor: Some(preparation.editor.clone()),
        stamp,
    })
}
