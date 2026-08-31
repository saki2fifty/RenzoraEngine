//! End-to-end test for the lean export's generated crate.
//!
//! This test fails if `cargo` is missing from the host (the validation
//! is required, not optional). It does not use Unix-only primitives.
//! It calls the production `stage_static_scripts` entry point — the
//! same function `build_lean` calls when it assembles the lean export
//! workspace. The workspace `Cargo.toml` produced by the test points
//! the generated crate at the engine source via absolute paths so the
//! build can resolve `renzora`, `renzora_identity`, and the transitive
//! path deps the engine already contains on disk.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn script_body() -> &'static str {
    "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
}

/// Find the engine workspace root by walking up from this crate's
/// `Cargo.toml`. `CARGO_MANIFEST_DIR` is the static_scripts crate.
fn engine_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // .../crates/renzora_static_scripts -> .../engine root
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR must be inside the engine workspace")
        .canonicalize()
        .expect("engine root must be canonicalizable")
}

#[test]
fn generated_lean_crate_compiles_in_assembled_workspace() {
    // Required tool: cargo. Missing cargo is a failure, not a skip.
    let cargo_status = Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        cargo_status,
        "cargo is required for F4 validation but is not available on PATH"
    );

    // 1. Build a synthetic project with two scripts.
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    fs::create_dir_all(project.join("enemy")).unwrap();
    fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();
    fs::create_dir_all(project.join("props")).unwrap();
    fs::write(project.join("props/spin.rs"), script_body()).unwrap();

    // 2. Call the production lean-export assembly: stage_static_scripts.
    //    This is the same function `build_lean` calls when assembling
    //    a real export. The `_for_test` wrapper that earlier rounds
    //    shipped is gone.
    let copy_root = project.join("export");
    fs::create_dir_all(&copy_root).unwrap();
    let mut progress = |_s: String| {};
    let wrote = renzora_export::build::stage_static_scripts(project, &copy_root, &mut progress)
        .expect("stage_static_scripts must succeed");
    assert!(wrote, "stage_static_scripts must return true when scripts exist");

    // 3. Resolve engine source paths. The exporter writes the
    //    generated crate under <copy_root>/crates/renzora_static_scripts
    //    with `path = "../renzora"` etc. In a real installed-editor
    //    export, the engine source ships next to the lean export at
    //    the same relative layout. In the test we point those path
    //    deps at the engine workspace on disk via absolute paths.
    let engine = engine_root();
    let renzora_path = engine.join("crates").join("renzora");
    let identity_path = engine.join("crates").join("renzora_identity");
    assert!(renzora_path.join("Cargo.toml").is_file(), "engine must have renzora crate at {}", renzora_path.display());
    assert!(identity_path.join("Cargo.toml").is_file(), "engine must have renzora_identity crate at {}", identity_path.display());

    // Rewrite the generated crate's path deps to absolute paths so
    // they resolve to the engine source tree regardless of where
    // the test copy_root happens to be. This is the same wire format
    // a real installed-editor lean export would use, just with the
    // engine source's actual location instead of a sibling copy.
    let generated_manifest =
        copy_root.join("crates/renzora_static_scripts/Cargo.toml");
    let mut manifest_text = fs::read_to_string(&generated_manifest).unwrap();
    // Replace `path = "../renzora"` and `path = "../renzora_identity"`
    // with absolute paths. Same string-replacement the exporter
    // would do for an installed-engine build.
    manifest_text = manifest_text.replace(
        "path = \"../renzora\"",
        &format!("path = \"{}\"", path_to_toml_str(&renzora_path)),
    );
    manifest_text = manifest_text.replace(
        "path = \"../renzora_identity\"",
        &format!("path = \"{}\"", path_to_toml_str(&identity_path)),
    );
    fs::write(&generated_manifest, manifest_text).unwrap();

    // 4. Write a workspace Cargo.toml at the copy_root with Bevy's
    //    dist profile and the resolved path deps.
    let workspace_toml = format!(
        "[workspace]\n\
         members = [\"crates/renzora_static_scripts\"]\n\
         resolver = \"2\"\n\
         \n\
         [workspace.lints]\n\
         \n\
         [workspace.dependencies]\n\
         renzora = {{ path = \"{}\" }}\n\
         renzora_identity = {{ path = \"{}\" }}\n\
         bevy = \"0.19\"\n\
         \n\
         [profile.dist]\n\
         inherits = \"release\"\n\
         opt-level = 2\n\
         lto = false\n",
        path_to_toml_str(&renzora_path),
        path_to_toml_str(&identity_path),
    );
    fs::write(copy_root.join("Cargo.toml"), workspace_toml).unwrap();

    // 4. Run cargo check --profile dist against the assembled workspace.
    let target_dir = copy_root.join("target");
    let output = Command::new("cargo")
        .args(["check", "--profile", "dist"])
        .current_dir(&copy_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("RUSTC_WRAPPER")
        .output()
        .expect("cargo check spawn");
    assert!(
        output.status.success(),
        "cargo check on the generated lean crate must succeed.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Render an absolute path for inclusion in a Cargo.toml manifest.
/// Cargo accepts both forward and backward slashes; the explicit
/// forward-slash form is portable across platforms.
fn path_to_toml_str(p: &std::path::Path) -> String {
    // `path_clean::clean` is unnecessary; `std::path::Path` joined with
    // forward slashes works in every Cargo manifest on every OS.
    p.to_string_lossy().replace('\\', "/")
}
