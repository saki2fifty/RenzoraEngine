//! One-time startup confirmation and atomic known-good/rollback selection.
//!
//! Filesystem operations belong on the restart worker, not the frame schedule.
//! This protocol records readiness; the launcher still owns process monitoring,
//! timeout/cancellation, unsaved-work consent, and deciding when to exit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use renzora::EnginePluginGenerationStamp;
use renzora_compiler_cache::staging::replace_active_pointer;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    load_candidate_generation, load_engine_generation, GenerationError, PublishedEngineGeneration,
};

const SCHEMA: u32 = 1;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// Immutable generation identity retained across candidate changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGeneration {
    /// Published generation number.
    pub generation: u64,
    /// Aggregate generation hash.
    pub content_hash: String,
}

impl StartupGeneration {
    fn from_published(generation: &PublishedEngineGeneration) -> Self {
        Self {
            generation: generation.manifest.generation,
            content_hash: generation.manifest.content_hash.clone(),
        }
    }

    fn load(&self, root: &Path) -> Result<PublishedEngineGeneration, StartupError> {
        Ok(load_engine_generation(
            root,
            self.generation,
            &self.content_hash,
        )?)
    }
}

/// Known-good and rollback identities committed in one atomic record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartupSelection {
    schema: u32,
    /// Generation whose startup was acknowledged.
    pub known_good: StartupGeneration,
    /// Previous acknowledged generation, retained for recovery.
    pub rollback: Option<StartupGeneration>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Ticket {
    schema: u32,
    token: String,
    selected: StartupGeneration,
}

/// Startup protocol failure; no failure selects an unacknowledged generation.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// Filesystem or process identity lookup failed.
    #[error("startup protocol I/O failure: {0}")]
    Io(#[from] io::Error),
    /// Another launcher owns the startup transaction.
    #[error("another engine-plugin restart is already pending")]
    Busy,
    /// Stored generation failed verification.
    #[error(transparent)]
    Generation(#[from] GenerationError),
    /// Invalid, mismatched, consumed, or corrupted protocol data.
    #[error("invalid startup confirmation: {0}")]
    Invalid(String),
    /// Atomic publication failed.
    #[error("could not publish startup record: {0}")]
    Publication(String),
}

/// An exclusive, one-time startup transaction; dropping it never promotes a build.
pub struct PendingStartup {
    root: PathBuf,
    ticket_dir: PathBuf,
    ticket: Ticket,
    previous: Option<StartupSelection>,
    completed: bool,
    restored: bool,
    _lock: File,
}

impl PendingStartup {
    /// Pin the explicitly requested candidate and create a fresh handoff token.
    ///
    /// Both editor and runtime artifacts are required. This does not launch a
    /// process or change known-good state. The cache is trusted local storage.
    pub fn begin(
        root: &Path,
        requested_generation: u64,
        expected_stamp: &EnginePluginGenerationStamp,
    ) -> Result<Self, StartupError> {
        let root = fs::canonicalize(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("startup.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StartupError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let selected = load_candidate_generation(&root)?;
        if selected.manifest.generation != requested_generation
            || &selected.manifest.stamp != expected_stamp
            || selected
                .manifest
                .artifacts
                .iter()
                .filter(|a| a.role == "editor")
                .count()
                != 1
            || selected
                .manifest
                .artifacts
                .iter()
                .filter(|a| a.role == "runtime")
                .count()
                != 1
        {
            return Err(StartupError::Invalid(
                "candidate identity or editor/runtime pair mismatch".into(),
            ));
        }
        let previous = load_startup_selection(&root)?;
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|error| {
            StartupError::Invalid(format!("handoff randomness unavailable: {error}"))
        })?;
        let token = blake3::Hash::from(random).to_hex().to_string();
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs)?;
        let ticket_dir = handoffs.join(&token);
        fs::create_dir(&ticket_dir)?;
        let ticket = Ticket {
            schema: SCHEMA,
            token,
            selected: StartupGeneration::from_published(&selected),
        };
        publish_record(&ticket_dir, &ticket)?;
        Ok(Self {
            root,
            ticket_dir,
            ticket,
            previous,
            completed: false,
            restored: false,
            _lock: lock,
        })
    }

    /// Opaque, one-time token passed only to the intended replacement process.
    pub fn token(&self) -> &str {
        &self.ticket.token
    }

    /// Undo this acknowledgement if the old editor cannot complete its handoff.
    /// The transaction still owns the startup lock, so another launcher cannot
    /// have committed a newer selection while this one is being restored.
    pub(crate) fn restore_previous(&mut self) -> Result<(), StartupError> {
        if !self.completed || self.restored {
            return Ok(());
        }
        let directory = self.root.join("startup-selection");
        if let Some(previous) = &self.previous {
            publish_record(&directory, previous)?;
        } else {
            fs::remove_file(directory.join("active.bin"))?;
            #[cfg(unix)]
            File::open(&directory)?.sync_all()?;
        }
        self.restored = true;
        Ok(())
    }

    /// Verify acknowledgement, then atomically select known-good and rollback.
    ///
    /// Returns `None` while confirmation is absent. The caller must separately
    /// check child health and its deadline before invoking this operation.
    pub fn try_confirm(&mut self) -> Result<Option<StartupSelection>, StartupError> {
        self.try_confirm_if(|| Ok(()))
    }

    /// Recheck launcher health after artifact verification, before promotion.
    pub fn try_confirm_if(
        &mut self,
        before_promotion: impl FnOnce() -> Result<(), StartupError>,
    ) -> Result<Option<StartupSelection>, StartupError> {
        if self.completed {
            return Err(StartupError::Invalid("handoff already consumed".into()));
        }
        let Some(ack) = read_optional::<Ticket>(&self.ticket_dir.join("ack/active.bin"))? else {
            return Ok(None);
        };
        if ack != self.ticket {
            return Err(StartupError::Invalid(
                "acknowledgement does not match this handoff".into(),
            ));
        }
        self.ticket.selected.load(&self.root)?;
        let rollback = self.previous.as_ref().and_then(|previous| {
            if previous.known_good == self.ticket.selected {
                previous.rollback.clone()
            } else {
                Some(previous.known_good.clone())
            }
        });
        if let Some(previous) = &rollback {
            previous.load(&self.root)?;
        }
        let selection = StartupSelection {
            schema: SCHEMA,
            known_good: self.ticket.selected.clone(),
            rollback,
        };
        let selection_dir = self.root.join("startup-selection");
        fs::create_dir_all(&selection_dir)?;
        before_promotion()?;
        publish_record(&selection_dir, &selection)?;
        self.completed = true;
        Ok(Some(selection))
    }
}

/// Acknowledge readiness from the replacement process after engine startup.
///
/// `running_stamp` must come from that executable's embedded build identity,
/// not from the ticket. Merely opening a process must not call this function.
pub fn acknowledge_startup(
    root: &Path,
    token: &str,
    running_stamp: &EnginePluginGenerationStamp,
) -> Result<(), StartupError> {
    acknowledge_at(root, token, running_stamp, &std::env::current_exe()?)
}

fn acknowledge_at(
    root: &Path,
    token: &str,
    running_stamp: &EnginePluginGenerationStamp,
    executable: &Path,
) -> Result<(), StartupError> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StartupError::Invalid("invalid handoff token".into()));
    }
    let ticket_dir = root.join("handoffs").join(token);
    let ticket = read_optional::<Ticket>(&ticket_dir.join("active.bin"))?
        .ok_or_else(|| StartupError::Invalid("handoff ticket is absent".into()))?;
    if ticket.schema != SCHEMA || ticket.token != token {
        return Err(StartupError::Invalid("handoff ticket mismatch".into()));
    }
    let selected = ticket.selected.load(root)?;
    let editor = selected
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.role == "editor")
        .ok_or_else(|| StartupError::Invalid("handoff has no editor".into()))?;
    if &selected.manifest.stamp != running_stamp
        || fs::canonicalize(executable)? != fs::canonicalize(selected.root.join(&editor.file))?
    {
        return Err(StartupError::Invalid(
            "running executable or build stamp mismatch".into(),
        ));
    }
    // Exclusive creation consumes the token, including on interrupted writes.
    // A failed attempt gets a new token rather than reusing partial state.
    let ack_dir = ticket_dir.join("ack");
    fs::create_dir(&ack_dir)?;
    publish_record(&ack_dir, &ticket)
}

/// Load and verify both saved recovery identities, or return `None` on first run.
pub fn load_startup_selection(root: &Path) -> Result<Option<StartupSelection>, StartupError> {
    let Some(selection) =
        read_optional::<StartupSelection>(&root.join("startup-selection/active.bin"))?
    else {
        return Ok(None);
    };
    if selection.schema != SCHEMA {
        return Err(StartupError::Invalid(
            "unsupported startup selection schema".into(),
        ));
    }
    selection.known_good.load(root)?;
    if let Some(rollback) = &selection.rollback {
        rollback.load(root)?;
    }
    Ok(Some(selection))
}

fn publish_record(dir: &Path, value: &impl Serialize) -> Result<(), StartupError> {
    let payload =
        toml::to_string(value).map_err(|error| StartupError::Invalid(error.to_string()))?;
    let mut bytes = blake3::hash(payload.as_bytes()).as_bytes().to_vec();
    bytes.extend_from_slice(payload.as_bytes());
    replace_active_pointer(dir, &bytes, !dir.join("active.bin").exists())
        .map_err(|error| StartupError::Publication(error.to_string()))
}

fn read_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StartupError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() < 32
        || bytes.len() as u64 > MAX_RECORD_BYTES
        || blake3::hash(&bytes[32..]).as_bytes() != &bytes[..32]
    {
        return Err(StartupError::Invalid("corrupt startup record".into()));
    }
    let text = std::str::from_utf8(&bytes[32..])
        .map_err(|error| StartupError::Invalid(error.to_string()))?;
    toml::from_str(text)
        .map(Some)
        .map_err(|error| StartupError::Invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{publish_generation, GenerationOutputs};

    fn publish(root: &Path) -> PublishedEngineGeneration {
        let editor = root.join("compiled-editor");
        let runtime = root.join("compiled-runtime");
        fs::write(&editor, b"editor").expect("editor fixture");
        fs::write(&runtime, b"runtime").expect("runtime fixture");
        publish_generation(
            root,
            EnginePluginGenerationStamp {
                schema: renzora::ENGINE_PLUGIN_GENERATION_SCHEMA,
                engine_build: "startup-test".into(),
                ..Default::default()
            },
            &GenerationOutputs {
                editor: Some(editor),
                runtime: Some(runtime),
            },
        )
        .expect("publish fixture")
    }

    fn begin(root: &Path, generation: &PublishedEngineGeneration) -> PendingStartup {
        PendingStartup::begin(
            root,
            generation.manifest.generation,
            &generation.manifest.stamp,
        )
        .expect("begin startup")
    }

    fn acknowledge(root: &Path, attempt: &PendingStartup, generation: &PublishedEngineGeneration) {
        let editor = generation
            .manifest
            .artifacts
            .iter()
            .find(|a| a.role == "editor")
            .expect("editor");
        acknowledge_at(
            root,
            attempt.token(),
            &generation.manifest.stamp,
            &generation.root.join(&editor.file),
        )
        .expect("acknowledge fixture");
    }

    #[test]
    fn missing_acknowledgement_never_promotes_and_drop_releases_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = publish(temp.path());
        let mut attempt = begin(temp.path(), &generation);
        assert!(attempt.try_confirm().expect("poll").is_none());
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
        assert!(matches!(
            PendingStartup::begin(
                temp.path(),
                generation.manifest.generation,
                &generation.manifest.stamp
            ),
            Err(StartupError::Busy)
        ));
        let token = attempt.token().to_string();
        drop(attempt);
        let next = begin(temp.path(), &generation);
        assert_ne!(token, next.token());
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
    }

    #[test]
    fn revoked_handoff_restores_previous_known_good_and_rollback() {
        let temp = tempfile::tempdir().expect("temp");
        let first = publish(temp.path());
        let mut attempt = begin(temp.path(), &first);
        acknowledge(temp.path(), &attempt, &first);
        let previous = attempt.try_confirm().expect("confirm").expect("selection");
        drop(attempt);
        let second = publish(temp.path());
        let mut replacement = begin(temp.path(), &second);
        acknowledge(temp.path(), &replacement, &second);
        replacement
            .try_confirm()
            .expect("replacement confirmation")
            .expect("ready");
        replacement.restore_previous().expect("abort handoff");
        assert!(
            replacement.try_confirm().is_err(),
            "revoked ticket cannot be replayed"
        );
        assert_eq!(
            load_startup_selection(temp.path()).expect("selection"),
            Some(previous)
        );
    }

    #[test]
    fn acknowledgement_is_pinned_and_consumed_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = publish(temp.path());
        let mut attempt = begin(temp.path(), &first);
        let newer = publish(temp.path());
        acknowledge(temp.path(), &attempt, &first);
        let selected = attempt.try_confirm().expect("confirm").expect("ready");
        assert_eq!(selected.known_good.generation, first.manifest.generation);
        assert!(selected.rollback.is_none());
        assert_eq!(
            load_candidate_generation(temp.path()).expect("candidate"),
            newer
        );
        assert!(attempt.try_confirm().is_err());
        let editor = first
            .manifest
            .artifacts
            .iter()
            .find(|a| a.role == "editor")
            .expect("editor");
        assert!(acknowledge_at(
            temp.path(),
            attempt.token(),
            &first.manifest.stamp,
            &first.root.join(&editor.file)
        )
        .is_err());
    }

    #[test]
    fn next_acknowledged_startup_retains_previous_known_good_for_rollback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = publish(temp.path());
        let mut attempt = begin(temp.path(), &first);
        acknowledge(temp.path(), &attempt, &first);
        let previous = attempt
            .try_confirm()
            .expect("confirm first")
            .expect("first ready");
        drop(attempt);

        let second = publish(temp.path());
        let mut attempt = begin(temp.path(), &second);
        assert_eq!(
            load_startup_selection(temp.path()).expect("old selection"),
            Some(previous.clone())
        );
        acknowledge(temp.path(), &attempt, &second);
        let next = attempt
            .try_confirm()
            .expect("confirm second")
            .expect("second ready");
        assert_eq!(next.rollback, Some(previous.known_good));
        assert_eq!(next.known_good.generation, second.manifest.generation);
        assert_eq!(
            load_startup_selection(temp.path()).expect("selection"),
            Some(next)
        );
    }

    #[test]
    fn rejects_wrong_generation_stamp_executable_and_token() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = publish(temp.path());
        assert!(PendingStartup::begin(
            temp.path(),
            generation.manifest.generation + 1,
            &generation.manifest.stamp
        )
        .is_err());
        let mut wrong_stamp = generation.manifest.stamp.clone();
        wrong_stamp.engine_build = "wrong-engine".into();
        assert!(
            PendingStartup::begin(temp.path(), generation.manifest.generation, &wrong_stamp)
                .is_err()
        );
        let mut attempt = begin(temp.path(), &generation);
        assert!(acknowledge_at(
            temp.path(),
            attempt.token(),
            &generation.manifest.stamp,
            &temp.path().join("compiled-editor")
        )
        .is_err());
        let editor = generation
            .manifest
            .artifacts
            .iter()
            .find(|a| a.role == "editor")
            .expect("editor");
        assert!(acknowledge_at(
            temp.path(),
            attempt.token(),
            &wrong_stamp,
            &generation.root.join(&editor.file)
        )
        .is_err());
        assert!(acknowledge_at(
            temp.path(),
            "../escape",
            &generation.manifest.stamp,
            &generation.root.join(&editor.file)
        )
        .is_err());
        assert!(attempt.try_confirm().expect("no ack").is_none());
    }

    #[test]
    fn corrupt_acknowledgement_does_not_select_a_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = publish(temp.path());
        let mut attempt = begin(temp.path(), &generation);
        acknowledge(temp.path(), &attempt, &generation);
        fs::write(attempt.ticket_dir.join("ack/active.bin"), b"incomplete").expect("corrupt ack");
        assert!(attempt.try_confirm().is_err());
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
    }

    #[test]
    fn interrupted_acknowledgement_and_late_previous_ticket_are_not_accepted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = publish(temp.path());
        let old = begin(temp.path(), &generation);
        let old_token = old.token().to_string();
        drop(old);
        let mut attempt = begin(temp.path(), &generation);
        let editor = generation
            .manifest
            .artifacts
            .iter()
            .find(|a| a.role == "editor")
            .expect("editor");
        acknowledge_at(
            temp.path(),
            &old_token,
            &generation.manifest.stamp,
            &generation.root.join(&editor.file),
        )
        .expect("late old acknowledgement");
        assert!(attempt.try_confirm().expect("no current ack").is_none());
        fs::create_dir(attempt.ticket_dir.join("ack")).expect("interrupted ack dir");
        assert!(attempt.try_confirm().expect("no completed ack").is_none());
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
    }

    #[test]
    fn changed_artifact_after_acknowledgement_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let generation = publish(temp.path());
        let mut attempt = begin(temp.path(), &generation);
        acknowledge(temp.path(), &attempt, &generation);
        let editor = generation
            .manifest
            .artifacts
            .iter()
            .find(|a| a.role == "editor")
            .expect("editor");
        fs::write(generation.root.join(&editor.file), b"changed").expect("tamper");
        assert!(attempt.try_confirm().is_err());
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
    }
}
