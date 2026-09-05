use bevy::prelude::*;

#[derive(Resource)]
struct RuntimeProbeValue(u32);

#[derive(Default)]
pub struct RuntimeProbePlugin;

impl Plugin for RuntimeProbePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(RuntimeProbeValue(42));
        app.add_systems(Startup, record_runtime);
        app.add_systems(Update, exit_after_frames);
    }
}

fn record_runtime(value: Res<RuntimeProbeValue>) {
    if let Some(root) = std::env::var_os("RENZORA_PHASE5_PROBE_OUTPUT") {
        std::fs::write(
            std::path::PathBuf::from(root).join("runtime-probe"),
            value.0.to_string(),
        )
        .expect("acceptance output");
    }
}

fn exit_after_frames(mut frames: Local<u32>, mut exit: MessageWriter<AppExit>) {
    if std::env::var_os("RENZORA_PHASE5_PROBE_OUTPUT").is_none() {
        return;
    }
    *frames += 1;
    let limit = std::env::var("RENZORA_PHASE5_PROBE_FRAME_LIMIT")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(30);
    if limit != 0 && *frames == limit {
        exit.write(AppExit::Success);
    }
}

renzora::add!(RuntimeProbePlugin, Runtime);
