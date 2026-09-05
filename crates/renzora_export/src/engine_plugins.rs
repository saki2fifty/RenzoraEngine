//! Export must build current native sources, never silently reuse the running editor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use renzora_engine_plugins::{
    discover_engine_plugins, prepare_engine_build, EngineBuildEvent, EngineBuildPreparation,
    EngineBuildService,
};

#[derive(Clone)]
pub(crate) struct Inputs {
    pub preparation: Option<EngineBuildPreparation>,
    pub trusted: bool,
    pub running_has_plugins: bool,
}

pub(crate) struct LeanOptions {
    pub disabled_bevy: Vec<String>,
    pub disabled_runtime: Vec<String>,
    pub profile: crate::build::LeanProfile,
    pub link_plugins: Vec<(String, bool)>,
}

pub(crate) struct BuiltRuntime {
    pub binary: PathBuf,
    pub linked_ids: Vec<String>,
}

/// Runs only on the export worker. Its cache cannot replace an editor candidate.
pub(crate) fn build_runtime(
    inputs: &Inputs,
    project: &Path,
    target: &str,
    cancel: &AtomicBool,
    lean: Option<&LeanOptions>,
    progress: &mut dyn FnMut(String),
) -> Result<Option<BuiltRuntime>, String> {
    let declarations =
        discover_engine_plugins(&project.join("plugins")).map_err(|error| error.to_string())?;
    if declarations.is_empty() && !inputs.running_has_plugins {
        return Ok(None);
    }
    if !inputs.trusted {
        return Err(
            "Approve this project's engine plugins in Settings > Plugins before exporting".into(),
        );
    }
    let mut preparation = inputs
        .preparation
        .clone()
        .ok_or("This export needs the matching installed engine build kit")?;
    if preparation.requirement.target != target {
        return Err(format!(
            "Engine plugin export needs a build kit for {target}; the installed kit targets {}",
            preparation.requirement.target
        ));
    }
    preparation.plugins_root = project.join("plugins");
    preparation.cache_root = preparation.cache_root.join("exports").join(target);
    progress("Preparing current engine plugins for the game runtime...".into());
    let mut job = prepare_engine_build(&preparation).map_err(|error| error.to_string())?;
    let expected_plugins = job.stamp.plugins.clone();
    job.editor = None;
    // The immutable editor snapshot is never changed by size/export settings.
    // A temporary copy survives until the build service has reaped its worker.
    let mut lean_copy = None;
    let mut linked_ids = Vec::new();
    if let Some(lean) = lean {
        std::fs::create_dir_all(&preparation.cache_root).map_err(|error| error.to_string())?;
        let directory = tempfile::Builder::new()
            .prefix("lean-inputs-")
            .tempdir_in(&preparation.cache_root)
            .map_err(|error| error.to_string())?;
        let workspace = directory.path().join("workspace");
        renzora_engine_plugins::packaging::copy_build_sources(&job.workspace, &workspace)
            .map_err(|error| error.to_string())?;
        let (statics, missing) =
            crate::build::resolve_static_plugins(&job.workspace, &lean.link_plugins);
        if !missing.is_empty() {
            progress(format!(
                "Plugins without source remain separate files: {}",
                missing.join(", ")
            ));
        }
        let has_scripts = crate::build::configure_lean_export_workspace(
            &job.workspace,
            &workspace,
            project,
            &lean.disabled_bevy,
            &lean.disabled_runtime,
            lean.profile,
            &statics,
            progress,
        )?;
        linked_ids = statics
            .iter()
            .map(|plugin| plugin.library_stem.clone())
            .collect();
        job.features.insert("runtime".into());
        if !statics.is_empty() {
            job.features.insert("static_plugins".into());
        }
        if has_scripts {
            job.features.insert("static_scripts".into());
        }
        job.profile = "dist-lean".into();
        job.stamp.profile.clone_from(&job.profile);
        job.stamp.features = job.features.iter().cloned().collect();
        let mut identity = preparation.requirement.clone();
        identity.profile.clone_from(&job.profile);
        job.stamp.integration_hash =
            renzora_engine_plugins::create_build_kit_manifest(&workspace, &identity)
                .map_err(|error| error.to_string())?
                .content_hash;
        job.workspace = workspace;
        lean_copy = Some(directory);
    }
    let mut service = EngineBuildService::new();
    service.submit(job).map_err(|error| error.to_string())?;
    loop {
        if cancel.load(Ordering::Acquire) {
            service.invalidate();
            return Err("Export cancelled".into());
        }
        for event in service.poll() {
            match event {
                EngineBuildEvent::Failed { message, .. } => return Err(message),
                EngineBuildEvent::Building { step, .. } => progress(step.into()),
                EngineBuildEvent::Published { generation, .. } => {
                    let latest =
                        prepare_engine_build(&preparation).map_err(|error| error.to_string())?;
                    if latest.stamp.plugins != expected_plugins {
                        return Err("Engine plugins changed during export; retry to include the latest sources".into());
                    }
                    if generation
                        .manifest
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.role == "editor")
                    {
                        return Err(
                            "Runtime export unexpectedly contains an editor executable".into()
                        );
                    }
                    let runtime = generation
                        .manifest
                        .artifacts
                        .iter()
                        .find(|artifact| artifact.role == "runtime")
                        .ok_or("Build produced no runtime")?;
                    // Explicit order: reap/cancel work before deleting its inputs.
                    drop(service);
                    drop(lean_copy);
                    return Ok(Some(BuiltRuntime {
                        binary: generation.root.join(&runtime.file),
                        linked_ids,
                    }));
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an installed engine kit, native build tools and a display"]
    fn installed_kit_export_runs_runtime_plugin_without_editor_plugin() {
        let kit =
            std::path::PathBuf::from(std::env::var_os("RENZORA_PHASE5_TEST_KIT").expect("kit"));
        let cache =
            std::path::PathBuf::from(std::env::var_os("RENZORA_PHASE5_TEST_CACHE").expect("cache"));
        let running = renzora_engine_plugins::load_candidate_generation(&cache).expect("candidate");
        let preparation =
            renzora_engine_plugins::installation::load(&kit, &cache, &running.manifest.stamp)
                .expect("installed kit");
        let project = tempfile::tempdir().expect("project");
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../renzora_engine_plugins/tests/fixtures");
        renzora_engine_plugins::packaging::copy_build_sources(
            &fixtures,
            &project.path().join("plugins"),
        )
        .expect("project plugins");
        std::fs::write(
            project.path().join("project.toml"),
            "name = \"Native export acceptance\"\nversion = \"0.1.0\"\n",
        )
        .expect("project manifest");
        let target = preparation.requirement.target.clone();
        let inputs = Inputs {
            preparation: Some(preparation),
            trusted: true,
            running_has_plugins: true,
        };
        let runtime = build_runtime(
            &inputs,
            project.path(),
            &target,
            &AtomicBool::new(false),
            None,
            &mut |message| eprintln!("{message}"),
        )
        .expect("runtime build")
        .expect("native runtime");
        let output = tempfile::tempdir().expect("output");
        let log = std::fs::File::create(output.path().join("runtime.log")).expect("log");
        let mut child = std::process::Command::new(&runtime.binary)
            .arg("--project")
            .arg(project.path())
            .env("RENZORA_PHASE5_PROBE_OUTPUT", output.path())
            .env("RENZORA_PHASE5_PROBE_FRAME_LIMIT", "30")
            .env_remove("RENZORA_PHASE5_SCRIPT_PROBE")
            .env_remove("RENZORA_PHASE5_RESTART_PROBE_CACHE")
            .stdout(log.try_clone().expect("log clone"))
            .stderr(log)
            .spawn()
            .expect("launch exported runtime");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll runtime") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "exported runtime did not finish: {}",
                    std::fs::read_to_string(output.path().join("runtime.log")).unwrap_or_default()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(output.path().join("runtime.log")).expect("runtime log")
        );
        assert_eq!(
            std::fs::read_to_string(output.path().join("runtime-probe")).expect("runtime marker"),
            "42"
        );
        assert!(!output.path().join("editor-probe").exists());
    }

    #[test]
    fn removing_all_native_plugins_cannot_export_the_old_embedded_runtime() {
        let project = tempfile::tempdir().expect("project");
        let inputs = Inputs {
            preparation: None,
            trusted: false,
            running_has_plugins: true,
        };
        let error = build_runtime(
            &inputs,
            project.path(),
            "target",
            &AtomicBool::new(false),
            None,
            &mut |_| {},
        )
        .err()
        .expect("must not fall back to old runtime");
        assert!(error.contains("Approve"));
    }

    #[test]
    fn ordinary_project_keeps_its_existing_export_path() {
        let project = tempfile::tempdir().expect("project");
        let inputs = Inputs {
            preparation: None,
            trusted: false,
            running_has_plugins: false,
        };
        assert!(build_runtime(
            &inputs,
            project.path(),
            "target",
            &AtomicBool::new(false),
            None,
            &mut |_| {}
        )
        .expect("ordinary export")
        .is_none());
    }
}
