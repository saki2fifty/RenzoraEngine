//! Exercise the installed-kit builder without entering the editor's GUI.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use renzora_engine_plugins::{
    prepare_engine_build, BuildKitManifest, BuildKitRequirement, EngineBinaryTarget,
    EngineBuildEvent, EngineBuildPreparation, EngineBuildService,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 3 {
        return Err("usage: build_installed_kit <kit> <new-cache> <plugin-root>".into());
    }
    let kit = PathBuf::from(&arguments[0]);
    let manifest: BuildKitManifest =
        toml::from_str(&std::fs::read_to_string(kit.join("build-kit.toml"))?)?;
    let selected = std::process::Command::new("rustup")
        .args(["which", "rustc"])
        .output()?;
    if !selected.status.success() {
        return Err("could not select the pinned Rust compiler".into());
    }
    let compiler = PathBuf::from(String::from_utf8(selected.stdout)?.trim());
    let cargo = compiler
        .parent()
        .ok_or("compiler has no bin directory")?
        .join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    let rustc = std::process::Command::new(&compiler).arg("-vV").output()?;
    if !rustc.status.success() {
        return Err("rustc identity lookup failed".into());
    }
    let windows = manifest.target.split('-').any(|part| part == "windows");
    let suffix = if windows { ".exe" } else { "" };
    let preparation = EngineBuildPreparation {
        plugins_root: PathBuf::from(&arguments[2]),
        build_kit_root: kit,
        expected_build_kit_hash: manifest.content_hash,
        cache_root: PathBuf::from(&arguments[1]),
        cargo,
        rustc: compiler,
        requirement: BuildKitRequirement {
            engine_build: manifest.engine_build,
            target: manifest.target,
            profile: manifest.profile,
            toolchain_stamp: String::from_utf8(rustc.stdout)?,
        },
        features: BTreeSet::new(),
        runtime: EngineBinaryTarget {
            package: "renzora_app".into(),
            binary: "renzora".into(),
            output_name: format!("renzora{suffix}"),
        },
        editor: EngineBinaryTarget {
            package: "renzora_editor_app".into(),
            binary: "renzora-editor".into(),
            output_name: format!("renzora-editor{suffix}"),
        },
    };
    let start = Instant::now();
    let job = prepare_engine_build(&preparation)?;
    println!(
        "prepared_seconds={:.3} workspace={}",
        start.elapsed().as_secs_f64(),
        job.workspace.display()
    );
    let mut service = EngineBuildService::new();
    service.submit(job)?;
    loop {
        for event in service.poll() {
            match event {
                EngineBuildEvent::Failed { message, .. } => return Err(message.into()),
                EngineBuildEvent::Published { generation, .. } => {
                    println!(
                        "published={} elapsed_seconds={:.3}",
                        generation.root.display(),
                        start.elapsed().as_secs_f64()
                    );
                    return Ok(());
                }
                other => println!("{other:?}"),
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
