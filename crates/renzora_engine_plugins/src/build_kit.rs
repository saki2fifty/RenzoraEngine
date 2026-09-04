//! Deterministic installed-editor build-kit inventory and validation.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current installed-editor build-kit manifest schema.
pub const BUILD_KIT_MANIFEST_SCHEMA: u32 = 1;

/// One immutable file shipped in an installed-editor Tier 2 build kit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildKitFile {
    /// Forward-slash path relative to the build-kit root.
    pub path: String,
    /// File length in bytes.
    pub size: u64,
    /// Lowercase BLAKE3 digest of the file bytes.
    pub blake3: String,
}

/// Signed/content-addressable description shipped as `build-kit.toml`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildKitManifest {
    /// Manifest format version.
    pub schema: u32,
    /// Exact engine build this kit can relink.
    pub engine_build: String,
    /// Target triple for every reusable artifact in this kit.
    pub target: String,
    /// Non-dev Cargo profile used to create the kit.
    pub profile: String,
    /// Exact `rustc -Vv` output expected from the compiler.
    pub toolchain_stamp: String,
    /// Canonically sorted immutable file inventory.
    pub files: Vec<BuildKitFile>,
    /// Hash of every preceding semantic field and inventory record.
    pub content_hash: String,
}

/// Values the running installed editor requires from a build kit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildKitRequirement {
    /// Running editor build identity.
    pub engine_build: String,
    /// Host compilation target.
    pub target: String,
    /// Approved Cargo profile.
    pub profile: String,
    /// Installed/selected compiler stamp.
    pub toolchain_stamp: String,
}

/// Fully validated build kit. Consumers may trust its immutable inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildKit {
    /// Canonical build-kit directory.
    pub root: PathBuf,
    /// Validated manifest.
    pub manifest: BuildKitManifest,
}

/// Build-kit creation or validation failure.
#[derive(Debug, thiserror::Error)]
pub enum BuildKitError {
    /// Filesystem operation failed.
    #[error("build kit could not access {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
    /// Manifest syntax or strict field validation failed.
    #[error("invalid build-kit manifest {path}: {source}")]
    Parse {
        /// Manifest path.
        path: PathBuf,
        /// TOML diagnostic.
        source: toml::de::Error,
    },
    /// Unsupported manifest version.
    #[error("build-kit schema {found} is unsupported; expected {expected}")]
    Schema {
        /// Found schema.
        found: u32,
        /// Supported schema.
        expected: u32,
    },
    /// Kit belongs to another engine, target, profile, or compiler.
    #[error("build-kit {field} mismatch: found `{found}`, expected `{expected}`")]
    Requirement {
        /// Mismatched field.
        field: &'static str,
        /// Manifest value.
        found: String,
        /// Required value.
        expected: String,
    },
    /// Inventory contains an unsafe or non-canonical path.
    #[error("build-kit inventory path `{0}` is unsafe or non-canonical")]
    UnsafePath(String),
    /// Symlinks would make the kit's meaning depend on external mutable state.
    #[error("build kit contains a symbolic link at {0}")]
    Symlink(PathBuf),
    /// Manifest inventory is unsorted, duplicated, incomplete, or contains extras.
    #[error("build-kit inventory does not match files on disk")]
    InventoryMismatch,
    /// One immutable file no longer matches its recorded size or digest.
    #[error("build-kit file failed verification: {0}")]
    FileMismatch(String),
    /// The manifest's aggregate content hash is invalid.
    #[error("build-kit content hash does not match its manifest")]
    ContentHashMismatch,
    /// Required metadata is empty and therefore cannot bind the kit safely.
    #[error("build-kit field `{0}` may not be empty")]
    EmptyField(&'static str),
}

/// Inventory a directory and create its deterministic manifest.
///
/// `build-kit.toml` itself is excluded because it contains the aggregate hash.
pub fn create_build_kit_manifest(
    root: &Path,
    requirement: &BuildKitRequirement,
) -> Result<BuildKitManifest, BuildKitError> {
    validate_requirement_fields(requirement)?;
    let root = canonical_directory(root)?;
    let mut files = Vec::new();
    collect_files(&root, &root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut manifest = BuildKitManifest {
        schema: BUILD_KIT_MANIFEST_SCHEMA,
        engine_build: requirement.engine_build.clone(),
        target: requirement.target.clone(),
        profile: requirement.profile.clone(),
        toolchain_stamp: requirement.toolchain_stamp.clone(),
        files,
        content_hash: String::new(),
    };
    manifest.content_hash = semantic_hash(&manifest);
    Ok(manifest)
}

/// Load `build-kit.toml`, verify its identity, and hash every immutable file.
pub fn load_and_validate_build_kit(
    root: &Path,
    requirement: &BuildKitRequirement,
) -> Result<BuildKit, BuildKitError> {
    validate_requirement_fields(requirement)?;
    let root = canonical_directory(root)?;
    let manifest_path = root.join("build-kit.toml");
    let text = fs::read_to_string(&manifest_path).map_err(|source| BuildKitError::Io {
        path: manifest_path.clone(),
        source,
    })?;
    let manifest: BuildKitManifest =
        toml::from_str(&text).map_err(|source| BuildKitError::Parse {
            path: manifest_path,
            source,
        })?;
    if manifest.schema != BUILD_KIT_MANIFEST_SCHEMA {
        return Err(BuildKitError::Schema {
            found: manifest.schema,
            expected: BUILD_KIT_MANIFEST_SCHEMA,
        });
    }
    check_requirement(
        "engine build",
        &manifest.engine_build,
        &requirement.engine_build,
    )?;
    check_requirement("target", &manifest.target, &requirement.target)?;
    check_requirement("profile", &manifest.profile, &requirement.profile)?;
    check_requirement(
        "toolchain",
        &manifest.toolchain_stamp,
        &requirement.toolchain_stamp,
    )?;
    if semantic_hash(&manifest) != manifest.content_hash {
        return Err(BuildKitError::ContentHashMismatch);
    }

    let mut seen = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for file in &manifest.files {
        validate_inventory_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) || !seen.insert(&file.path) {
            return Err(BuildKitError::InventoryMismatch);
        }
        previous = Some(&file.path);
        let path = root.join(path_from_manifest(&file.path));
        let metadata = fs::symlink_metadata(&path).map_err(|source| BuildKitError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(BuildKitError::Symlink(path));
        }
        if !metadata.is_file() || metadata.len() != file.size || hash_file(&path)? != file.blake3 {
            return Err(BuildKitError::FileMismatch(file.path.clone()));
        }
    }

    let mut actual = Vec::new();
    collect_files(&root, &root, &mut actual)?;
    actual.sort_by(|left, right| left.path.cmp(&right.path));
    if actual != manifest.files {
        return Err(BuildKitError::InventoryMismatch);
    }
    Ok(BuildKit { root, manifest })
}

fn validate_requirement_fields(requirement: &BuildKitRequirement) -> Result<(), BuildKitError> {
    for (name, value) in [
        ("engine_build", requirement.engine_build.as_str()),
        ("target", requirement.target.as_str()),
        ("profile", requirement.profile.as_str()),
        ("toolchain_stamp", requirement.toolchain_stamp.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(BuildKitError::EmptyField(name));
        }
    }
    if matches!(requirement.profile.as_str(), "dev" | "test") {
        return Err(BuildKitError::Requirement {
            field: "profile",
            found: requirement.profile.clone(),
            expected: "dist or release".to_string(),
        });
    }
    Ok(())
}

fn check_requirement(
    field: &'static str,
    found: &str,
    expected: &str,
) -> Result<(), BuildKitError> {
    if found == expected {
        Ok(())
    } else {
        Err(BuildKitError::Requirement {
            field,
            found: found.to_string(),
            expected: expected.to_string(),
        })
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, BuildKitError> {
    let canonical = path.canonicalize().map_err(|source| BuildKitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(BuildKitError::Io {
            path: canonical,
            source: std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"),
        });
    }
    Ok(canonical)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    out: &mut Vec<BuildKitFile>,
) -> Result<(), BuildKitError> {
    let entries = fs::read_dir(directory).map_err(|source| BuildKitError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| BuildKitError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| BuildKitError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(BuildKitError::Symlink(path));
        }
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| BuildKitError::UnsafePath(path.display().to_string()))?;
            let relative = manifest_path(relative)?;
            if relative == "build-kit.toml" {
                continue;
            }
            out.push(BuildKitFile {
                path: relative,
                size: metadata.len(),
                blake3: hash_file(&path)?,
            });
        }
    }
    Ok(())
}

fn manifest_path(path: &Path) -> Result<String, BuildKitError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part
                    .to_str()
                    .ok_or_else(|| BuildKitError::UnsafePath(path.display().to_string()))?;
                parts.push(text);
            }
            _ => return Err(BuildKitError::UnsafePath(path.display().to_string())),
        }
    }
    let normalized = parts.join("/");
    validate_inventory_path(&normalized)?;
    Ok(normalized)
}

fn validate_inventory_path(path: &str) -> Result<(), BuildKitError> {
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || Path::new(path).is_absolute()
    {
        return Err(BuildKitError::UnsafePath(path.to_string()));
    }
    Ok(())
}

fn path_from_manifest(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn hash_file(path: &Path) -> Result<String, BuildKitError> {
    let mut file = fs::File::open(path).map_err(|source| BuildKitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| BuildKitError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn semantic_hash(manifest: &BuildKitManifest) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&manifest.schema.to_le_bytes());
    hash_field(&mut hasher, manifest.engine_build.as_bytes());
    hash_field(&mut hasher, manifest.target.as_bytes());
    hash_field(&mut hasher, manifest.profile.as_bytes());
    hash_field(&mut hasher, manifest.toolchain_stamp.as_bytes());
    for file in &manifest.files {
        hash_field(&mut hasher, file.path.as_bytes());
        hasher.update(&file.size.to_le_bytes());
        hash_field(&mut hasher, file.blake3.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement() -> BuildKitRequirement {
        BuildKitRequirement {
            engine_build: "r1-alpha7+abc".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
            profile: "dist".to_string(),
            toolchain_stamp: "rustc 1.95.0\nhost: x86_64-unknown-linux-gnu".to_string(),
        }
    }

    fn write_manifest(root: &Path, manifest: &BuildKitManifest) {
        fs::write(
            root.join("build-kit.toml"),
            toml::to_string(manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
    }

    #[test]
    fn inventory_is_sorted_and_validates_without_global_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join("engine/src")).expect("create tree");
        fs::write(temp.path().join("z.lock"), b"lock").expect("write lock");
        fs::write(temp.path().join("engine/src/lib.rs"), b"pub fn engine() {}")
            .expect("write source");
        let manifest =
            create_build_kit_manifest(temp.path(), &requirement()).expect("create manifest");
        assert_eq!(manifest.files[0].path, "engine/src/lib.rs");
        assert_eq!(manifest.files[1].path, "z.lock");
        write_manifest(temp.path(), &manifest);
        let kit = load_and_validate_build_kit(temp.path(), &requirement()).expect("validate kit");
        assert_eq!(kit.manifest.content_hash, manifest.content_hash);
    }

    #[test]
    fn changed_and_extra_files_fail_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("Cargo.lock"), b"one").expect("write lock");
        let manifest = create_build_kit_manifest(temp.path(), &requirement()).expect("manifest");
        write_manifest(temp.path(), &manifest);

        fs::write(temp.path().join("Cargo.lock"), b"two").expect("tamper");
        assert!(matches!(
            load_and_validate_build_kit(temp.path(), &requirement()),
            Err(BuildKitError::FileMismatch(_))
        ));
        fs::write(temp.path().join("Cargo.lock"), b"one").expect("restore");
        fs::write(temp.path().join("extra"), b"extra").expect("extra");
        assert!(matches!(
            load_and_validate_build_kit(temp.path(), &requirement()),
            Err(BuildKitError::InventoryMismatch)
        ));
    }

    #[test]
    fn mismatched_target_and_toolchain_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("Cargo.lock"), b"lock").expect("write lock");
        let manifest = create_build_kit_manifest(temp.path(), &requirement()).expect("manifest");
        write_manifest(temp.path(), &manifest);

        let mut wrong_target = requirement();
        wrong_target.target = "x86_64-pc-windows-msvc".to_string();
        assert!(matches!(
            load_and_validate_build_kit(temp.path(), &wrong_target),
            Err(BuildKitError::Requirement {
                field: "target",
                ..
            })
        ));
        let mut wrong_toolchain = requirement();
        wrong_toolchain.toolchain_stamp = "rustc other".to_string();
        assert!(matches!(
            load_and_validate_build_kit(temp.path(), &wrong_toolchain),
            Err(BuildKitError::Requirement {
                field: "toolchain",
                ..
            })
        ));
    }

    #[test]
    fn dev_profile_and_empty_identity_fields_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut dev = requirement();
        dev.profile = "dev".to_string();
        assert!(matches!(
            create_build_kit_manifest(temp.path(), &dev),
            Err(BuildKitError::Requirement {
                field: "profile",
                ..
            })
        ));
        let mut empty = requirement();
        empty.engine_build.clear();
        assert!(matches!(
            create_build_kit_manifest(temp.path(), &empty),
            Err(BuildKitError::EmptyField("engine_build"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_part_of_a_build_kit() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        symlink(outside.path(), temp.path().join("external")).expect("symlink");
        assert!(matches!(
            create_build_kit_manifest(temp.path(), &requirement()),
            Err(BuildKitError::Symlink(_))
        ));
    }

    #[test]
    fn aggregate_hash_detects_manifest_tampering() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("Cargo.lock"), b"lock").expect("write lock");
        let mut manifest =
            create_build_kit_manifest(temp.path(), &requirement()).expect("manifest");
        manifest.engine_build = "different".to_string();
        let altered_requirement = BuildKitRequirement {
            engine_build: "different".to_string(),
            ..requirement()
        };
        write_manifest(temp.path(), &manifest);
        assert!(matches!(
            load_and_validate_build_kit(temp.path(), &altered_requirement),
            Err(BuildKitError::ContentHashMismatch)
        ));
    }
}
