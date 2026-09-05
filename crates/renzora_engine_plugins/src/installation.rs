//! Resolve installed build inputs without relying on a source checkout.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Mutex};

use bevy::prelude::*;

use renzora::EnginePluginGenerationStamp;

use crate::{BuildKitManifest, BuildKitRequirement, EngineBinaryTarget, EngineBuildPreparation};

#[derive(Resource, Default)]
pub(crate) struct PendingInstallation(
    Mutex<Option<mpsc::Receiver<Result<EngineBuildPreparation, String>>>>,
);

pub(crate) fn begin(
    running: Option<Res<renzora::EnginePluginRunningGeneration>>,
    config: Option<Res<crate::EnginePluginEcsConfig>>,
    pending: Res<PendingInstallation>,
) {
    if config.is_some() {
        return;
    }
    let Some(stamp) = running.and_then(|running| running.0.clone()) else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    if let Ok(mut pending) = pending.0.lock() {
        *pending = Some(receiver);
    } else {
        return;
    }
    let result_sender = sender.clone();
    let spawn = std::thread::Builder::new()
        .name("engine_plugins.installation".into())
        .spawn(move || {
            let result = (|| {
                let executable = std::env::current_exe().map_err(|error| error.to_string())?;
                let kit = std::env::args_os()
                    .skip_while(|argument| argument != "--engine-build-kit")
                    .nth(1)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        executable
                            .parent()
                            .expect("absolute executable parent")
                            .join("engine-build-kit")
                    });
                load(&kit, &cache_root(&stamp)?, &stamp)
            })();
            let _ = result_sender.send(result);
        });
    if let Err(error) = spawn {
        let _ = sender.send(Err(format!(
            "Could not start installation validation: {error}"
        )));
    }
}

pub(crate) fn drain(
    mut commands: Commands,
    pending: Res<PendingInstallation>,
    mut requests: MessageWriter<renzora::EnginePluginBuildRequest>,
    mut diagnostics: ResMut<renzora::EnginePluginDiagnostics>,
) {
    let Ok(mut pending) = pending.0.lock() else {
        return;
    };
    let Some(receiver) = pending.as_ref() else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => {
            Err("Installation validation stopped unexpectedly".into())
        }
    };
    *pending = None;
    match result {
        Ok(preparation) => {
            commands.insert_resource(crate::EnginePluginEcsConfig { preparation });
            requests.write(renzora::EnginePluginBuildRequest {
                plugin_id: None,
                reason: renzora::EnginePluginBuildReason::Reconcile,
            });
        }
        Err(message) => diagnostics.push(renzora::EnginePluginDiagnostic {
            plugin_id: None,
            level: renzora::EnginePluginDiagnosticLevel::Error,
            message,
            source: None,
        }),
    }
}

/// Resolve and pin installation inputs on a worker, before any project build.
pub fn load(
    kit_root: &Path,
    cache_root: &Path,
    running: &EnginePluginGenerationStamp,
) -> Result<EngineBuildPreparation, String> {
    let kit_root = std::fs::canonicalize(kit_root)
        .map_err(|error| format!("Engine build kit is missing: {error}"))?;
    if !cache_root.is_absolute() {
        return Err("Engine build cache must be an absolute path".into());
    }
    let manifest: BuildKitManifest = toml::from_str(
        &std::fs::read_to_string(kit_root.join("build-kit.toml"))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Invalid engine build kit: {error}"))?;
    validate_identity(&manifest, running)?;
    let release = manifest
        .toolchain_stamp
        .lines()
        .find_map(|line| line.strip_prefix("release: "))
        .ok_or("Build kit has no Rust release stamp")?;
    let host = manifest
        .toolchain_stamp
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("Build kit has no Rust host stamp")?;
    let selected = Command::new("rustup")
        .args([
            "which",
            "--toolchain",
            &format!("{release}-{host}"),
            "rustc",
        ])
        .output()
        .map_err(|error| format!("Rust toolchain manager is unavailable: {error}"))?;
    if !selected.status.success() {
        return Err(format!(
            "Install Rust {release} for {host} to build engine plugins: {}",
            String::from_utf8_lossy(&selected.stderr)
        ));
    }
    let rustc = PathBuf::from(
        String::from_utf8(selected.stdout)
            .map_err(|_| "Rust compiler path is not UTF-8")?
            .trim(),
    );
    if !rustc.is_absolute() {
        return Err("Rust toolchain manager returned a relative compiler path".into());
    }
    let output = Command::new(&rustc)
        .arg("-vV")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() || output.stdout != manifest.toolchain_stamp.as_bytes() {
        return Err("Installed Rust compiler does not match this editor's build kit".into());
    }
    let cargo = rustc
        .parent()
        .ok_or("Rust compiler has no directory")?
        .join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    if !cargo.is_file() {
        return Err("Pinned Rust toolchain has no Cargo executable".into());
    }
    let suffix = if manifest.target.split('-').any(|part| part == "windows") {
        ".exe"
    } else {
        ""
    };
    Ok(EngineBuildPreparation {
        plugins_root: PathBuf::new(),
        build_kit_root: kit_root,
        expected_build_kit_hash: running.build_kit_hash.clone(),
        cache_root: cache_root.to_path_buf(),
        cargo,
        rustc,
        requirement: BuildKitRequirement {
            engine_build: manifest.engine_build,
            target: manifest.target,
            profile: manifest.profile,
            toolchain_stamp: manifest.toolchain_stamp,
        },
        features: running.features.iter().cloned().collect(),
        runtime: EngineBinaryTarget {
            package: "renzora_app".into(),
            binary: "renzora".into(),
            output_name: format!("renzora{suffix}"),
        },
        editor: EngineBinaryTarget {
            package: "renzora_editor_app".into(),
            binary: "renzora-editor".into(),
            output_name: format!("renzora-editor{suffix}"),
        },
    })
}

fn validate_identity(
    manifest: &BuildKitManifest,
    running: &EnginePluginGenerationStamp,
) -> Result<(), String> {
    if running.schema != renzora::ENGINE_PLUGIN_GENERATION_SCHEMA
        || manifest.schema != crate::BUILD_KIT_MANIFEST_SCHEMA
        || manifest.content_hash != running.build_kit_hash
        || manifest.engine_build != running.engine_build
        || manifest.target != running.target
        || manifest.profile != running.profile
        || blake3::hash(manifest.toolchain_stamp.as_bytes())
            .to_hex()
            .as_str()
            != running.toolchain_hash
    {
        return Err(
            "Engine build kit does not belong to the running editor; repair the installation"
                .into(),
        );
    }
    Ok(())
}

/// Per-user cache location, independent of a potentially read-only installation.
pub fn cache_root(stamp: &EnginePluginGenerationStamp) -> Result<PathBuf, String> {
    if stamp.build_kit_hash.len() != 64
        || !stamp
            .build_kit_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid embedded build-kit identity".into());
    }
    dirs::cache_dir()
        .map(|root| {
            root.join("renzora/engine-plugins")
                .join(&stamp.build_kit_hash)
        })
        .ok_or_else(|| "Operating system did not provide a user cache directory".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identities() -> (BuildKitManifest, EnginePluginGenerationStamp) {
        let manifest = BuildKitManifest {
            schema: crate::BUILD_KIT_MANIFEST_SCHEMA,
            engine_build: "installed-editor".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "dist".into(),
            toolchain_stamp: "pinned compiler".into(),
            files: Vec::new(),
            content_hash: "a".repeat(64),
        };
        let stamp = EnginePluginGenerationStamp {
            schema: renzora::ENGINE_PLUGIN_GENERATION_SCHEMA,
            engine_build: manifest.engine_build.clone(),
            target: manifest.target.clone(),
            profile: manifest.profile.clone(),
            build_kit_hash: manifest.content_hash.clone(),
            toolchain_hash: blake3::hash(manifest.toolchain_stamp.as_bytes())
                .to_hex()
                .to_string(),
            ..Default::default()
        };
        (manifest, stamp)
    }

    #[test]
    fn self_consistent_other_kit_cannot_replace_the_embedded_identity() {
        let (mut manifest, stamp) = identities();
        assert!(validate_identity(&manifest, &stamp).is_ok());
        manifest.content_hash = "b".repeat(64);
        assert!(validate_identity(&manifest, &stamp).is_err());
    }

    #[test]
    fn unsupported_running_identity_and_changed_compiler_are_rejected() {
        let (mut manifest, mut stamp) = identities();
        stamp.schema += 1;
        assert!(validate_identity(&manifest, &stamp).is_err());
        stamp.schema -= 1;
        manifest.toolchain_stamp.push_str(" changed");
        assert!(validate_identity(&manifest, &stamp).is_err());
        stamp.build_kit_hash = "../escape".into();
        assert!(cache_root(&stamp).is_err());
    }
}
