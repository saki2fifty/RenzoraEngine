//! Strict discovery and validation for Tier 2 `plugin.toml` declarations.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use renzora::{EnginePluginHalf, EnginePluginManifest, ENGINE_PLUGIN_MANIFEST_SCHEMA};

/// A validated manifest with canonical paths for its declared halves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnginePluginDeclaration {
    /// Parsed, schema-checked manifest.
    pub manifest: EnginePluginManifest,
    /// Canonical directory containing `plugin.toml`.
    pub root: PathBuf,
    /// Canonical runtime crate directory.
    pub runtime: Option<PathBuf>,
    /// Canonical editor crate directory.
    pub editor: Option<PathBuf>,
}

/// Failure to discover or validate an engine-plugin declaration.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// A manifest or directory could not be read.
    #[error("could not read {path}: {source}")]
    Read {
        /// Affected path.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
    /// TOML failed strict deserialization.
    #[error("invalid engine-plugin manifest {path}: {source}")]
    Parse {
        /// Manifest path.
        path: PathBuf,
        /// TOML diagnostic.
        source: toml::de::Error,
    },
    /// The manifest uses a schema this editor cannot safely interpret.
    #[error("engine-plugin manifest {path} uses schema {found}; this editor supports schema {supported}")]
    UnsupportedSchema {
        /// Manifest path.
        path: PathBuf,
        /// Declared schema.
        found: u32,
        /// Supported schema.
        supported: u32,
    },
    /// Stable plugin identity is malformed.
    #[error("engine-plugin id `{id}` in {path} must be a lower-case reverse-domain id")]
    InvalidId {
        /// Manifest path.
        path: PathBuf,
        /// Rejected identity.
        id: String,
    },
    /// Neither runtime nor editor half was declared.
    #[error("engine-plugin manifest {path} must declare at least one of [runtime] or [editor]")]
    MissingHalf {
        /// Manifest path.
        path: PathBuf,
    },
    /// Both scopes point at one crate, defeating editor/runtime separation.
    #[error(
        "engine-plugin manifest {path} must use separate crates for editor and runtime halves"
    )]
    CombinedScope {
        /// Manifest path.
        path: PathBuf,
    },
    /// A declared crate path is absolute, contains traversal, or escapes by symlink.
    #[error("{scope} crate path `{declared}` in {path} must stay inside the plugin directory")]
    EscapingPath {
        /// Manifest path.
        path: PathBuf,
        /// Scope being validated.
        scope: &'static str,
        /// Rejected path text.
        declared: String,
    },
    /// A declared crate directory is missing its manifest.
    #[error("{scope} crate `{declared}` in {path} has no Cargo.toml")]
    MissingCargoManifest {
        /// Manifest path.
        path: PathBuf,
        /// Scope being validated.
        scope: &'static str,
        /// Declared path text.
        declared: String,
    },
    /// Two declarations claim one stable identity.
    #[error("duplicate engine-plugin id `{id}` in {first} and {second}")]
    DuplicateId {
        /// Conflicting id.
        id: String,
        /// First manifest.
        first: PathBuf,
        /// Second manifest.
        second: PathBuf,
    },
}

/// Load and fully validate one `plugin.toml`.
pub fn load_engine_plugin(path: &Path) -> Result<EnginePluginDeclaration, ManifestError> {
    let text = fs::read_to_string(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest: EnginePluginManifest =
        toml::from_str(&text).map_err(|source| ManifestError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    if manifest.schema != ENGINE_PLUGIN_MANIFEST_SCHEMA {
        return Err(ManifestError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: manifest.schema,
            supported: ENGINE_PLUGIN_MANIFEST_SCHEMA,
        });
    }
    if !valid_reverse_domain_id(&manifest.id) {
        return Err(ManifestError::InvalidId {
            path: path.to_path_buf(),
            id: manifest.id,
        });
    }
    if manifest.runtime.is_none() && manifest.editor.is_none() {
        return Err(ManifestError::MissingHalf {
            path: path.to_path_buf(),
        });
    }

    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|source| ManifestError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let runtime = validate_half(path, &root, "runtime", manifest.runtime.as_ref())?;
    let editor = validate_half(path, &root, "editor", manifest.editor.as_ref())?;
    if runtime.is_some() && runtime == editor {
        return Err(ManifestError::CombinedScope {
            path: path.to_path_buf(),
        });
    }

    Ok(EnginePluginDeclaration {
        manifest,
        root,
        runtime,
        editor,
    })
}

/// Discover direct child declarations in deterministic path order.
///
/// Loose Tier 1 `.rs` files and ordinary C-ABI plugin directories are ignored;
/// only a child containing `plugin.toml` explicitly enters Tier 2 discovery.
pub fn discover_engine_plugins(root: &Path) -> Result<Vec<EnginePluginDeclaration>, ManifestError> {
    let mut manifests = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ManifestError::Read {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| ManifestError::Read {
            path: root.to_path_buf(),
            source,
        })?;
        let candidate = entry.path().join("plugin.toml");
        if candidate.is_file() {
            manifests.push(candidate);
        }
    }
    manifests.sort();

    let mut declarations = Vec::with_capacity(manifests.len());
    let mut identities = BTreeMap::<String, PathBuf>::new();
    for path in manifests {
        let declaration = load_engine_plugin(&path)?;
        if let Some(first) = identities.insert(declaration.manifest.id.clone(), path.clone()) {
            return Err(ManifestError::DuplicateId {
                id: declaration.manifest.id,
                first,
                second: path,
            });
        }
        declarations.push(declaration);
    }
    Ok(declarations)
}

fn validate_half(
    manifest_path: &Path,
    root: &Path,
    scope: &'static str,
    half: Option<&EnginePluginHalf>,
) -> Result<Option<PathBuf>, ManifestError> {
    let Some(half) = half else {
        return Ok(None);
    };
    let declared = Path::new(&half.crate_path);
    if half.crate_path.is_empty()
        || declared.is_absolute()
        || declared.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ManifestError::EscapingPath {
            path: manifest_path.to_path_buf(),
            scope,
            declared: half.crate_path.clone(),
        });
    }
    let joined = root.join(declared);
    let canonical = joined
        .canonicalize()
        .map_err(|source| ManifestError::Read {
            path: joined.clone(),
            source,
        })?;
    if !canonical.starts_with(root) {
        return Err(ManifestError::EscapingPath {
            path: manifest_path.to_path_buf(),
            scope,
            declared: half.crate_path.clone(),
        });
    }
    if !canonical.join("Cargo.toml").is_file() {
        return Err(ManifestError::MissingCargoManifest {
            path: manifest_path.to_path_buf(),
            scope,
            declared: half.crate_path.clone(),
        });
    }
    Ok(Some(canonical))
}

fn valid_reverse_domain_id(id: &str) -> bool {
    let segments: Vec<&str> = id.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.as_bytes()[0].is_ascii_lowercase()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !segment.ends_with('-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_plugins_directory_is_an_empty_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(discover_engine_plugins(&temp.path().join("plugins"))
            .expect("optional plugins directory")
            .is_empty());
    }

    fn write_crate(root: &Path, relative: &str) {
        let crate_root = root.join(relative);
        fs::create_dir_all(crate_root.join("src")).expect("create crate");
        fs::write(
            crate_root.join("Cargo.toml"),
            "[package]\nname = \"test_plugin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
    }

    fn write_manifest(root: &Path, body: &str) -> PathBuf {
        let path = root.join("plugin.toml");
        fs::write(&path, body).expect("write plugin.toml");
        path
    }

    #[test]
    fn explicit_runtime_and_editor_halves_validate() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_crate(temp.path(), "runtime");
        write_crate(temp.path(), "editor");
        let path = write_manifest(
            temp.path(),
            "schema = 1\ntype = \"engine\"\nid = \"com.example.weather\"\n\n[runtime]\ncrate = \"runtime\"\n\n[editor]\ncrate = \"editor\"\n",
        );
        let declaration = load_engine_plugin(&path).expect("valid declaration");
        assert!(declaration.runtime.is_some());
        assert!(declaration.editor.is_some());
        assert_ne!(declaration.runtime, declaration.editor);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_crate(temp.path(), "runtime");
        let path = write_manifest(
            temp.path(),
            "schema = 1\ntype = \"engine\"\nid = \"com.example.weather\"\nmagic = true\n\n[runtime]\ncrate = \"runtime\"\n",
        );
        assert!(matches!(
            load_engine_plugin(&path),
            Err(ManifestError::Parse { .. })
        ));
    }

    #[test]
    fn missing_halves_and_combined_scope_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let empty = write_manifest(
            temp.path(),
            "schema = 1\ntype = \"engine\"\nid = \"com.example.empty\"\n",
        );
        assert!(matches!(
            load_engine_plugin(&empty),
            Err(ManifestError::MissingHalf { .. })
        ));

        write_crate(temp.path(), "both");
        let combined = write_manifest(
            temp.path(),
            "schema = 1\ntype = \"engine\"\nid = \"com.example.both\"\n\n[runtime]\ncrate = \"both\"\n\n[editor]\ncrate = \"both\"\n",
        );
        assert!(matches!(
            load_engine_plugin(&combined),
            Err(ManifestError::CombinedScope { .. })
        ));
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        for declared in ["../outside", "/outside"] {
            let path = write_manifest(
                temp.path(),
                &format!(
                    "schema = 1\ntype = \"engine\"\nid = \"com.example.escape\"\n\n[runtime]\ncrate = \"{declared}\"\n"
                ),
            );
            assert!(matches!(
                load_engine_plugin(&path),
                Err(ManifestError::EscapingPath { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        write_crate(outside.path(), "external");
        symlink(outside.path().join("external"), temp.path().join("runtime")).expect("symlink");
        let path = write_manifest(
            temp.path(),
            "schema = 1\ntype = \"engine\"\nid = \"com.example.escape\"\n\n[runtime]\ncrate = \"runtime\"\n",
        );
        assert!(matches!(
            load_engine_plugin(&path),
            Err(ManifestError::EscapingPath { .. })
        ));
    }

    #[test]
    fn discovery_is_sorted_and_duplicate_ids_fail() {
        let temp = tempfile::tempdir().expect("tempdir");
        for directory in ["zeta", "alpha"] {
            let root = temp.path().join(directory);
            fs::create_dir_all(&root).expect("plugin root");
            write_crate(&root, "runtime");
            write_manifest(
                &root,
                &format!(
                    "schema = 1\ntype = \"engine\"\nid = \"com.example.{directory}\"\n\n[runtime]\ncrate = \"runtime\"\n"
                ),
            );
        }
        let found = discover_engine_plugins(temp.path()).expect("discover");
        assert_eq!(found[0].manifest.id, "com.example.alpha");
        assert_eq!(found[1].manifest.id, "com.example.zeta");

        let zeta = temp.path().join("zeta/plugin.toml");
        fs::write(
            zeta,
            "schema = 1\ntype = \"engine\"\nid = \"com.example.alpha\"\n\n[runtime]\ncrate = \"runtime\"\n",
        )
        .expect("rewrite duplicate");
        assert!(matches!(
            discover_engine_plugins(temp.path()),
            Err(ManifestError::DuplicateId { .. })
        ));
    }
}
