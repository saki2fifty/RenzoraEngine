use std::fs;
use std::path::Path;

/// Source SDK packaging failed before publication.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// A source file could not be read or copied.
    #[error("Rust SDK I/O failure: {0}")]
    Io(#[from] std::io::Error),
    /// Input layout or manifest is unsupported.
    #[error("invalid Rust SDK inputs: {0}")]
    Invalid(String),
}

/// Package the three guest SDK source crates into a new directory.
///
/// Callers own atomic publication of the resulting directory. Existing output
/// is never overwritten, and symlinks are never followed.
pub fn package(engine_root: &Path, destination: &Path) -> Result<(), PackageError> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(PackageError::Invalid("destination already exists".into()));
    }
    let engine_root = fs::canonicalize(engine_root)?;
    let destination = fs::canonicalize(
        destination
            .parent()
            .ok_or_else(|| PackageError::Invalid("destination has no parent".into()))?,
    )?
    .join(
        destination
            .file_name()
            .ok_or_else(|| PackageError::Invalid("destination has no name".into()))?,
    );
    for name in [
        "renzora_plugin",
        "renzora_plugin_derive",
        "renzora_identity",
    ] {
        if destination.starts_with(engine_root.join("crates").join(name)) {
            return Err(PackageError::Invalid(
                "destination overlaps SDK source".into(),
            ));
        }
    }
    let destination = destination.as_path();
    let source: toml::Value = fs::read_to_string(engine_root.join("Cargo.toml"))?
        .parse()
        .map_err(|error: toml::de::Error| PackageError::Invalid(error.to_string()))?;
    let bevy = source
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|dependencies| dependencies.get("bevy"))
        .ok_or_else(|| PackageError::Invalid("missing optional host Bevy declaration".into()))?;
    let mut workspace = toml::map::Map::new();
    workspace.insert("resolver".into(), toml::Value::String("2".into()));
    workspace.insert(
        "members".into(),
        toml::Value::Array(vec![toml::Value::String("crates/*".into())]),
    );
    workspace.insert(
        "lints".into(),
        source
            .get("workspace")
            .and_then(|workspace| workspace.get("lints"))
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(Default::default())),
    );
    // Optional host metadata remains valid without enabling Bevy for guests.
    workspace.insert(
        "dependencies".into(),
        toml::Value::Table([("bevy".into(), bevy.clone())].into_iter().collect()),
    );
    let document = toml::Value::Table(
        [("workspace".into(), toml::Value::Table(workspace))]
            .into_iter()
            .collect(),
    );
    fs::create_dir(destination)?;
    fs::create_dir(destination.join("crates"))?;
    for name in [
        "renzora_plugin",
        "renzora_plugin_derive",
        "renzora_identity",
    ] {
        let target = destination.join("crates").join(name);
        fs::create_dir(&target)?;
        for item in ["src", "Cargo.toml"] {
            copy(
                &engine_root.join("crates").join(name).join(item),
                &target.join(item),
            )?;
        }
    }
    fs::write(
        destination.join("Cargo.toml"),
        toml::to_string(&document).map_err(|error| PackageError::Invalid(error.to_string()))?,
    )?;
    for license in ["LICENSE-MIT", "LICENSE-APACHE"] {
        copy(&engine_root.join(license), &destination.join(license))?;
    }
    fs::write(
        destination
            .join("crates/renzora_plugin")
            .join(crate::CONTENT_ROOT_MARKER),
        crate::CONTENT_ROOT_LAYOUT,
    )?;
    Ok(())
}

/// Refresh an installation's source SDK without mixing old and new files.
pub fn stage(engine_root: &Path, installation: &Path) -> Result<(), PackageError> {
    fs::create_dir_all(installation)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| PackageError::Invalid(error.to_string()))?
        .as_nanos();
    let suffix = format!("{}-{nonce}", std::process::id());
    let pending = installation.join(format!(".rust-sdk-pending-{suffix}"));
    let previous = installation.join(format!(".rust-sdk-previous-{suffix}"));
    let active = installation.join("rust-sdk");
    let had_previous = match fs::symlink_metadata(&active) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || fs::read(
                    active
                        .join("crates/renzora_plugin")
                        .join(crate::CONTENT_ROOT_MARKER),
                )? != crate::CONTENT_ROOT_LAYOUT
            {
                return Err(PackageError::Invalid(
                    "refusing to replace an unrecognized source SDK".into(),
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = package(engine_root, &pending) {
        if pending.is_dir() {
            let _ = fs::remove_dir_all(&pending);
        }
        return Err(error);
    }
    if had_previous {
        if let Err(error) = fs::rename(&active, &previous) {
            let _ = fs::remove_dir_all(&pending);
            return Err(error.into());
        }
    }
    if let Err(error) = fs::rename(&pending, &active) {
        if had_previous {
            let _ = fs::rename(&previous, &active);
        }
        let _ = fs::remove_dir_all(&pending);
        return Err(error.into());
    }
    if had_previous {
        fs::remove_dir_all(previous)?;
    }
    Ok(())
}

fn copy(source: &Path, destination: &Path) -> Result<(), PackageError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(PackageError::Invalid(format!(
            "symlink input {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
                continue;
            }
            copy(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        fs::copy(source, destination)?;
    } else {
        return Err(PackageError::Invalid(format!(
            "non-regular input {}",
            source.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_real_sdk_and_refuses_to_overwrite_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates")
            .parent()
            .expect("engine");
        let temp = tempfile::tempdir().expect("temp");
        let destination = temp.path().join("sdk");
        package(root, &destination).expect("package");
        assert!(destination
            .join("crates/renzora_identity/src/lib.rs")
            .is_file());
        assert!(destination
            .join("crates/renzora_plugin_derive/src/lib.rs")
            .is_file());
        assert_eq!(
            fs::read(
                destination
                    .join("crates/renzora_plugin")
                    .join(crate::CONTENT_ROOT_MARKER)
            )
            .expect("marker"),
            crate::CONTENT_ROOT_LAYOUT
        );
        assert!(package(root, &destination).is_err());
        assert!(!destination.join("target").exists());
    }

    #[test]
    fn staging_replaces_only_recognized_sdk_and_cleans_old_copy() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates")
            .parent()
            .expect("engine");
        let temp = tempfile::tempdir().expect("installation");
        stage(root, temp.path()).expect("first stage");
        fs::write(temp.path().join("rust-sdk/obsolete"), "retired").expect("old file");
        stage(root, temp.path()).expect("refresh");
        assert!(!temp.path().join("rust-sdk/obsolete").exists());
        assert_eq!(fs::read_dir(temp.path()).expect("installation").count(), 1);
        fs::remove_file(
            temp.path()
                .join("rust-sdk/crates/renzora_plugin")
                .join(crate::CONTENT_ROOT_MARKER),
        )
        .expect("remove marker");
        assert!(stage(root, temp.path()).is_err());
        assert!(temp.path().join("rust-sdk/Cargo.toml").is_file());
    }
}
