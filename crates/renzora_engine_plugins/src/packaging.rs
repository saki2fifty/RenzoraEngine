//! Release-side assembly of immutable installed-editor build inputs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use toml_edit::{value, DocumentMut, Table};

use crate::{create_build_kit_manifest, BuildKitError, BuildKitManifest, BuildKitRequirement};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Copy validated build sources into a new caller-owned editing workspace.
/// The source and destination must not overlap; links and special files fail closed.
pub fn copy_build_sources(source: &Path, destination: &Path) -> Result<(), BuildKitPackageError> {
    let source = fs::canonicalize(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| BuildKitPackageError::Invalid("destination has no parent".into()))?;
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(&source) || fs::symlink_metadata(destination).is_ok() {
        return Err(BuildKitPackageError::Invalid(
            "source copy must be new and outside the snapshot".into(),
        ));
    }
    copy_tree(&source, destination, true)
}

const SOURCE_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "build.rs",
    ".cargo",
    "src",
    "crates",
    "xtask",
    "assets",
    "plugins",
    "languages",
    "templates",
    "icon.ico",
    "icon.png",
    "LICENSE-APACHE",
    "LICENSE-MIT",
];

/// Prepare release sources before resolving and vendoring their full native graph.
pub fn prepare_build_kit_workspace(
    engine_root: &Path,
    destination: &Path,
) -> Result<(), BuildKitPackageError> {
    if fs::symlink_metadata(destination).is_ok()
        || destination.starts_with(fs::canonicalize(engine_root)?)
    {
        return Err(BuildKitPackageError::Invalid(
            "prepared workspace must be new and outside the engine source".into(),
        ));
    }
    copy_workspace(engine_root, destination)?;
    crate::native_overlay::configure_native_overlay(destination)
        .map_err(|error| BuildKitPackageError::Invalid(error.to_string()))
}

/// Reject release resolution that changes existing engine dependency pins.
pub fn verify_build_kit_lockfile(
    baseline: &str,
    resolved: &str,
) -> Result<(), BuildKitPackageError> {
    crate::lockfile::verify_extension(baseline, resolved).map_err(BuildKitPackageError::Invalid)
}

fn copy_workspace(engine_root: &Path, destination: &Path) -> Result<(), BuildKitPackageError> {
    fs::create_dir(destination)?;
    for name in SOURCE_INPUTS {
        let source = engine_root.join(name);
        if source.exists() {
            copy_tree(&source, &destination.join(name), true)?;
        }
    }
    Ok(())
}

/// Package the source SDK used by the shared Tier 1 compiler.
///
/// This is separate from the legacy metadata SDK: only these three source
/// crates are copied, never compiled Bevy artifacts or a developer target tree.
/// The caller publishes the surrounding runtime inventory atomically.
pub fn package_rust_sdk(
    engine_root: &Path,
    destination: &Path,
) -> Result<(), BuildKitPackageError> {
    renzora_rust_sdk::package(engine_root, destination)
        .map_err(|error| BuildKitPackageError::Invalid(error.to_string()))
}

/// Inputs owned by the release packager, never the running editor's project.
pub struct BuildKitPackageInputs {
    /// Engine checkout whose manifests and source form the kit.
    pub engine_root: PathBuf,
    /// Versioned dependency directory produced by `cargo vendor --locked`.
    pub vendor_root: PathBuf,
    /// Reviewed companion-file tree, relative to the future executable pair.
    pub runtime_root: PathBuf,
    /// New destination; an existing kit is never overwritten.
    pub destination: PathBuf,
    /// Exact identity of the intended release and toolchain.
    pub requirement: BuildKitRequirement,
}

/// Build-kit packaging failure.
#[derive(Debug, thiserror::Error)]
pub enum BuildKitPackageError {
    /// Source copy or publication failed.
    #[error("could not package build kit: {0}")]
    Io(#[from] std::io::Error),
    /// File inventory validation failed.
    #[error(transparent)]
    Inventory(#[from] BuildKitError),
    /// Inputs cannot be packaged without guessing or changing their semantics.
    #[error("invalid build-kit packaging inputs: {0}")]
    Invalid(String),
}

/// Assemble a new self-contained source/dependency kit without editing inputs.
///
/// This does not install a compiler or prove native system-library availability.
/// Release validation must build and launch the packaged editor separately.
pub fn package_build_kit(
    inputs: &BuildKitPackageInputs,
) -> Result<BuildKitManifest, BuildKitPackageError> {
    if fs::symlink_metadata(&inputs.destination).is_ok() {
        return Err(BuildKitPackageError::Invalid(
            "destination already exists".into(),
        ));
    }
    let parent = inputs
        .destination
        .parent()
        .ok_or_else(|| BuildKitPackageError::Invalid("destination has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    for source in [
        &inputs.engine_root,
        &inputs.vendor_root,
        &inputs.runtime_root,
    ] {
        if parent.starts_with(fs::canonicalize(source)?) {
            return Err(BuildKitPackageError::Invalid(
                "destination must be outside all source trees".into(),
            ));
        }
    }
    let temp = parent.join(format!(
        ".renzora-kit.{}.{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&temp)?;
    let result = assemble(inputs, &temp).and_then(|manifest| {
        // The destination must remain new; never replace an existing release.
        if fs::symlink_metadata(&inputs.destination).is_ok() {
            return Err(BuildKitPackageError::Invalid(
                "destination appeared during packaging".into(),
            ));
        }
        fs::rename(&temp, &inputs.destination)?;
        Ok(manifest)
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

fn assemble(
    inputs: &BuildKitPackageInputs,
    temp: &Path,
) -> Result<BuildKitManifest, BuildKitPackageError> {
    let workspace = temp.join("workspace");
    // Explicit release inputs exclude developer caches, Git state and dist.
    // Nested target directories are excluded too (plugins own workspaces).
    copy_workspace(&inputs.engine_root, &workspace)?;
    for required in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "xtask/Cargo.toml",
        "xtask/Cargo.lock",
    ] {
        if !workspace.join(required).is_file() {
            return Err(BuildKitPackageError::Invalid(format!(
                "missing source input {required}"
            )));
        }
    }
    copy_tree(&inputs.vendor_root, &workspace.join("vendor"), false)?;
    // A previously launched staging directory may contain mutable reload
    // shadows and compiler caches. They are not release dependencies.
    copy_tree(&inputs.runtime_root, &temp.join("runtime"), true)?;
    renzora_native_build::artwork::stage(&inputs.engine_root, &temp.join("runtime"))?;
    let config_path = workspace.join(".cargo/config.toml");
    fs::create_dir_all(config_path.parent().expect("Cargo config parent"))?;
    let existing = match fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut config: DocumentMut = existing
        .parse()
        .map_err(|error: toml_edit::TomlError| BuildKitPackageError::Invalid(error.to_string()))?;
    if config.contains_key("source") {
        return Err(BuildKitPackageError::Invalid(
            "existing Cargo source replacement requires an explicit packaging adapter".into(),
        ));
    }
    // The repository currently has no git-sourced lock entries. Fail closed if
    // that changes: each git source needs its own cargo-vendor replacement.
    for lock in ["Cargo.lock", "xtask/Cargo.lock"] {
        let document: toml::Value = fs::read_to_string(workspace.join(lock))?
            .parse()
            .map_err(|error: toml::de::Error| BuildKitPackageError::Invalid(error.to_string()))?;
        if document
            .get("package")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|package| package.get("source").and_then(toml::Value::as_str))
            .any(|source| source != "registry+https://github.com/rust-lang/crates.io-index")
        {
            return Err(BuildKitPackageError::Invalid(
                "non-crates.io dependency source needs a packaging adapter".into(),
            ));
        }
    }
    let mut sources = Table::new();
    let mut registry = Table::new();
    registry.insert("replace-with", value("renzora-kit"));
    let mut vendor = Table::new();
    vendor.insert("directory", value("vendor"));
    sources.insert("crates-io", toml_edit::Item::Table(registry));
    sources.insert("renzora-kit", toml_edit::Item::Table(vendor));
    config.insert("source", toml_edit::Item::Table(sources));
    fs::write(config_path, config.to_string())?;
    let manifest = create_build_kit_manifest(temp, &inputs.requirement)?;
    fs::write(
        temp.join("build-kit.toml"),
        toml::to_string(&manifest)
            .map_err(|error| BuildKitPackageError::Invalid(error.to_string()))?,
    )?;
    Ok(manifest)
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    source_tree: bool,
) -> Result<(), BuildKitPackageError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(BuildKitPackageError::Invalid(format!(
            "symlink source {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if source_tree
                && matches!(
                    entry.file_name().to_str(),
                    Some("target" | ".git" | ".reload" | ".compiler-cache" | ".compiler_cache")
                )
            {
                continue;
            }
            copy_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                source_tree,
            )?;
        }
    } else if metadata.is_file() {
        fs::copy(source, destination)?;
    } else {
        return Err(BuildKitPackageError::Invalid(format!(
            "not a regular source file {}",
            source.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_sdk_contains_sources_and_resolves_its_own_workspace() {
        let temp = tempfile::tempdir().expect("SDK directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let sdk = temp.path().join("rust-sdk");
        package_rust_sdk(&source, &sdk).expect("package real SDK sources");
        let workspace: toml::Value = fs::read_to_string(sdk.join("Cargo.toml"))
            .expect("workspace")
            .parse()
            .expect("workspace TOML");
        assert_eq!(workspace["workspace"]["resolver"].as_str(), Some("2"));
        assert_eq!(
            fs::read_dir(sdk.join("crates"))
                .expect("SDK crates")
                .count(),
            3
        );
        for name in [
            "renzora_plugin",
            "renzora_plugin_derive",
            "renzora_identity",
        ] {
            let root = sdk.join("crates").join(name);
            assert!(root.join("src/lib.rs").is_file());
            assert!(root.join("Cargo.toml").is_file());
            assert!(!root.join("target").exists());
        }
        assert!(package_rust_sdk(&source, &sdk).is_err());
    }

    fn inputs(root: &Path) -> BuildKitPackageInputs {
        let engine = root.join("engine");
        for directory in [
            "xtask",
            ".cargo",
            "crates/example/target",
            "plugins/example/target",
            "target",
            ".git",
        ] {
            fs::create_dir_all(engine.join(directory)).expect("source directory");
        }
        for file in ["Cargo.toml", "xtask/Cargo.toml"] {
            fs::write(engine.join(file), "[workspace]\n").expect("manifest");
        }
        for file in ["Cargo.lock", "xtask/Cargo.lock"] {
            fs::write(engine.join(file), "version = 4\n").expect("lock");
        }
        fs::write(
            engine.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = '1.95.0'\n",
        )
        .expect("toolchain");
        fs::write(engine.join(".cargo/config.toml"), "[build]\njobs = 2\n").expect("config");
        fs::write(engine.join("crates/example/source.rs"), "// source\n").expect("source");
        let vendor = root.join("vendor");
        fs::create_dir(&vendor).expect("vendor");
        fs::write(vendor.join("fixture"), "dependency bytes").expect("vendor fixture");
        let runtime = root.join("runtime");
        fs::create_dir(&runtime).expect("runtime");
        fs::write(runtime.join("companion"), "companion bytes").expect("companion");
        BuildKitPackageInputs {
            engine_root: engine,
            vendor_root: vendor,
            runtime_root: runtime,
            destination: root.join("kit"),
            requirement: BuildKitRequirement {
                engine_build: "fixture".into(),
                target: "x86_64-unknown-linux-gnu".into(),
                profile: "dist".into(),
                toolchain_stamp: "fixture compiler".into(),
            },
        }
    }

    #[test]
    fn packages_verified_inputs_without_developer_outputs_or_source_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let inputs = inputs(temp.path());
        let manifest = package_build_kit(&inputs).expect("package");
        let kit = crate::load_and_validate_build_kit(&inputs.destination, &inputs.requirement)
            .expect("verify");
        assert_eq!(kit.manifest, manifest);
        let workspace = inputs.destination.join("workspace");
        for excluded in [
            "target",
            ".git",
            "crates/example/target",
            "plugins/example/target",
        ] {
            assert!(!workspace.join(excluded).exists());
        }
        let config: toml::Value = fs::read_to_string(workspace.join(".cargo/config.toml"))
            .expect("config")
            .parse()
            .expect("TOML");
        assert_eq!(config["build"]["jobs"].as_integer(), Some(2));
        assert_eq!(
            config["source"]["renzora-kit"]["directory"].as_str(),
            Some("vendor")
        );
        assert_eq!(
            fs::read_to_string(inputs.engine_root.join(".cargo/config.toml"))
                .expect("original config"),
            "[build]\njobs = 2\n"
        );
        assert!(package_build_kit(&inputs).is_err());
        assert_eq!(
            crate::load_and_validate_build_kit(&inputs.destination, &inputs.requirement)
                .expect("still intact")
                .manifest,
            manifest
        );
    }

    #[test]
    fn runtime_reload_shadows_and_caches_are_not_immutable_companions() {
        let temp = tempfile::tempdir().expect("temp");
        let inputs = inputs(temp.path());
        for path in [
            "plugins/.reload",
            ".compiler-cache",
            ".compiler_cache",
            "target",
        ] {
            let directory = inputs.runtime_root.join(path);
            fs::create_dir_all(&directory).expect("scratch directory");
            fs::write(directory.join("scratch"), "not a release input").expect("scratch");
        }
        let manifest = package_build_kit(&inputs).expect("package");
        assert!(inputs.destination.join("runtime/companion").is_file());
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path.ends_with("scratch")));
        assert!(inputs
            .runtime_root
            .join("plugins/.reload/scratch")
            .is_file());
    }

    #[test]
    fn rejects_recursive_destination_and_existing_source_replacement() {
        let temp = tempfile::tempdir().expect("temp");
        let mut inputs = inputs(temp.path());
        inputs.destination = inputs.engine_root.join("nested/kit");
        assert!(package_build_kit(&inputs).is_err());
        inputs.destination = temp.path().join("kit");
        fs::write(
            inputs.engine_root.join(".cargo/config.toml"),
            "[source.crates-io]\nreplace-with = 'custom'\n",
        )
        .expect("custom config");
        assert!(package_build_kit(&inputs).is_err());
        assert!(!inputs.destination.exists());
    }
}
