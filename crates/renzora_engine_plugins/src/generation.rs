//! Immutable publication for replacement editor/runtime generations.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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

/// A verified build-kit file required beside the replacement executables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationSupportFile {
    /// Source in the verified build kit.
    pub source: PathBuf,
    /// Slash-separated generation-relative destination.
    pub destination: String,
    /// Expected build-kit BLAKE3 digest.
    pub blake3: String,
}

/// One file recorded in an immutable generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationArtifact {
    /// Stable role: `editor`, `runtime`, or `support`.
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
    /// Another process selected a newer generation before this one.
    #[error("generation was superseded before candidate selection")]
    CandidateSuperseded,
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
    let published = stage_generation(cache_root, stamp, outputs)?;
    if !select_candidate_generation(cache_root, &published)? {
        return Err(GenerationError::CandidateSuperseded);
    }
    Ok(published)
}

/// Copy and verify large artifacts into an immutable generation without
/// changing the restart candidate. This is safe to run on a worker thread.
pub fn stage_generation(
    cache_root: &Path,
    stamp: EnginePluginGenerationStamp,
    outputs: &GenerationOutputs,
) -> Result<PublishedEngineGeneration, GenerationError> {
    stage_generation_with_support(cache_root, stamp, outputs, &[])
}

/// Stage executable outputs together with their verified runtime dependencies.
pub fn stage_generation_with_support(
    cache_root: &Path,
    stamp: EnginePluginGenerationStamp,
    outputs: &GenerationOutputs,
    support: &[GenerationSupportFile],
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

    let result = stage_locked(&generations, &temp, generation, stamp, outputs, support);
    let _ = lock.unlock();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

fn stage_locked(
    generations: &Path,
    temp: &Path,
    generation: u64,
    stamp: EnginePluginGenerationStamp,
    outputs: &GenerationOutputs,
    support: &[GenerationSupportFile],
) -> Result<PublishedEngineGeneration, GenerationError> {
    let mut artifacts = Vec::new();
    for (role, source, filename) in [
        (
            "editor",
            outputs.editor.as_deref(),
            editor_filename(&stamp.target),
        ),
        (
            "runtime",
            outputs.runtime.as_deref(),
            runtime_filename(&stamp.target),
        ),
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

    let mut names = artifacts
        .iter()
        .map(|a| a.file.clone())
        .collect::<BTreeSet<_>>();
    names.insert("generation.toml".to_string());
    let mut support = support.iter().collect::<Vec<_>>();
    support.sort_by(|a, b| a.destination.cmp(&b.destination));
    for file in support {
        if !safe_relative_file(&file.destination) || !names.insert(file.destination.clone()) {
            return Err(GenerationError::CandidateMismatch);
        }
        validate_artifact("support", &file.source)?;
        let destination = temp.join(&file.destination);
        let parent = destination
            .parent()
            .expect("generation-relative destination");
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        fs::copy(&file.source, &destination).map_err(|error| io_error(&destination, error))?;
        sync_file(&destination)?;
        let digest = hash_file(&destination)?;
        if digest != file.blake3 {
            return Err(GenerationError::CandidateMismatch);
        }
        artifacts.push(GenerationArtifact {
            role: "support".to_string(),
            file: file.destination.clone(),
            size: fs::metadata(&destination)
                .map_err(|error| io_error(&destination, error))?
                .len(),
            blake3: digest,
        });
        sync_directory(parent)?;
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

    Ok(PublishedEngineGeneration {
        root: final_root,
        manifest,
    })
}

/// Atomically select a completely staged generation if it is newer than the
/// current candidate. The monotonic check prevents cross-process stale writes.
pub fn select_candidate_generation(
    cache_root: &Path,
    published: &PublishedEngineGeneration,
) -> Result<bool, GenerationError> {
    let expected_root = cache_root.join("generations").join(format!(
        "{:020}-{}",
        published.manifest.generation, published.manifest.content_hash
    ));
    if published.root != expected_root
        || published.manifest.schema != ENGINE_PLUGIN_GENERATION_SCHEMA
        || generation_hash(&published.manifest) != published.manifest.content_hash
    {
        return Err(GenerationError::CandidateMismatch);
    }
    let lock_path = cache_root.join("generation.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| io_error(&lock_path, source))?;
    lock.lock().map_err(|source| io_error(&lock_path, source))?;
    let candidate_dir = cache_root.join("candidate");
    fs::create_dir_all(&candidate_dir).map_err(|error| io_error(&candidate_dir, error))?;
    let active_path = candidate_dir.join("active.bin");
    if active_path.is_file() {
        let bytes = fs::read(&active_path).map_err(|error| io_error(&active_path, error))?;
        let active = ActivePointer::decode(&bytes).map_err(GenerationError::CandidatePointer)?;
        if active.generation.0 >= published.manifest.generation {
            let _ = lock.unlock();
            return Ok(false);
        }
    }
    let pointer = ActivePointer {
        generation: PublishedGeneration(published.manifest.generation),
        fingerprint_hash: decode_hash(&published.manifest.content_hash)?,
        compiler_service_schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
    };
    let first = !active_path.exists();
    replace_active_pointer(&candidate_dir, &pointer.encode(), first)
        .map_err(|error| GenerationError::PublishPointer(error.to_string()))?;
    let _ = lock.unlock();
    Ok(true)
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
    load_engine_generation(cache_root, pointer.generation.0, &hash)
}

/// Load a pinned generation independently of the mutable candidate pointer.
///
/// Restart acknowledgement and rollback must verify the generation they began
/// with even if a subsequent build has selected a different candidate.
pub fn load_engine_generation(
    cache_root: &Path,
    generation: u64,
    content_hash: &str,
) -> Result<PublishedEngineGeneration, GenerationError> {
    // Decode before constructing a path: callers supply an identity, not an
    // arbitrary cache-relative directory name.
    let hash = encode_hash(&decode_hash(content_hash)?);
    if hash != content_hash || generation == 0 {
        return Err(GenerationError::CandidateMismatch);
    }
    let root = cache_root
        .join("generations")
        .join(format!("{generation:020}-{hash}"));
    let manifest_path = root.join("generation.toml");
    let text =
        fs::read_to_string(&manifest_path).map_err(|error| io_error(&manifest_path, error))?;
    let manifest: GenerationManifest =
        toml::from_str(&text).map_err(|error| GenerationError::Manifest {
            path: manifest_path.clone(),
            message: error.to_string(),
        })?;
    if manifest.schema != ENGINE_PLUGIN_GENERATION_SCHEMA
        || manifest.generation != generation
        || manifest.content_hash != hash
        || generation_hash(&manifest) != manifest.content_hash
    {
        return Err(GenerationError::CandidateMismatch);
    }
    let mut names = BTreeSet::from(["generation.toml".to_string()]);
    for artifact in &manifest.artifacts {
        if !matches!(artifact.role.as_str(), "editor" | "runtime" | "support")
            || !safe_relative_file(&artifact.file)
            || !names.insert(artifact.file.clone())
            || (artifact.role != "support" && artifact.file.contains('/'))
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
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || (role != "support" && metadata.len() == 0)
    {
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
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn safe_relative_file(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
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

fn editor_filename(target: &str) -> &'static str {
    if target.split('-').any(|part| part == "windows") {
        "renzora-editor.exe"
    } else {
        "renzora-editor"
    }
}

fn runtime_filename(target: &str) -> &'static str {
    if target.split('-').any(|part| part == "windows") {
        "renzora.exe"
    } else {
        "renzora"
    }
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
    fn target_names_and_support_files_round_trip_and_detect_tampering() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "built", b"executable");
        let source = write_artifact(temp.path(), "companion", b"plugin library");
        let mut stamp = stamp();
        stamp.target = "x86_64-pc-windows-msvc".into();
        let generation = stage_generation_with_support(
            temp.path(),
            stamp,
            &GenerationOutputs {
                editor: Some(output.clone()),
                runtime: Some(output),
            },
            &[GenerationSupportFile {
                source,
                destination: "plugins/example.dll".into(),
                blake3: blake3::hash(b"plugin library").to_hex().to_string(),
            }],
        )
        .expect("stage full generation");
        assert!(generation.root.join("renzora-editor.exe").is_file());
        assert!(generation.root.join("renzora.exe").is_file());
        assert_eq!(generation.manifest.artifacts.len(), 3);
        select_candidate_generation(temp.path(), &generation).expect("select");
        assert_eq!(
            load_candidate_generation(temp.path()).expect("verify"),
            generation
        );
        // Startup requires the executable pair, not exactly two total files.
        let pending = crate::startup::PendingStartup::begin(
            temp.path(),
            generation.manifest.generation,
            &generation.manifest.stamp,
        )
        .expect("prepare restart");
        drop(pending);
        fs::write(generation.root.join("plugins/example.dll"), b"tampered").expect("tamper");
        assert!(load_candidate_generation(temp.path()).is_err());
        assert_eq!(editor_filename("aarch64-apple-darwin"), "renzora-editor");
        assert_eq!(runtime_filename("x86_64-unknown-linux-gnu"), "renzora");
    }

    #[test]
    fn support_paths_and_digests_cannot_escape_or_replace_executables() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = write_artifact(temp.path(), "built", b"executable");
        for destination in [
            "../escape",
            "/absolute",
            "x/../../escape",
            "x\\file",
            "C:drive",
            "generation.toml",
            "renzora-editor",
        ] {
            let result = stage_generation_with_support(
                temp.path(),
                stamp(),
                &GenerationOutputs {
                    editor: Some(source.clone()),
                    runtime: None,
                },
                &[GenerationSupportFile {
                    source: source.clone(),
                    destination: destination.into(),
                    blake3: blake3::hash(b"executable").to_hex().to_string(),
                }],
            );
            assert!(result.is_err(), "accepted {destination}");
        }
        let result = stage_generation_with_support(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: Some(source.clone()),
                runtime: None,
            },
            &[GenerationSupportFile {
                source,
                destination: "plugins/changed".into(),
                blake3: "incorrect".into(),
            }],
        );
        assert!(result.is_err());
        assert!(!temp.path().join("candidate").exists());
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
        assert_eq!(
            load_engine_generation(
                temp.path(),
                first.manifest.generation,
                &first.manifest.content_hash
            )
            .expect("load pinned rollback generation"),
            first
        );
    }

    #[test]
    fn pinned_generation_rejects_path_injection_and_zero_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        for hash in ["../outside", "", "ABCDEF"] {
            assert!(load_engine_generation(temp.path(), 1, hash).is_err());
        }
        assert!(load_engine_generation(temp.path(), 0, &"0".repeat(64)).is_err());
    }

    #[test]
    fn staging_is_invisible_and_candidate_selection_is_monotonic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let output = write_artifact(temp.path(), "runtime-output", b"runtime one");
        let first = stage_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: None,
                runtime: Some(output.clone()),
            },
        )
        .expect("stage first");
        assert!(!temp.path().join("candidate/active.bin").exists());
        fs::write(&output, b"runtime two").expect("artifact two");
        let second = stage_generation(
            temp.path(),
            stamp(),
            &GenerationOutputs {
                editor: None,
                runtime: Some(output),
            },
        )
        .expect("stage second");

        assert!(select_candidate_generation(temp.path(), &second).expect("select second"));
        assert!(!select_candidate_generation(temp.path(), &first).expect("reject older"));
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
        fs::write(
            published
                .root
                .join(editor_filename(&published.manifest.stamp.target)),
            b"tampered",
        )
        .expect("tamper");
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
