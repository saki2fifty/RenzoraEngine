//! Real subprocess proof for the startup protocol, not a rendered editor test.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use renzora::{
    spawn_replacement_process, EnginePluginGenerationStamp, ENGINE_PLUGIN_GENERATION_SCHEMA,
};
use renzora_engine_plugins::replacement::{PendingReplacement, ReplacementProgress};
use renzora_engine_plugins::startup::{
    acknowledge_startup, load_startup_selection, PendingStartup,
};
use renzora_engine_plugins::{publish_generation, GenerationOutputs};
use std::time::{Duration, Instant};

fn fixture_stamp() -> EnginePluginGenerationStamp {
    EnginePluginGenerationStamp {
        schema: ENGINE_PLUGIN_GENERATION_SCHEMA,
        engine_build: "startup-subprocess-fixture".into(),
        target: format!(
            "{}-unknown-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ),
        ..Default::default()
    }
}

#[test]
fn staged_process_acknowledges_its_own_executable_before_selection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let runtime = temp.path().join("runtime-fixture");
    fs::write(&runtime, b"runtime fixture; not executed").expect("runtime");
    let published = publish_generation(
        temp.path(),
        fixture_stamp(),
        &GenerationOutputs {
            editor: Some(std::env::current_exe().expect("test executable")),
            runtime: Some(runtime),
        },
    )
    .expect("stage real test executable");
    let mut pending =
        PendingStartup::begin(temp.path(), published.manifest.generation, &fixture_stamp())
            .expect("begin");
    assert!(pending.try_confirm().expect("not yet ready").is_none());
    let editor = published
        .manifest
        .artifacts
        .iter()
        .find(|a| a.role == "editor")
        .expect("editor");
    // Libtest's ignored/skip arguments transport fixture inputs without changing
    // the parent environment. Production command-line integration is separate.
    let args = vec![
        OsString::from("--exact"),
        OsString::from("startup_confirmation_child"),
        OsString::from("--ignored"),
        OsString::from("--skip"),
        temp.path().as_os_str().to_owned(),
        OsString::from("--skip"),
        OsString::from(pending.token()),
    ];
    let mut child = spawn_replacement_process(&published.root.join(&editor.file), args)
        .expect("spawn staged child");
    assert!(child.wait().expect("reap child").success());
    let selection = pending
        .try_confirm()
        .expect("confirm")
        .expect("acknowledged");
    assert_eq!(
        selection.known_good.generation,
        published.manifest.generation
    );
    assert_eq!(
        load_startup_selection(temp.path()).expect("load selection"),
        Some(selection)
    );
}

#[test]
#[ignore = "executed only as the staged child of the subprocess test"]
fn startup_confirmation_child() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let values: Vec<&OsString> = args
        .windows(2)
        .filter(|pair| pair[0] == "--skip")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(values.len(), 2);
    let root = PathBuf::from(values[0]);
    let token = values[1].to_str().expect("ASCII token");
    acknowledge_startup(&root, token, &fixture_stamp()).expect("acknowledge actual current_exe");
}

fn monitored_fixture(root: &std::path::Path) -> renzora_engine_plugins::PublishedEngineGeneration {
    let runtime = root.join("runtime-fixture");
    fs::write(&runtime, b"runtime fixture").expect("runtime");
    publish_generation(
        root,
        fixture_stamp(),
        &GenerationOutputs {
            editor: Some(std::env::current_exe().expect("test executable")),
            runtime: Some(runtime),
        },
    )
    .expect("stage monitor fixture")
}

fn monitor_arguments(root: &std::path::Path, token: &str, mode: &str) -> Vec<OsString> {
    vec![
        "--exact".into(),
        "monitored_startup_child".into(),
        "--ignored".into(),
        "--skip".into(),
        root.as_os_str().to_owned(),
        "--skip".into(),
        token.into(),
        "--skip".into(),
        mode.into(),
    ]
}

#[test]
fn monitor_accepts_only_a_live_acknowledged_child_and_transfers_handle() {
    let temp = tempfile::tempdir().expect("temp");
    let generation = monitored_fixture(temp.path());
    let mut pending = PendingReplacement::launch(
        temp.path(),
        generation.manifest.generation,
        &fixture_stamp(),
        Duration::from_secs(15),
        |root, token| monitor_arguments(root, token, "ack"),
    )
    .expect("launch monitored child");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match pending.poll().expect("poll") {
            ReplacementProgress::Ready(selection) => {
                assert_eq!(
                    selection.known_good.generation,
                    generation.manifest.generation
                );
                break;
            }
            ReplacementProgress::Pending => {}
        }
        assert!(Instant::now() < deadline, "child did not become ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut child = pending.finish().expect("take acknowledged child");
    fs::write(temp.path().join("release-child"), b"release").expect("release");
    assert!(child.wait().expect("reap child").success());
}

#[test]
fn cancelling_after_readiness_restores_selection_and_releases_restart_lock() {
    for explicit_cancel in [true, false] {
        let temp = tempfile::tempdir().expect("temp");
        let generation = monitored_fixture(temp.path());
        let mut pending = PendingReplacement::launch(
            temp.path(),
            generation.manifest.generation,
            &fixture_stamp(),
            Duration::from_secs(15),
            |root, token| monitor_arguments(root, token, "ack"),
        )
        .expect("launch");
        let deadline = Instant::now() + Duration::from_secs(15);
        while matches!(pending.poll().expect("poll"), ReplacementProgress::Pending) {
            assert!(Instant::now() < deadline, "child did not acknowledge");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_some());
        if explicit_cancel {
            pending.cancel().expect("cancel after new unsaved work");
        } else {
            drop(pending);
        }
        assert!(load_startup_selection(temp.path())
            .expect("restored selection")
            .is_none());
        let next = PendingStartup::begin(
            temp.path(),
            generation.manifest.generation,
            &fixture_stamp(),
        )
        .expect("released startup lock");
        drop(next);
    }
}

#[test]
fn monitor_rejects_early_exit_and_timeout_without_promoting() {
    for (mode, timeout) in [
        ("exit", Duration::from_secs(15)),
        ("wait", Duration::from_millis(500)),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let generation = monitored_fixture(temp.path());
        let mut pending = PendingReplacement::launch(
            temp.path(),
            generation.manifest.generation,
            &fixture_stamp(),
            timeout,
            |root, token| monitor_arguments(root, token, mode),
        )
        .expect("launch");
        let deadline = Instant::now() + Duration::from_secs(15);
        let error = loop {
            match pending.poll() {
                Err(error) => break error,
                Ok(ReplacementProgress::Ready(_)) => panic!("unacknowledged child promoted"),
                Ok(ReplacementProgress::Pending) => {}
            }
            assert!(Instant::now() < deadline, "monitor did not terminate");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(error.to_string().contains(if mode == "exit" {
            "exited"
        } else {
            "timed out"
        }));
        pending.cancel().expect("stop and reap candidate");
        assert!(load_startup_selection(temp.path())
            .expect("selection")
            .is_none());
        assert!(PendingStartup::begin(
            temp.path(),
            generation.manifest.generation,
            &fixture_stamp()
        )
        .is_ok());
    }
}

#[test]
#[ignore = "executed only by monitored startup tests"]
fn monitored_startup_child() {
    let arguments: Vec<_> = std::env::args_os().collect();
    let values: Vec<_> = arguments
        .windows(2)
        .filter(|pair| pair[0] == "--skip")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(values.len(), 3);
    let root = PathBuf::from(values[0]);
    if values[2] == "exit" {
        return;
    }
    if values[2] == "ack" {
        acknowledge_startup(&root, values[1].to_str().expect("token"), &fixture_stamp())
            .expect("acknowledge");
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("release-child").is_file() {
        assert!(Instant::now() < deadline, "parent did not release child");
        std::thread::sleep(Duration::from_millis(10));
    }
}
