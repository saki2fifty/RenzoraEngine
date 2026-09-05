//! Prepare the complete native graph before release-side dependency vendoring.

use std::path::PathBuf;
use std::process::Command;

use renzora_engine_plugins::packaging::{prepare_build_kit_workspace, verify_build_kit_lockfile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 2 {
        return Err("usage: prepare_kit_sources <engine-root> <new-workspace>".into());
    }
    let source = PathBuf::from(&arguments[0]);
    let workspace = PathBuf::from(&arguments[1]);
    prepare_build_kit_workspace(&source, &workspace)?;
    let baseline = std::fs::read_to_string(workspace.join("Cargo.lock"))?;
    let status = Command::new("cargo")
        .current_dir(&workspace)
        .args([
            "run",
            "--offline",
            "--locked",
            "--profile",
            "release",
            "--manifest-path",
            "xtask/Cargo.toml",
            "--",
            "sync",
        ])
        .status()?;
    if !status.success() {
        return Err("native wiring failed".into());
    }
    let status = Command::new("cargo")
        .current_dir(&workspace)
        .args(["tree", "--offline", "--depth", "0", "--prefix", "none"])
        .status()?;
    if !status.success() {
        return Err("native dependency resolution failed; obtain the missing pinned release inputs before packaging".into());
    }
    verify_build_kit_lockfile(
        &baseline,
        &std::fs::read_to_string(workspace.join("Cargo.lock"))?,
    )?;
    println!("Prepared {} for locked vendoring", workspace.display());
    Ok(())
}
