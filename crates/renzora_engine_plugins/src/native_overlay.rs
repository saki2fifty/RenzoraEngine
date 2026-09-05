//! Native replacement build configuration, applied only to copied workspaces.

use std::fs;
use std::path::Path;

use toml_edit::{value, Array, DocumentMut, Item};

use crate::OverlayError;

pub(crate) const NATIVE_OVERLAY_SCHEMA: &str = "renzora-native-overlay/v3";

pub(crate) fn configure_native_overlay(root: &Path) -> Result<(), OverlayError> {
    let root_path = root.join("Cargo.toml");
    let mut workspace = read(&root_path)?;
    // Generic overlay fixtures and non-engine workspaces have no editor graph.
    if workspace["package"]["name"].as_str() != Some("renzora_app") {
        return Ok(());
    }
    let editor_path = root.join("crates/renzora_editor/Cargo.toml");
    let app_path = root.join("crates/renzora_editor_app/Cargo.toml");
    let mut editor = read(&editor_path)?;
    let mut app = read(&app_path)?;
    configure(&mut workspace, &mut editor, &mut app).map_err(|message| {
        OverlayError::WorkspaceManifest {
            path: root_path.clone(),
            message,
        }
    })?;
    for (path, document) in [
        (root_path, workspace),
        (editor_path, editor),
        (app_path, app),
    ] {
        super::overlay::make_owner_writable(&path)?;
        fs::write(&path, document.to_string())
            .map_err(|source| OverlayError::Io { path, source })?;
    }
    crate::legacy_overlay::migrate(root)?;
    Ok(())
}

fn read(path: &Path) -> Result<DocumentMut, OverlayError> {
    let text = fs::read_to_string(path).map_err(|source| OverlayError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse().map_err(
        |error: toml_edit::TomlError| OverlayError::WorkspaceManifest {
            path: path.to_path_buf(),
            message: error.to_string(),
        },
    )
}

fn configure(
    workspace: &mut DocumentMut,
    editor: &mut DocumentMut,
    app: &mut DocumentMut,
) -> Result<(), String> {
    for (document, expected) in [(&*editor, "renzora_editor"), (&*app, "renzora_editor_app")] {
        if document["package"]["name"].as_str() != Some(expected) {
            return Err(format!("native overlay requires package {expected}"));
        }
    }
    let legacy_shared_image = editor
        .get("features")
        .and_then(|features| features.get("shared-image"))
        .is_some();
    let static_editor = editor.get("lib")
        .and_then(|library| library.get("crate-type"))
        .and_then(Item::as_array)
        .is_some_and(|formats| formats.len() == 1 && formats.get(0).and_then(|format| format.as_str()) == Some("rlib"));
    if !legacy_shared_image && !static_editor {
        return Err("build kit does not support the standalone editor feature split".into());
    }
    let binaries = app
        .get_mut("bin")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or("editor app has no binary declarations")?;
    let binary = binaries
        .iter_mut()
        .find(|binary| binary.get("name").and_then(Item::as_str) == Some("renzora-editor"))
        .ok_or("editor app has no renzora-editor binary")?;
    if let Some(required) = binary.get("required-features") {
        let features = required
            .as_array()
            .ok_or("invalid editor binary feature gate")?;
        if features
            .iter()
            .any(|feature| feature.as_str() != Some("wasm"))
        {
            return Err("unrecognized editor binary feature gate; update the build kit".into());
        }
    }
    binary.remove("required-features");
    let mut native_defaults = Array::new();
    if let Some(defaults) = app
        .get("features")
        .and_then(|features| features.get("default"))
        .and_then(Item::as_array)
    {
        for feature in defaults.iter() {
            if let Some(name) = feature.as_str() {
                if name != "tier2-native" {
                    native_defaults.push(name);
                }
            }
        }
    }
    native_defaults.push("tier2-native");
    app["features"]["default"] = value(native_defaults);
    remove_default(workspace, "dynamic_linking")?;
    remove_default(editor, "shared-image")?;
    let library = editor
        .get_mut("lib")
        .and_then(Item::as_table_mut)
        .ok_or("editor has no library declaration")?;
    let mut formats = Array::new();
    formats.push("rlib");
    library.insert("crate-type", value(formats));
    Ok(())
}

fn remove_default(document: &mut DocumentMut, excluded: &str) -> Result<(), String> {
    let defaults = document
        .get_mut("features")
        .and_then(|features| features.get_mut("default"))
        .and_then(Item::as_array_mut)
        .ok_or("package has no default feature list")?;
    let mut retained = Array::new();
    for feature in defaults.iter() {
        let name = feature.as_str().ok_or("non-string default feature")?;
        if name != excluded {
            retained.push(name);
        }
    }
    *defaults = retained;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifests() -> (DocumentMut, DocumentMut, DocumentMut) {
        (
            include_str!("../../../Cargo.toml")
                .parse()
                .expect("root manifest"),
            include_str!("../../renzora_editor/Cargo.toml")
                .parse()
                .expect("editor manifest"),
            include_str!("../../renzora_editor_app/Cargo.toml")
                .parse()
                .expect("app manifest"),
        )
    }

    #[test]
    fn actual_manifests_produce_native_pair_without_removing_runtime_capabilities() {
        let (mut root, mut editor, mut app) = manifests();
        let original_app = app.to_string();
        let runtime_features: Vec<_> = root["features"]["default"]
            .as_array()
            .expect("defaults")
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|v| *v != "dynamic_linking")
            .map(str::to_string)
            .collect();
        configure(&mut root, &mut editor, &mut app).expect("native graph");
        let configured: Vec<_> = root["features"]["default"]
            .as_array()
            .expect("defaults")
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect();
        assert_eq!(configured, runtime_features);
        assert_eq!(
            editor["lib"]["crate-type"]
                .as_array()
                .expect("formats")
                .len(),
            1
        );
        assert_eq!(editor["lib"]["crate-type"][0].as_str(), Some("rlib"));
        assert!(!editor["features"]["default"]
            .as_array()
            .expect("defaults")
            .iter()
            .any(|feature| feature.as_str() == Some("shared-image")));
        assert!(!original_app.contains("required-features = [\"wasm\"]"));
        assert!(app["bin"]
            .as_array_of_tables()
            .expect("bins")
            .iter()
            .all(|bin| !bin.contains_key("required-features")));
        let once = (root.to_string(), editor.to_string(), app.to_string());
        configure(&mut root, &mut editor, &mut app).expect("idempotent");
        assert_eq!(
            once,
            (root.to_string(), editor.to_string(), app.to_string())
        );
    }

    #[test]
    fn unknown_editor_gate_is_not_silently_removed() {
        let (mut root, mut editor, mut app) = manifests();
        let mut required = Array::new();
        required.push("future-required-capability");
        app["bin"]
            .as_array_of_tables_mut()
            .expect("bins")
            .get_mut(0)
            .expect("bin")
            .insert("required-features", value(required));
        assert!(configure(&mut root, &mut editor, &mut app).is_err());
    }

    #[test]
    fn unknown_library_format_is_not_silently_replaced() {
        let (mut root, mut editor, mut app) = manifests();
        let mut formats = Array::new();
        formats.push("cdylib");
        editor["lib"]["crate-type"] = value(formats);
        assert!(configure(&mut root, &mut editor, &mut app).is_err());
    }

    #[test]
    fn recognized_legacy_manifest_still_migrates() {
        let (mut root, mut editor, mut app) = manifests();
        editor["features"]["shared-image"] = value(Array::new());
        editor["features"]["default"]
            .as_array_mut()
            .expect("defaults")
            .push("shared-image");
        editor["lib"]["crate-type"]
            .as_array_mut()
            .expect("formats")
            .push("dylib");
        root["features"]["default"]
            .as_array_mut()
            .expect("defaults")
            .push("dynamic_linking");
        let mut required = Array::new();
        required.push("wasm");
        app["bin"]
            .as_array_of_tables_mut()
            .expect("bins")
            .get_mut(0)
            .expect("editor")
            .insert("required-features", value(required));
        configure(&mut root, &mut editor, &mut app).expect("recognized legacy graph");
        assert_eq!(editor["lib"]["crate-type"].as_array().expect("formats").len(), 1);
        assert!(!app["bin"]
            .as_array_of_tables()
            .expect("bins")
            .get(0)
            .expect("editor")
            .contains_key("required-features"));
    }
}
