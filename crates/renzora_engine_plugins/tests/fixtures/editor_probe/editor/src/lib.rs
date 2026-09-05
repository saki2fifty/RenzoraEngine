use bevy::prelude::*;

#[derive(Default)]
pub struct EditorProbePlugin;

impl Plugin for EditorProbePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, record_editor);
        app.add_systems(
            Update,
            (
                install_script_probe,
                record_script_result,
                drive_restart_probe,
            ),
        );
    }
}

fn record_editor() {
    if let Some(root) = std::env::var_os("RENZORA_PHASE5_PROBE_OUTPUT") {
        std::fs::write(
            std::path::PathBuf::from(root).join("editor-probe"),
            "editor only",
        )
        .expect("acceptance output");
    }
}

renzora::add!(EditorProbePlugin, Editor);

#[derive(Component)]
struct ScriptProbe;

fn install_script_probe(
    mut commands: Commands,
    project: Option<Res<renzora::CurrentProject>>,
    splash: Option<Res<State<renzora::SplashState>>>,
    mut installed: Local<bool>,
) {
    if *installed || std::env::var_os("RENZORA_PHASE5_SCRIPT_PROBE").is_none() {
        return;
    }
    if project.is_none() || splash.is_none_or(|state| *state.get() != renzora::SplashState::Editor)
    {
        return;
    }
    let mut scripts = renzora_scripting::ScriptComponent::from_file("scripts/probe.rs".into());
    scripts.scripts[0].preview = true;
    commands.spawn((
        Name::new("installed Rust script acceptance"),
        Transform::default(),
        scripts,
        ScriptProbe,
    ));
    *installed = true;
}

fn record_script_result(
    query: Query<&Transform, With<ScriptProbe>>,
    mut exit: MessageWriter<AppExit>,
    mut recorded: Local<bool>,
) {
    if *recorded {
        return;
    }
    if query
        .iter()
        .any(|transform| (transform.translation.x - 7.0).abs() < 0.001)
    {
        let root = std::env::var_os("RENZORA_PHASE5_PROBE_OUTPUT").expect("acceptance output");
        std::fs::write(
            std::path::PathBuf::from(root).join("script-probe"),
            "position=7",
        )
        .expect("script acceptance output");
        *recorded = true;
        exit.write(AppExit::Success);
    }
}

fn drive_restart_probe(
    config: Option<ResMut<renzora_engine_plugins::EnginePluginEcsConfig>>,
    project: Option<Res<renzora::CurrentProject>>,
    state: Res<renzora::EnginePluginBuildState>,
    mut trusts: MessageWriter<renzora::EnginePluginTrustRequest>,
    mut builds: MessageWriter<renzora::EnginePluginBuildRequest>,
    mut restarts: MessageWriter<renzora::EnginePluginRestartRequest>,
    mut configured: Local<bool>,
    mut approved: Local<bool>,
    mut requested: Local<bool>,
) {
    let Some(cache) = std::env::var_os("RENZORA_PHASE5_RESTART_PROBE_CACHE") else {
        return;
    };
    let Some(project) = project else {
        return;
    };
    if !*configured {
        let Some(mut config) = config else {
            return;
        };
        // Test-only cache injection reuses the expensive installed-kit build.
        // Installation validation, preparation, restart and acknowledgement
        // still use the production systems; no readiness state is fabricated.
        config.preparation.cache_root = cache.into();
        builds.write(renzora::EnginePluginBuildRequest {
            plugin_id: None,
            reason: renzora::EnginePluginBuildReason::Reconcile,
        });
        *configured = true;
        return;
    }
    if matches!(
        *state,
        renzora::EnginePluginBuildState::AwaitingTrust { .. }
    ) && !*approved
    {
        trusts.write(renzora::EnginePluginTrustRequest {
            project: project.path.clone(),
            trusted: true,
        });
        *approved = true;
    }
    if let renzora::EnginePluginBuildState::RestartReady { generation, stamp } = &*state {
        if !*requested {
            restarts.write(renzora::EnginePluginRestartRequest {
                project: project.path.clone(),
                generation: *generation,
                stamp: stamp.clone(),
            });
            let root = std::env::var_os("RENZORA_PHASE5_PROBE_OUTPUT").expect("acceptance output");
            std::fs::write(
                std::path::PathBuf::from(root).join("restart-requested"),
                generation.to_string(),
            )
            .expect("restart acceptance output");
            *requested = true;
        }
    }
}
