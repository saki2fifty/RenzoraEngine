//! Immutable publication for replacement editor/runtime generations.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use renzora::{EnginePluginGenerationStamp, ENGINE_PLUGIN_GENERATION_SCHEMA};
use renzora_compiler_cache::staging::{replace_active_pointer, ActivePointer};
use renzora_compiler_cache::types::PublishedGeneration;
use serde::{Deserialize, Serialize};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Compiler outputs ready to copy into an immutable generation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GenerationOutputs {
    /// Complete editor executable, when the build targets the editor.
    pub editor: Option<PathBuf>,
    /// Complete runtime executable, when the build targets the runtime.
    pub runtime: Option<PathBuf>,
}

/// One executable recorded in an immutable generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationArtifact {
    /// Stable role: `editor` or `runtime`.
    pub role: String,
    /// Generation-relative filename.
    pub file: String,
    /// Exact byte length.
    pub size: u64,
    /// Lowercase BLAKE3 digest.
    pub blake3: String,
}

/// On-disk generation record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationManifest {
    /// Record format version.
    pub schema: u32,
    /// Monotonic published generation.
    pub generation: u64,
    /// Full build identity.
    pub stamp: EnginePluginGenerationStamp,
    /// Editor/runtime outputs in stable role order.
    pub artifacts: Vec<GenerationArtifact>,
    /// Aggregate hash used by the atomic candidate pointer.
    pub content_hash: String,
}

/// A validated candidate generation selected by the atomic pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedEngineGeneration {
    /// Immutable generation directory.
    pub root: PathBuf,
    /// Validated record.
    pub manifest: GenerationManifest,
}

/// Generation publication or loading failure.
#[derive(Debug, thiserror::Error)]
pub enum GenerationError {
    /// Filesystem operation failed.
    #[error("generation could not access {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
    /// A supplied compiler output is missing, empty, symlinked, or not a file.
    #[error("invalid {role} generation artifact at {path}")]
    InvalidArtifact {
        /// Expected artifact role.
        role: &'static str,
        /// Supplied path.
        path: PathBuf,
    },
    /// No executable was supplied.
    #[error("a generation must contain an editor or runtime executable")]
    EmptyGeneration,
    /// Generation metadata is invalid.
    #[error("invalid generation manifest {path}: {message}")]
    Manifest {
        /// Manifest path.
        path: PathBuf,
        /// Parse or validation detail.
        message: String,
    },
    /// Atomic candidate pointer is absent or malformed.
    #[error("invalid generation candidate pointer: {0}")]
    CandidatePointer(String),
    /// Pointer and immutable generation disagree.
    #[error("candidate generation failed content verification")]
    CandidateMismatch,
    /// Atomic pointer publication failed.
    #[error("could not publish generation candidate: {0}")]
    PublishPointer(String),
}

/// Publish compiler outputs without replacing the currently running executable.
pub fn publish_generation(
    cache_root: &Path,
    stamp: EnginePluginGenerationStamp,
    outputs: &GenerationOutputs,
) -> Result<PublishedEngineGeneration, GenerationError> {
    if outputs.editor.is_none() && outputs.runtime.is_none() {
        return Err(GenerationError::EmptyGeneration);
    }
    if stamp.schema != ENGINE_PLUGIN_GENERATION_SCHEMA {
        return Err(GenerationError::Manifest {
            path: cache_root.to_path_buf(),
            message: format!(
                "generation stamp schema {} != {}",
                stamp.schema, ENGINE_PLUGIN_GENERATION_SCHEMA
            ),
        });
    }

    let generations = cache_root.join("generations");
    fs::create_dir_all(&generations).map_err(|source| io_error(&generations, source))?;
    let lock_path = cache_root.join("generation.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| io_error(&lock_path, source))?;
    lock.lock().map_err(|source| io_error(&lock_path, source))?;

    let generation = next_generation(&generations)?;
    let temp = generations.join(format!(
        ".tmp.{}.{}.{}",
        generation,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&temp).map_err(|source| io_error(&temp, source))?;

    let result = publish_locked(cache_root, &generations, &temp, generation, stamp, outputs);
    let _ = lock.unlock();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

fn publish_locked(
    cache_root: &Path,
    generations: &Path,
    temp: &Path,
    generation: u64,
    stamp: EnginePluginGenerationStamp,
    outputs: &GenerationOutputs,
) -> Result<PublishedEngineGeneration, GenerationError> {
    let mut artifacts = Vec::new();
    for (role, source, filename) in [
        ("editor", outputs.editor.as_deref(), editor_filename()),
        ("runtime", outputs.runtime.as_deref(), runtime_filename()),
    ] {
        let Some(source) = source else {
            continue;
        };
        validate_artifact(role, source)?;
        let destination = temp.join(filename);
        fs::copy(source, &destination).map_err(|error| io_error(&destination, error))?;
        sync_file(&destination)?;
        let metadata = fs::metadata(&destination).map_err(|error| io_error(&destination, error))?;
        artifacts.push(GenerationArtifact {
            role: role.to_string(),
            file: filename.to_string(),
            size: metadata.len(),
            blake3: hash_file(&destination)?,
        });
    }

    let mut manifest = GenerationManifest {
        schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
        generation,
        stamp,
        artifacts,
        content_hash: String::new(),
    };
    manifest.content_hash = generation_hash(&manifest);
    let manifest_path = temp.join("generation.toml");
    let bytes = toml::to_string(&manifest)
        .map_err(|error| GenerationError::Manifest {
            path: manifest_path.clone(),
            message: error.to_string(),
        })?
        .into_bytes();
    write_synced(&manifest_path, &bytes)?;
    sync_directory(temp)?;

    let final_root = generations.join(format!("{generation:020}-{}", manifest.content_hash));
    fs::rename(temp, &final_root).map_err(|error| io_error(&final_root, error))?;
    sync_directory(generations)?;

    let candidate_dir = cache_root.join("candidate");
    fs::create_dir_all(&candidate_dir).map_err(|error| io_error(&candidate_dir, error))?;
    let pointer = ActivePointer {
        generation: PublishedGeneration(generation),
        fingerprint_hash: decode_hash(&manifest.content_hash)?,
        compiler_service_schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
    };
    let first = !candidate_dir.join("active.bin").exists();
    replace_active_pointer(&candidate_dir, &pointer.encode(), first)
        .map_err(|error| GenerationError::PublishPointer(error.to_string()))?;

    Ok(PublishedEngineGeneration {
        root: final_root,
        manifest,
    })
}

/// Load and fully verify the generation selected by the atomic candidate pointer.
pub fn load_candidate_generation(
    cache_root: &Path,
) -> Result<PublishedEngineGeneration, GenerationError> {
    let pointer_path = cache_root.join("candidate/active.bin");
    let pointer_bytes = fs::read(&pointer_path).map_err(|error| io_error(&pointer_path, error))?;
    let pointer =
        ActivePointer::decode(&pointer_bytes).map_err(GenerationError::CandidatePointer)?;
    if pointer.compiler_service_schema != ENGINE_PLUGIN_GENERATION_SCHEMA {
        return Err(GenerationError::CandidateMismatch);
    }
    let hash = encode_hash(&pointer.fingerprint_hash);
    let root = cache_root
        .join("generations")
        .join(format!("{:020}-{hash}", pointer.generation.0));
    let manifest_path = root.join("generation.toml");
    let text =
        fs::read_to_string(&manifest_path).map_err(|error| io_error(&manifest_path, error))?;
    let manifest: GenerationManifest =
        toml::from_str(&text).map_err(|error| GenerationError::Manifest {
            path: manifest_path.clone(),
            message: error.to_string(),
        })?;
    if manifest.schema != ENGINE_PLUGIN_GENERATION_SCHEMA
        || manifest.generation != pointer.generation.0
        || manifest.content_hash != hash
        || generation_hash(&manifest) != manifest.content_hash
    {
        return Err(GenerationError::CandidateMismatch);
    }
    for artifact in &manifest.artifacts {
        if !matches!(artifact.role.as_str(), "editor" | "runtime")
            || artifact.file.contains(['/', '\\'])
        {
            return Err(GenerationError::CandidateMismatch);
        }
        let path = root.join(&artifact.file);
        let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != artifact.size
            || hash_file(&path)? != artifact.blake3
        {
            return Err(GenerationError::CandidateMismatch);
        }
    }
    Ok(PublishedEngineGeneration { root, manifest })
}

fn next_generation(generations: &Path) -> Result<u64, GenerationError> {
    let mut highest = 0u64;
    for entry in fs::read_dir(generations).map_err(|error| io_error(generations, error))? {
        let entry = entry.map_err(|error| io_error(generations, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(prefix) = name.split('-').next() else {
            continue;
        };
        if let Ok(found) = prefix.parse::<u64>() {
            highest = highest.max(found);
        }
    }
    highest
        .checked_add(1)
        .ok_or_else(|| GenerationError::Manifest {
            path: generations.to_path_buf(),
            message: "generation counter exhausted".to_string(),
        })
}

fn validate_artifact(role: &'static str, path: &Path) -> Result<(), GenerationError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| GenerationError::InvalidArtifact {
        role,
        path: path.to_path_buf(),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(GenerationError::InvalidArtifact {
            role,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn generation_hash(manifest: &GenerationManifest) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&manifest.schema.to_le_bytes());
    hasher.update(&manifest.generation.to_le_bytes());
    hash_field(&mut hasher, manifest.stamp.engine_build.as_bytes());
    hash_field(&mut hasher, manifest.stamp.build_kit_hash.as_bytes());
    hash_field(&mut hasher, manifest.stamp.target.as_bytes());
    hash_field(&mut hasher, manifest.stamp.profile.as_bytes());
    hash_field(&mut hasher, manifest.stamp.toolchain_hash.as_bytes());
    hash_field(&mut hasher, manifest.stamp.lockfile_hash.as_bytes());
    hash_field(&mut hasher, manifest.stamp.integration_hash.as_bytes());
    for feature in &manifest.stamp.features {
        hash_field(&mut hasher, feature.as_bytes());
    }
    for (id, hash) in &manifest.stamp.plugins {
        hash_field(&mut hasher, id.as_bytes());
        hash_field(&mut hasher, hash.as_bytes());
    }
    for artifact in &manifest.artifacts {
        hash_field(&mut hasher, artifact.role.as_bytes());
        hash_field(&mut hasher, artifact.file.as_bytes());
        hasher.update(&artifact.size.to_le_bytes());
        hash_field(&mut hasher, artifact.blake3.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_file(path: &Path) -> Result<String, GenerationError> {
    let bytes = fs::read(path).map_err(|error| io_error(path, error))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn decode_hash(hash: &str) -> Result<[u8; 32], GenerationError> {
    if hash.len() != 64 {
        return Err(GenerationError::CandidateMismatch);
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in hash.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| GenerationError::CandidateMismatch)?;
        bytes[index] =
            u8::from_str_radix(text, 16).map_err(|_| GenerationError::CandidateMismatch)?;
    }
    Ok(bytes)
}

fn encode_hash(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), GenerationError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))?;
    file.write_all(bytes)
        .map_err(|error| io_error(path, error))?;
    file.sync_all().map_err(|error| io_error(path, error))
}

fn sync_file(path: &Path) -> Result<(), GenerationError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error(path, error))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), GenerationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error(path, error))
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), GenerationError> {
    Ok(())
}

fn io_error(path: &Path, source: std::io::Error) -> GenerationError {
    GenerationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(windows)]
fn editor_filename() -> &'static str {
    "renzora-editor.exe"
}

#[cfg(not(windows))]
fn editor_filename() -> &'static str {
    "renzora-editor"
}

#[cfg(windows)]
fn runtime_filename() -> &'static str {
    "renzora.exe"
}

#[cfg(not(windows))]
fn runtime_filename() -> &'static str {
    "renzora"
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn stamp() -> EnginePluginGenerationStamp {
        EnginePluginGenerationStamp {
            schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
            engine_build: "engine".to_string(),
            build_kit_hash: "kit".to_string(),
            target: "target".to_string(),
            profile: "dist".to_string(),
            toolchain_hash: "toolchain".to_string(),
            lockfile_hash: "lock".to_string(),
            integration_hash: "integration".to_string(),
            features: vec!["runtime".to_string()],
            plugins: BTreeMap::from([("com.example.runtime".to_string(), "source".to_string())]),
        }
    }

    fn write_artifact(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, bytes).expect("write artifact");
        path
    }

    #[test]
    fn publication_is_immutable_and_candidate_round_trips() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "built-editor", b"editor generation one");
        let published = publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: Some(output.clone()),
                runtime: None,
            },
        )
        .expect("publish");
        assert_eq!(published.manifest.generation, 1);
        assert_eq!(fs::read(&output).unwrap(), b"editor generation one");
        assert_eq!(load_candidate_generation(temp.path()).unwrap(), published);
    }

    #[test]
    fn later_publication_selects_new_generation_and_keeps_old() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "runtime-output", b"runtime one");
        let first = publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: None,
                runtime: Some(output.clone()),
            },
        )
        .expect("first");
        fs::write(&output, b"runtime two").expect("artifact two");
        let second = publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: None,
                runtime: Some(output),
            },
        )
        .expect("second");
        assert_eq!(first.manifest.generation, 1);
        assert_eq!(second.manifest.generation, 2);
        assert!(first.root.is_dir());
        assert!(second.root.is_dir());
        assert_eq!(load_candidate_generation(temp.path()).unwrap(), second);
    }

    #[test]
    fn failed_publication_never_replaces_candidate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "editor-output", b"known good");
        let known_good = publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: Some(output),
                runtime: None,
            },
        )
        .expect("known good");
        assert!(publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: Some(temp.path().join("missing")),
                runtime: None,
            },
        )
        .is_err());
        assert_eq!(load_candidate_generation(temp.path()).unwrap(), known_good);
    }

    #[test]
    fn tampered_candidate_artifact_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "editor-output", b"known good");
        let published = publish_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: Some(output),
                runtime: None,
            },
        )
        .expect("publish");
        fs::write(published.root.join(editor_filename()), b"tampered").expect("tamper");
        assert!(matches!(
            load_candidate_generation(temp.path()),
            Err(GenerationError::CandidateMismatch)
        ));
    }

    #[test]
    fn empty_zero_length_and_wrong_schema_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            publish_generation(temp.path(), stamp(), &GenerationOutputs::default()),
            Err(GenerationError::EmptyGeneration)
        ));
        let empty = write_artifact(temp.path(), "empty", &[]);
        assert!(matches!(
            publish_generation(
                temp.path(),
                stamp(),
                &GenerationOutputs {
                    editor: Some(empty),
                    runtime: None,
                }
            ),
            Err(GenerationError::InvalidArtifact { .. })
        ));
        let mut wrong = stamp();
        wrong.schema = 99;
        let artifact = write_artifact(temp.path(), "artifact", b"bytes");
        assert!(matches!(
            publish_generation(
                temp.path(),
                wrong,
                &GenerationOutputs {
                    editor: Some(artifact),
                    runtime: None,
                }
            ),
            Err(GenerationError::Manifest { .. })
        ));
    }
}
