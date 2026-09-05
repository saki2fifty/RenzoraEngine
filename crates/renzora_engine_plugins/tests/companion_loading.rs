//! Release-artifact gate: standalone plugins cannot borrow Rust host symbols.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires compiled companions in RENZORA_COMPANION_DIR and a C compiler"]
fn every_distribution_companion_loads_without_a_rust_host() {
    let artifacts = PathBuf::from(
        std::env::var_os("RENZORA_COMPANION_DIR").expect("set RENZORA_COMPANION_DIR"),
    );
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plugins = crate_root.join("../../plugins");
    let mut libraries = Vec::new();
    for entry in std::fs::read_dir(plugins).expect("plugin sources") {
        let manifest = entry
            .expect("plugin directory entry")
            .path()
            .join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(manifest).expect("plugin manifest"))
                .expect("valid plugin manifest");
        if manifest
            .get("dependencies")
            .and_then(|dependencies| dependencies.get("renzora_plugin"))
            .is_none()
        {
            continue;
        }
        let name = manifest["package"]["name"].as_str().expect("package name");
        let library = artifacts.join(format!("lib{}.so", name.replace('-', "_")));
        assert!(library.is_file(), "missing companion {}", library.display());
        libraries.push(library);
    }
    assert!(
        !libraries.is_empty(),
        "must exercise real distribution plugins"
    );
    libraries.sort();
    let temporary = tempfile::tempdir().expect("C loader build directory");
    let executable = temporary.path().join("companion-loader");
    let compile = Command::new("cc")
        .arg("-O2")
        .arg(crate_root.join("tests/companion_loader.c"))
        .arg("-ldl")
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("C compiler");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let loaded = Command::new(executable)
        .args(&libraries)
        .output()
        .expect("independent C loader");
    assert!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    println!(
        "{} companion libraries loaded without a Rust host",
        libraries.len()
    );
}
