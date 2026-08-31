//! End-to-end test for the lean export's workspace assembly. Exercises
//! the production `assemble_lean_export_workspace` (which is the same
//! code `build_lean` runs) and then runs `cargo check --profile dist`
//! on the assembled workspace's `renzora_static_scripts` crate.
//!
//! Cross-platform: uses only `std::fs` and absolute paths (no Unix-only
//! primitives). Fails when cargo is missing.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn script_body() -> &'static str {
    "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
}

/// Find the engine workspace root by walking up from this crate's
/// `Cargo.toml`.
fn engine_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // .../crates/renzora_export -> .../engine root
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR must be inside the engine workspace")
        .canonicalize()
        .expect("engine root must be canonicalizable")
}

#[test]
fn generated_lean_workspace_compiles_in_production_assembly() {
    // Required tool: cargo. Missing cargo is a failure, not a skip.
    let cargo_status = Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        cargo_status,
        "cargo is required for the lean-compilation validation but is not available on PATH"
    );

    // 1. Build a synthetic project with two scripts.
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();
    fs::create_dir_all(project.join("props")).unwrap();
    fs::write(project.join("props/spin.rs"), script_body()).unwrap();

    // 2. Call the production workspace-assembly function. This is the
    //    same code `build_lean` runs (source sync + feature/profile
    //    patches + static-plugin staging + static-script staging).
    let engine = engine_root();
    let mut progress = |s: String| eprintln!("[lean] {s}");
    let profile = renzora_export::build::LeanProfile {
        panic_abort: false,
        opt_level_z: false,
        codegen_units_one: false,
    };
    let (workspace, _has_scripts) = renzora_export::build::assemble_lean_export_workspace(
        &engine,
        project,
        renzora_export::Platform::current().expect("host platform known"),
        &[],
        &[],
        profile,
        &[],
        &mut progress,
    )
    .expect("assemble_lean_export_workspace");

    // 3. Run `cargo check --profile dist` against the assembled
    //    workspace's app crate. The static_scripts feature is enabled
    //    when there are scripts (which the assembled workspace has).
    //    This is the EXACT build command `build_lean` issues after
    //    assembly, so a green run is end-to-end coverage.
    let target_dir = workspace.join("target");
    let mut cmd = if let Some(platform) = renzora_export::Platform::current() {
        // Use the platform-default target triple (the host).
        let _ = platform;
        let mut c = Command::new("cargo");
        c.args(["check", "--profile", "dist"]);
        c
    } else {
        let mut c = Command::new("cargo");
        c.args(["check", "--profile", "dist"]);
        c
    };
    cmd.current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("RUSTC_WRAPPER");
    let output = cmd.output().expect("cargo check spawn");
    assert!(
        output.status.success(),
        "cargo check on the assembled lean workspace must succeed.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
