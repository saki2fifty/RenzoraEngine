#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn listing(path: &Path) -> String {
    let output = Command::new("unzip").arg("-Z1").arg(path).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn package(root: &Path) {
    let script = root.join("runner/scripts/package-release.sh");
    std::fs::create_dir_all(script.parent().unwrap()).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/package-release.sh"),
        &script,
    )
    .unwrap();
    // Outside a checkout, so this fixture does not archive the engine itself.
    let output = Command::new("bash")
        .arg(script)
        .arg(root.join("artifacts"))
        .arg(root.join("out"))
        .args(["r1-alpha7", "fixture"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn flat_release_keeps_source_sdk_and_omits_legacy_archive_without_deleting_it() {
    let root = tempfile::tempdir().unwrap();
    let platform = root.path().join("artifacts/build/windows-x64");
    write(&platform.join("renzora.exe"), b"runtime fixture");
    write(&platform.join("renzora-editor.exe"), b"editor fixture");
    write(&platform.join("plugins/example.dll"), b"plugin fixture");
    write(&platform.join("openxr_loader.dll"), b"OpenXR fixture");
    write(&platform.join("native_support.dll"), b"native dependency");
    write(&platform.join("renzora_dylib.dll"), b"retired engine");
    write(&platform.join("std-deadbeef.dll"), b"retired Rust runtime");
    write(
        &platform.join("rust-sdk/Cargo.toml"),
        b"source package fixture",
    );
    write(&platform.join("sdk/old.rmeta"), b"retired metadata");
    write(&platform.join("sdk.tar.zst"), b"retired archive");
    package(root.path());
    let engine = listing(&root.path().join("out/windows-x64.zip"));
    assert!(engine.lines().any(|p| p == "rust-sdk/Cargo.toml"));
    assert!(!engine
        .lines()
        .any(|p| p.starts_with("sdk/") || p.starts_with("sdk.tar.")));
    let runtime = listing(&root.path().join("out/renzora-runtime-windows-x64.zip"));
    assert!(runtime.lines().any(|p| p == "renzora.exe"));
    assert!(runtime.lines().any(|p| p == "plugins/example.dll"));
    assert!(!runtime.contains("renzora-editor"));
    assert!(runtime.lines().any(|p| p == "openxr_loader.dll"));
    assert!(runtime.lines().any(|p| p == "native_support.dll"));
    assert!(!runtime.contains("renzora_dylib"));
    assert!(!runtime.contains("std-deadbeef"));
    assert!(platform.join("renzora_dylib.dll").is_file());
    assert!(platform.join("sdk/old.rmeta").is_file());
    assert!(platform.join("sdk.tar.zst").is_file());
}

#[test]
fn appimage_release_does_not_add_the_retired_sdk_archive() {
    let root = tempfile::tempdir().unwrap();
    let platform = root.path().join("artifacts/build/linux-x64");
    write(&platform.join("Renzora.AppImage"), b"image fixture");
    write(&platform.join("Renzora.AppDir/renzora"), b"runtime fixture");
    write(&platform.join("sdk.tar.zst"), b"retired archive");
    package(root.path());
    assert_eq!(
        listing(&root.path().join("out/linux-x64.zip")).trim(),
        "Renzora.AppImage"
    );
    assert!(platform.join("sdk.tar.zst").is_file());
}
