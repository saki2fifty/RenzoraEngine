//! OS notifications with bounded delivery; no source scans on the frame loop.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use bevy::prelude::*;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use renzora::{
    CurrentProject, EnginePluginBuildReason, EnginePluginBuildRequest, EnginePluginDiagnostic,
    EnginePluginDiagnosticLevel, EnginePluginDiagnostics,
};

struct ProjectWatch {
    _watcher: RecommendedWatcher,
    changed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

#[derive(Default, Resource)]
pub(crate) struct EnginePluginWatch {
    project: Option<PathBuf>,
    pending: Option<Mutex<mpsc::Receiver<Result<ProjectWatch, String>>>>,
    active: Option<ProjectWatch>,
}

/// Bind notifications on a worker, including when `plugins/` does not exist yet.
pub(crate) fn reconcile_watch(
    project: Option<Res<CurrentProject>>,
    pending: Option<Res<renzora::EnginePluginPendingProject>>,
    mut watch: ResMut<EnginePluginWatch>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
) {
    let project = renzora::engine_plugin_project(project.as_deref(), pending.as_deref())
        .map(|path| path.to_path_buf());
    if project == watch.project {
        return;
    }
    watch.active = None;
    watch.pending = None;
    watch.project = project.clone();
    let Some(project) = project else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    match std::thread::Builder::new()
        .name("engine_plugins.watch".into())
        .spawn(move || {
            let _ = sender.send(start_watch(&project));
        }) {
        Ok(_) => watch.pending = Some(Mutex::new(receiver)),
        Err(error) => report_error(&mut diagnostics, &error.to_string()),
    }
}

pub(crate) fn drain_watch(
    mut watch: ResMut<EnginePluginWatch>,
    mut builds: MessageWriter<EnginePluginBuildRequest>,
    mut diagnostics: ResMut<EnginePluginDiagnostics>,
) {
    if let Some(pending) = watch.pending.take() {
        let result = match pending.lock() {
            Ok(receiver) => Some(receiver.try_recv()),
            Err(_) => None,
        };
        match result {
            Some(Ok(Ok(active))) => {
                watch.active = Some(active);
                // Cover a save between project reconciliation and watch installation.
                builds.write(EnginePluginBuildRequest {
                    plugin_id: None,
                    reason: EnginePluginBuildReason::Reconcile,
                });
            }
            Some(Ok(Err(error))) => report_error(&mut diagnostics, &error),
            Some(Err(mpsc::TryRecvError::Empty)) => {
                watch.pending = Some(pending);
            }
            _ => report_error(&mut diagnostics, "notification worker stopped"),
        }
    }
    let Some(active) = watch.active.as_ref() else {
        return;
    };
    if active.failed.swap(false, Ordering::AcqRel) {
        report_error(
            &mut diagnostics,
            "filesystem notification failed; rebuild manually if needed",
        );
    }
    if active.changed.swap(false, Ordering::AcqRel) {
        builds.write(EnginePluginBuildRequest {
            plugin_id: None,
            reason: EnginePluginBuildReason::SourceChanged,
        });
    }
}

fn start_watch(project: &Path) -> Result<ProjectWatch, String> {
    let project = project.canonicalize().map_err(|error| error.to_string())?;
    let plugins = project.join("plugins");
    let changed = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let event_changed = changed.clone();
    let event_failed = failed.clone();
    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| match event {
            Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                if event.need_rescan()
                    || event.paths.is_empty()
                    || event
                        .paths
                        .iter()
                        .any(|path| affects_plugins(&plugins, path))
                {
                    event_changed.store(true, Ordering::Release);
                }
            }
            Ok(_) => {}
            Err(_) => {
                event_failed.store(true, Ordering::Release);
                event_changed.store(true, Ordering::Release);
            }
        },
        Config::default().with_follow_symlinks(false),
    )
    .map_err(|error| error.to_string())?;
    watcher
        .watch(&project, RecursiveMode::Recursive)
        .map_err(|error| error.to_string())?;
    Ok(ProjectWatch {
        _watcher: watcher,
        changed,
        failed,
    })
}

fn affects_plugins(plugins: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(plugins) else {
        return false;
    };
    !relative.components().any(|component| {
        matches!(component, Component::Normal(name) if name == "target" || name == ".git")
    })
}

fn report_error(diagnostics: &mut EnginePluginDiagnostics, message: &str) {
    diagnostics.push(EnginePluginDiagnostic {
        plugin_id: None,
        level: EnginePluginDiagnosticLevel::Warning,
        message: format!("Engine plugin source notifications unavailable: {message}"),
        source: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_plugins_created_after_project_open_and_later_source_changes() {
        let temp = tempfile::tempdir().expect("project");
        let watch = start_watch(temp.path()).expect("watch project");
        let plugins = temp.path().join("plugins");
        std::fs::create_dir(&plugins).expect("plugins");
        await_change(&watch);
        let plugin = plugins.join("weather");
        std::fs::create_dir(&plugin).expect("plugin");
        await_change(&watch);
        let source = plugin.join("plugin.toml");
        std::fs::write(&source, "v1").expect("manifest");
        await_change(&watch);
        std::fs::write(&source, "v2").expect("save");
        await_change(&watch);
        std::fs::remove_file(source).expect("remove");
        await_change(&watch);
        assert!(!watch.failed.load(Ordering::Acquire));
    }

    fn await_change(watch: &ProjectWatch) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !watch.changed.swap(false, Ordering::AcqRel) {
            assert!(
                std::time::Instant::now() < deadline,
                "missing source notification"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn filters_unrelated_files_but_includes_manifest_removal_and_plugin_root() {
        let root = Path::new("project/plugins");
        assert!(affects_plugins(root, root));
        assert!(affects_plugins(root, &root.join("weather/plugin.toml")));
        assert!(affects_plugins(
            root,
            &root.join("weather/runtime/src/lib.rs")
        ));
        assert!(!affects_plugins(root, &root.join("weather/target/output")));
        assert!(!affects_plugins(
            root,
            Path::new("project/assets/scene.ron")
        ));
        assert!(!affects_plugins(
            root,
            Path::new("project/plugins-old/weather.rs")
        ));
    }
}
