//! Explicit compatibility migration of the release's eleven native plugins.
//!
//! Only copied build-kit sources are transformed. Authored plugins and the
//! existing shared-image distribution retain their original loading path.

use std::fs;
use std::path::Path;

use toml_edit::{value, Array, DocumentMut, InlineTable};

use crate::OverlayError;

const PLUGINS: &[(&str, &str, &str)] = &[
    ("ai_chat", "AiChatPlugin", "Editor"),
    ("auto_exposure", "AutoExposurePlugin", "Runtime"),
    ("clouds", "CloudsPlugin", "Runtime"),
    ("gamepad", "GamepadPlugin", "Editor"),
    ("mesh_draw", "MeshDrawPlugin", "Editor"),
    ("night_stars", "NightStarsPlugin", "Runtime"),
    ("pool_water", "PoolWaterPlugin", "Runtime"),
    ("procedural_tree", "ProceduralTreePlugin", "Runtime"),
    ("spline", "SplinePlugin", "Runtime"),
    ("text3d", "Text3dPlugin", "Runtime"),
    ("vignette", "VignettePlugin", "Runtime"),
];

pub(crate) fn migrate(root: &Path) -> Result<(), OverlayError> {
    for &(name, plugin, scope) in PLUGINS {
        if migrated_workspace_plugin(root, name, plugin, scope)? {
            continue;
        }
        let source = root.join("plugins").join(name);
        let target = root.join("crates").join(format!("renzora_builtin_{name}"));
        if target.exists() {
            if fs::read_to_string(target.join(".renzora-native-migration"))
                .ok()
                .as_deref()
                == Some("v1")
            {
                continue;
            }
            return Err(OverlayError::Collision(target));
        }
        crate::overlay::copy_tree(&source, &target)?;
        let path = target.join("Cargo.toml");
        let text = fs::read_to_string(&path).map_err(|source| OverlayError::Io {
            path: path.clone(),
            source,
        })?;
        let mut manifest: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
            OverlayError::WorkspaceManifest {
                path: path.clone(),
                message: error.to_string(),
            }
        })?;
        manifest.remove("workspace");
        let mut formats = Array::new();
        formats.push("rlib");
        manifest["lib"]["crate-type"] = value(formats);
        let mut bevy = InlineTable::new();
        bevy.insert("workspace", true.into());
        manifest["dependencies"]["bevy"] = value(bevy);
        // Keep the package name: embedded shader paths use that namespace.
        fs::write(&path, manifest.to_string())
            .map_err(|source| OverlayError::Io { path, source })?;
        let path = target.join("src/lib.rs");
        let text = fs::read_to_string(&path).map_err(|source| OverlayError::Io {
            path: path.clone(),
            source,
        })?;
        let declaration = format!("renzora::plugin!({plugin}, {scope});");
        if text.lines().filter(|line| *line == declaration).count() != 1 {
            return Err(OverlayError::WorkspaceManifest {
                path,
                message: format!("review the {name} native declaration before migrating it"),
            });
        }
        let wrapper = format!(
            r#"#[derive(Default)]
pub struct Packaged{plugin};
impl bevy::app::Plugin for Packaged{plugin} {{
    fn build(&self, app: &mut bevy::app::App) {{
        let disabled = renzora::load_disabled_plugins().iter().any(|name| name == "{name}");
        if !disabled {{ app.add_plugins({plugin}::default()); }}
        renzora::record_plugin(app.world_mut(), "{name}", renzora::PluginKind::Native,
            if disabled {{ renzora::PluginState::Disabled }} else {{ renzora::PluginState::Loaded }});
    }}
}}
renzora::add!(Packaged{plugin}, {scope});"#
        );
        let updated = text
            .lines()
            .map(|line| {
                if line == declaration {
                    wrapper.as_str()
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, format!("{updated}\n"))
            .map_err(|source| OverlayError::Io { path, source })?;
        let path = target.join(".renzora-native-migration");
        fs::write(&path, "v1").map_err(|source| OverlayError::Io { path, source })?;
    }
    Ok(())
}

fn migrated_workspace_plugin(
    root: &Path,
    name: &str,
    plugin: &str,
    scope: &str,
) -> Result<bool, OverlayError> {
    let package = format!("renzora_{name}");
    let target = root.join("crates").join(&package);
    if !target.exists() {
        return Ok(false);
    }
    let path = target.join("Cargo.toml");
    let text = fs::read_to_string(&path).map_err(|source| OverlayError::Io {
        path: path.clone(),
        source,
    })?;
    let manifest: DocumentMut =
        text.parse().map_err(
            |error: toml_edit::TomlError| OverlayError::WorkspaceManifest {
                path: path.clone(),
                message: error.to_string(),
            },
        )?;
    let source_path = target.join("src/lib.rs");
    let source = fs::read_to_string(&source_path).map_err(|source| OverlayError::Io {
        path: source_path.clone(),
        source,
    })?;
    let declaration = format!("renzora::add!({plugin}, {scope});");
    let static_library = manifest
        .get("lib")
        .and_then(|lib| lib.get("crate-type"))
        .is_none_or(|formats| {
            formats.as_array().is_some_and(|formats| {
                formats.len() == 1
                    && formats.get(0).and_then(|format| format.as_str()) == Some("rlib")
            })
        });
    if manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        != Some(package.as_str())
        || !static_library
        || manifest.get("workspace").is_some()
        || root.join("plugins").join(name).join("Cargo.toml").exists()
        || source.lines().filter(|line| *line == declaration).count() != 1
    {
        return Err(OverlayError::WorkspaceManifest {
            path,
            message: format!("review the already-migrated {name} crate before packaging"),
        });
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrated_crate_must_be_static_and_must_not_have_a_legacy_twin() {
        let root = tempfile::tempdir().expect("fixture");
        let target = root.path().join("crates/renzora_spline");
        fs::create_dir_all(target.join("src")).expect("crate");
        fs::write(
            target.join("src/lib.rs"),
            "renzora::add!(SplinePlugin, Runtime);\n",
        )
        .expect("source");
        let manifest = "[package]\nname = 'renzora_spline'\n";
        fs::write(target.join("Cargo.toml"), manifest).expect("manifest");
        assert!(
            migrated_workspace_plugin(root.path(), "spline", "SplinePlugin", "Runtime")
                .expect("migrated")
        );
        fs::write(
            target.join("Cargo.toml"),
            format!("{manifest}[lib]\ncrate-type = ['dylib']\n"),
        )
        .expect("wrong format");
        assert!(
            migrated_workspace_plugin(root.path(), "spline", "SplinePlugin", "Runtime").is_err()
        );
        fs::write(target.join("Cargo.toml"), manifest).expect("restore manifest");
        fs::create_dir_all(root.path().join("plugins/spline")).expect("legacy directory");
        fs::write(root.path().join("plugins/spline/Cargo.toml"), manifest).expect("legacy twin");
        assert!(
            migrated_workspace_plugin(root.path(), "spline", "SplinePlugin", "Runtime").is_err()
        );
    }

    #[test]
    fn actual_first_party_declarations_match_the_explicit_migration_map() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(PLUGINS.len(), 11);
        for &(name, plugin, scope) in PLUGINS {
            if migrated_workspace_plugin(&repo, name, plugin, scope).expect("verified migration") {
                continue;
            }
            let source = fs::read_to_string(repo.join("plugins").join(name).join("src/lib.rs"))
                .expect("plugin source");
            assert_eq!(
                source
                    .lines()
                    .filter(|line| *line == format!("renzora::plugin!({plugin}, {scope});"))
                    .count(),
                1,
                "{name}"
            );
        }
    }
}
