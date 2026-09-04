//! Deterministic, immutable overlay workspaces for Tier 2 builds.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use toml_edit::{Array, DocumentMut, Item};

use crate::{BuildKit, EnginePluginDeclaration};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A complete content-addressed workspace ready for generated wiring and Cargo.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayWorkspace {
    /// Canonical workspace root.
    pub root: PathBuf,
    /// Hash of the kit plus every declared plugin input.
    pub integration_hash: String,
    /// True when an already-complete identical workspace was reused.
    pub cache_hit: bool,
}

/// Hash one validated plugin independently for generation inventories.
pub fn engine_plugin_source_hash(
    declaration: &EnginePluginDeclaration,
) -> Result<String, OverlayError> {
    let mut hasher = blake3::Hasher::new();
    let manifest = toml::to_string(&declaration.manifest).map_err(|error| {
        OverlayError::WorkspaceManifest {
            path: declaration.root.join("plugin.toml"),
            message: error.to_string(),
        }
    })?;
    hash_field(&mut hasher, manifest.as_bytes());
    hash_tree(&mut hasher, &declaration.root, &declaration.root)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Overlay creation failure.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    /// Filesystem operation failed.
    #[error("overlay could not access {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
    /// The build kit has no workspace source tree.
    #[error("build kit has no workspace directory at {0}")]
    MissingWorkspace(PathBuf),
    /// A plugin source contains a symlink and is therefore not an immutable snapshot.
    #[error("engine-plugin source contains a symbolic link at {0}")]
    Symlink(PathBuf),
    /// A source path cannot be represented portably.
    #[error("engine-plugin source path is not portable: {0}")]
    UnsafePath(PathBuf),
    /// The copied root manifest cannot be edited safely.
    #[error("invalid overlay workspace manifest {path}: {message}")]
    WorkspaceManifest {
        /// Root Cargo manifest.
        path: PathBuf,
        /// Parse or shape diagnostic.
        message: String,
    },
    /// Two plugin halves would be copied to the same generated location.
    #[error("engine-plugin integration path collision at {0}")]
    Collision(PathBuf),
}

/// Materialize a content-addressed build workspace without touching kit or user sources.
pub fn materialize_overlay(
    kit: &BuildKit,
    declarations: &[EnginePluginDeclaration],
    cache_root: &Path,
) -> Result<OverlayWorkspace, OverlayError> {
    let kit_workspace = kit.root.join("workspace");
    if !kit_workspace.is_dir() {
        return Err(OverlayError::MissingWorkspace(kit_workspace));
    }
    let integration_hash = integration_hash(kit, declarations)?;
    let overlays = cache_root.join("overlays").join(&kit.manifest.content_hash);
    let destination = overlays.join(&integration_hash);
    let completion = destination.join(".renzora-overlay-complete");
    if fs::read_to_string(&completion).ok().as_deref() == Some(integration_hash.as_str()) {
        return Ok(OverlayWorkspace {
            root: destination,
            integration_hash,
            cache_hit: true,
        });
    }

    fs::create_dir_all(&overlays).map_err(|source| io_error(&overlays, source))?;
    let temp = overlays.join(format!(
        ".{}.tmp.{}.{}",
        integration_hash,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if temp.exists() {
        fs::remove_dir_all(&temp).map_err(|source| io_error(&temp, source))?;
    }
    copy_tree(&kit_workspace, &temp)?;

    let mut members = Vec::new();
    let mut registry = String::from("schema = 1\n");
    let tier2_root = temp.join("tier2");
    for declaration in declarations {
        let id_dir = portable_id_directory(&declaration.manifest.id);
        registry.push_str("\n[[plugin]]\n");
        push_toml_string(&mut registry, "id", &declaration.manifest.id);
        for (scope, source) in [
            ("runtime", declaration.runtime.as_deref()),
            ("editor", declaration.editor.as_deref()),
        ] {
            let Some(source) = source else {
                continue;
            };
            let relative = format!("tier2/{id_dir}/{scope}");
            let target = temp.join(relative_path(&relative)?);
            if target.exists() {
                return cleanup_error(&temp, OverlayError::Collision(target));
            }
            copy_tree(source, &target).inspect_err(|_| {
                let _ = fs::remove_dir_all(&temp);
            })?;
            members.push(relative.clone());
            push_toml_string(&mut registry, scope, &relative);
        }
    }
    members.sort();
    patch_workspace_members(&temp.join("Cargo.toml"), &members)?;
    fs::create_dir_all(&tier2_root).map_err(|source| io_error(&tier2_root, source))?;
    write_new_file(&temp.join("tier2/plugins.toml"), registry.as_bytes())?;
    write_new_file(
        &temp.join(".renzora-overlay-complete"),
        integration_hash.as_bytes(),
    )?;

    match fs::rename(&temp, &destination) {
        Ok(()) => {}
        Err(error) if destination.join(".renzora-overlay-complete").is_file() => {
            let _ = fs::remove_dir_all(&temp);
            if fs::read_to_string(destination.join(".renzora-overlay-complete"))
                .ok()
                .as_deref()
                != Some(integration_hash.as_str())
            {
                return Err(io_error(&destination, error));
            }
        }
        Err(error) => return cleanup_error(&temp, io_error(&destination, error)),
    }

    Ok(OverlayWorkspace {
        root: destination,
        integration_hash,
        cache_hit: false,
    })
}

fn integration_hash(
    kit: &BuildKit,
    declarations: &[EnginePluginDeclaration],
) -> Result<String, OverlayError> {
    let mut ordered: Vec<&EnginePluginDeclaration> = declarations.iter().collect();
    ordered.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, kit.manifest.content_hash.as_bytes());
    for declaration in ordered {
        hash_field(&mut hasher, declaration.manifest.id.as_bytes());
        for (scope, source) in [
            ("runtime", declaration.runtime.as_deref()),
            ("editor", declaration.editor.as_deref()),
        ] {
            if let Some(source) = source {
                hash_field(&mut hasher, scope.as_bytes());
                hash_tree(&mut hasher, source, source)?;
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_tree(
    hasher: &mut blake3::Hasher,
    root: &Path,
    directory: &Path,
) -> Result<(), OverlayError> {
    let mut entries = read_dir_sorted(directory)?;
    for path in entries.drain(..) {
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(OverlayError::Symlink(path));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| OverlayError::UnsafePath(path.clone()))?;
        let portable = portable_path(relative)?;
        if metadata.is_dir() {
            hash_field(hasher, b"directory");
            hash_field(hasher, portable.as_bytes());
            hash_tree(hasher, root, &path)?;
        } else if metadata.is_file() {
            hash_field(hasher, b"file");
            hash_field(hasher, portable.as_bytes());
            let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
            hash_field(hasher, &bytes);
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), OverlayError> {
    fs::create_dir_all(destination).map_err(|source_error| io_error(destination, source_error))?;
    for path in read_dir_sorted(source)? {
        let metadata =
            fs::symlink_metadata(&path).map_err(|source_error| io_error(&path, source_error))?;
        if metadata.file_type().is_symlink() {
            return Err(OverlayError::Symlink(path));
        }
        let name = path
            .file_name()
            .ok_or_else(|| OverlayError::UnsafePath(path.clone()))?;
        let target = destination.join(name);
        if metadata.is_dir() {
            copy_tree(&path, &target)?;
        } else if metadata.is_file() {
            fs::copy(&path, &target).map_err(|source_error| io_error(&target, source_error))?;
        }
    }
    Ok(())
}

fn patch_workspace_members(path: &Path, new_members: &[String]) -> Result<(), OverlayError> {
    let source = fs::read_to_string(path).map_err(|error| io_error(path, error))?;
    let mut document =
        source
            .parse::<DocumentMut>()
            .map_err(|error| OverlayError::WorkspaceManifest {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    let members = document
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .and_then(|workspace| workspace.get_mut("members"))
        .and_then(Item::as_array_mut)
        .ok_or_else(|| OverlayError::WorkspaceManifest {
            path: path.to_path_buf(),
            message: "[workspace].members must be an array".to_string(),
        })?;
    append_unique_members(members, new_members);
    make_owner_writable(path)?;
    fs::write(path, document.to_string()).map_err(|error| io_error(path, error))
}

#[cfg(unix)]
fn make_owner_writable(path: &Path) -> Result<(), OverlayError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(path, permissions).map_err(|error| io_error(path, error))
}

#[cfg(windows)]
fn make_owner_writable(path: &Path) -> Result<(), OverlayError> {
    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).map_err(|error| io_error(path, error))
}

fn append_unique_members(members: &mut Array, new_members: &[String]) {
    for member in new_members {
        if !members.iter().any(|value| value.as_str() == Some(member)) {
            members.push(member.as_str());
        }
    }
}

fn read_dir_sorted(directory: &Path) -> Result<Vec<PathBuf>, OverlayError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
    let mut paths = Vec::new();
    for entry in entries {
        paths.push(entry.map_err(|source| io_error(directory, source))?.path());
    }
    paths.sort();
    Ok(paths)
}

fn portable_path(path: &Path) -> Result<String, OverlayError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| OverlayError::UnsafePath(path.to_path_buf()))?,
            ),
            _ => return Err(OverlayError::UnsafePath(path.to_path_buf())),
        }
    }
    Ok(parts.join("/"))
}

fn relative_path(path: &str) -> Result<PathBuf, OverlayError> {
    if path
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(OverlayError::UnsafePath(PathBuf::from(path)));
    }
    Ok(path.split('/').collect())
}

fn portable_id_directory(id: &str) -> String {
    id.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'0'..=b'9' => byte as char,
            _ => '_',
        })
        .collect()
}

fn push_toml_string(target: &mut String, key: &str, value: &str) {
    target.push_str(key);
    target.push_str(" = ");
    target.push_str(&toml_edit::Value::from(value).to_string());
    target.push('\n');
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), OverlayError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn io_error(path: &Path, source: std::io::Error) -> OverlayError {
    OverlayError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn cleanup_error<T>(temp: &Path, error: OverlayError) -> Result<T, OverlayError> {
    let _ = fs::remove_dir_all(temp);
    Err(error)
}

#[cfg(test)]
mod tests {
    use renzora::{EnginePluginHalf, EnginePluginManifest, EnginePluginType};

    use super::*;
    use crate::{BuildKitManifest, BUILD_KIT_MANIFEST_SCHEMA};

    fn kit(root: &Path) -> BuildKit {
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("src")).expect("workspace");
        fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\".\"]\n[package]\nname = \"host\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("root manifest");
        fs::write(workspace.join("src/lib.rs"), "pub fn host() {}\n").expect("host source");
        BuildKit {
            root: root.canonicalize().expect("canonical kit"),
            manifest: BuildKitManifest {
                schema: BUILD_KIT_MANIFEST_SCHEMA,
                engine_build: "engine".to_string(),
                target: "target".to_string(),
                profile: "dist".to_string(),
                toolchain_stamp: "rustc".to_string(),
                files: Vec::new(),
                content_hash: "kit-hash".to_string(),
            },
        }
    }

    fn plugin(root: &Path, id: &str, runtime: bool, editor: bool) -> EnginePluginDeclaration {
        let create = |scope: &str| {
            let path = root.join(scope);
            fs::create_dir_all(path.join("src")).expect("plugin crate");
            fs::write(
                path.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{}_{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                    id.replace('.', "_"),
                    scope
                ),
            )
            .expect("plugin manifest");
            fs::write(path.join("src/lib.rs"), format!("pub fn {scope}() {{}}\n"))
                .expect("plugin source");
            path.canonicalize().expect("canonical plugin")
        };
        let runtime_path = runtime.then(|| create("runtime"));
        let editor_path = editor.then(|| create("editor"));
        EnginePluginDeclaration {
            manifest: EnginePluginManifest {
                schema: 1,
                plugin_type: EnginePluginType::Engine,
                id: id.to_string(),
                runtime: runtime.then(|| EnginePluginHalf {
                    crate_path: "runtime".to_string(),
                }),
                editor: editor.then(|| EnginePluginHalf {
                    crate_path: "editor".to_string(),
                }),
            },
            root: root.canonicalize().expect("canonical root"),
            runtime: runtime_path,
            editor: editor_path,
        }
    }

    #[test]
    fn overlay_is_deterministic_separated_and_does_not_modify_inputs() {
        let kit_dir = tempfile::tempdir().expect("kit");
        let plugin_dir = tempfile::tempdir().expect("plugin");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let declaration = plugin(plugin_dir.path(), "com.example.weather", true, true);
        let original_root =
            fs::read_to_string(kit.root.join("workspace/Cargo.toml")).expect("root");
        let original_runtime = fs::read_to_string(
            declaration
                .runtime
                .as_ref()
                .expect("runtime")
                .join("src/lib.rs"),
        )
        .expect("runtime");

        let first = materialize_overlay(&kit, std::slice::from_ref(&declaration), cache.path())
            .expect("first overlay");
        assert!(!first.cache_hit);
        assert!(first
            .root
            .join("tier2/com_example_weather/runtime/Cargo.toml")
            .is_file());
        assert!(first
            .root
            .join("tier2/com_example_weather/editor/Cargo.toml")
            .is_file());
        let generated =
            fs::read_to_string(first.root.join("tier2/plugins.toml")).expect("registry");
        assert!(generated.contains("runtime = \"tier2/com_example_weather/runtime\""));
        assert!(generated.contains("editor = \"tier2/com_example_weather/editor\""));
        let root_manifest =
            fs::read_to_string(first.root.join("Cargo.toml")).expect("overlay root");
        assert!(root_manifest.contains("tier2/com_example_weather/runtime"));
        assert!(root_manifest.contains("tier2/com_example_weather/editor"));
        assert_eq!(
            fs::read_to_string(kit.root.join("workspace/Cargo.toml")).unwrap(),
            original_root
        );
        assert_eq!(
            fs::read_to_string(declaration.runtime.as_ref().unwrap().join("src/lib.rs")).unwrap(),
            original_runtime
        );

        let second = materialize_overlay(&kit, &[declaration], cache.path()).expect("cache hit");
        assert!(second.cache_hit);
        assert_eq!(first.root, second.root);
        assert_eq!(first.integration_hash, second.integration_hash);
    }

    #[test]
    fn source_change_creates_a_new_overlay() {
        let kit_dir = tempfile::tempdir().expect("kit");
        let plugin_dir = tempfile::tempdir().expect("plugin");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let declaration = plugin(plugin_dir.path(), "com.example.runtime", true, false);
        let first = materialize_overlay(&kit, std::slice::from_ref(&declaration), cache.path())
            .expect("first");
        fs::write(
            declaration.runtime.as_ref().unwrap().join("src/lib.rs"),
            "pub fn runtime() { println!(\"changed\"); }\n",
        )
        .expect("edit source");
        let second = materialize_overlay(&kit, &[declaration], cache.path()).expect("second");
        assert_ne!(first.integration_hash, second.integration_hash);
        assert_ne!(first.root, second.root);
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let kit_dir = tempfile::tempdir().expect("kit");
        let plugin_dir = tempfile::tempdir().expect("plugin");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let declaration = plugin(plugin_dir.path(), "com.example.runtime", true, false);
        let outside = tempfile::NamedTempFile::new().expect("outside");
        symlink(
            outside.path(),
            declaration
                .runtime
                .as_ref()
                .unwrap()
                .join("src/external.rs"),
        )
        .expect("symlink");
        assert!(matches!(
            materialize_overlay(&kit, &[declaration], cache.path()),
            Err(OverlayError::Symlink(_))
        ));
    }

    #[test]
    fn declaration_order_does_not_change_identity() {
        let kit_dir = tempfile::tempdir().expect("kit");
        let a_dir = tempfile::tempdir().expect("a");
        let b_dir = tempfile::tempdir().expect("b");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let a = plugin(a_dir.path(), "com.example.alpha", true, false);
        let b = plugin(b_dir.path(), "com.example.beta", false, true);
        let first =
            materialize_overlay(&kit, &[a.clone(), b.clone()], cache.path()).expect("first");
        let second = materialize_overlay(&kit, &[b, a], cache.path()).expect("second");
        assert_eq!(first.integration_hash, second.integration_hash);
        assert_eq!(first.root, second.root);
        assert!(second.cache_hit);
    }

    #[test]
    fn empty_plugin_set_still_produces_a_valid_overlay() {
        let kit_dir = tempfile::tempdir().expect("kit");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let overlay = materialize_overlay(&kit, &[], cache.path()).expect("overlay");
        let registry =
            fs::read_to_string(overlay.root.join("tier2/plugins.toml")).expect("registry");
        assert_eq!(registry, "schema = 1\n");
    }

    #[test]
    fn generated_registry_is_valid_toml() {
        let kit_dir = tempfile::tempdir().expect("kit");
        let plugin_dir = tempfile::tempdir().expect("plugin");
        let cache = tempfile::tempdir().expect("cache");
        let kit = kit(kit_dir.path());
        let declaration = plugin(plugin_dir.path(), "com.example.weather", true, true);
        let overlay = materialize_overlay(&kit, &[declaration], cache.path()).expect("overlay");
        let registry =
            fs::read_to_string(overlay.root.join("tier2/plugins.toml")).expect("registry");
        let parsed: toml::Value = toml::from_str(&registry).expect("valid TOML");
        let plugin = parsed["plugin"].as_array().expect("plugin array");
        assert_eq!(plugin.len(), 1);
        assert_eq!(plugin[0]["id"].as_str(), Some("com.example.weather"));
    }
}
