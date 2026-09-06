#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn fixture(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; 2048]).unwrap();
}

#[test]
fn windows_dispatch_honors_requested_profile_without_building() {
    let script = include_str!("../../docker/build-all.sh");
    let start = script.find("build_one() {").unwrap();
    let end = start + script[start..].find("\n}\n").unwrap() + 3;
    for profile in ["release", "dist"] {
        let command = format!(
            "{}\nbuild_desktop() {{ test \"$PROFILE\" = \"$EXPECTED\" && test \"$RENZORA_PROFILE\" = \"$EXPECTED\"; }}\nbuild_plugins() {{ :; }}\nbuild_updater() {{ :; }}\ncompress_binaries() {{ :; }}\nbuild_one windows-x64 editor",
            &script[start..end]
        );
        assert!(Command::new("bash")
            .args(["-c", &command])
            .env("PROFILE", profile)
            .env("RENZORA_PROFILE", profile)
            .env("EXPECTED", profile)
            .status()
            .unwrap()
            .success());
    }
}

#[test]
fn docker_staging_keeps_plugins_and_ignores_retired_engine_images() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let out = root.path().join("out");
    for name in [
        "example.dll",
        "std-deadbeef.dll",
        "libstd-deadbeef.dll",
        "bevy_dylib-deadbeef.dll",
        "renzora_dylib.dll",
        "renzora_ember_dylib.dll",
        "renzora_editor.dll",
    ] {
        fixture(&source.join(name));
    }
    // Execute the production staging function alone, never Docker or a build.
    let script = include_str!("../../docker/build-all.sh");
    let start = script.find("copy_shared_libs() {").unwrap();
    let end = start + script[start..].find("\n}\n").unwrap() + 3;
    let command = format!(
        "{}\ncopy_shared_libs \"$1\" \"$2\" dll",
        &script[start..end]
    );
    assert!(Command::new("bash")
        .args(["-c", &command, "fixture"])
        .arg(&source)
        .arg(&out)
        .status()
        .unwrap()
        .success());
    let files = fs::read_dir(out.join("plugins"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_name(), "example.dll");
    assert_eq!(fs::read_dir(&source).unwrap().count(), 7);
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
}

#[test]
fn compression_selects_current_hosts_and_plugins_in_flat_and_nested_layouts() {
    let root = tempfile::tempdir().unwrap();
    let platform = root.path().join("windows-x64");
    for name in [
        "renzora.exe",
        "renzora-editor.exe",
        "plugins/example.dll",
        "runtime/renzora-runtime.exe",
        "runtime/plugins/second.dll",
        "bevy_dylib-deadbeef.dll",
        "renzora.dll",
        "renzora_editor.dll",
        "rust-sdk/Cargo.toml",
        "openxr_loader.dll",
    ] {
        fixture(&platform.join(name));
    }
    // Record selection without running UPX or modifying any fixture binary.
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let upx = bin.join("upx");
    fs::write(
        &upx,
        "#!/bin/sh\nprintf '%s\\n' \"$2\" >> \"$SELECTION_LOG\"\n",
    )
    .unwrap();
    fs::set_permissions(&upx, fs::Permissions::from_mode(0o755)).unwrap();
    let log = root.path().join("selection.log");
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docker/upx-compress.sh"))
        .arg(&platform)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("SELECTION_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selected = fs::read_to_string(log).unwrap();
    assert_eq!(selected.lines().count(), 5, "{selected}");
    for name in [
        "renzora.exe",
        "renzora-editor.exe",
        "example.dll",
        "renzora-runtime.exe",
        "second.dll",
    ] {
        assert!(
            selected
                .lines()
                .any(|line| Path::new(line).file_name().unwrap() == name),
            "{selected}"
        );
    }
    assert_eq!(
        fs::read(platform.join("renzora-editor.exe")).unwrap(),
        vec![b'x'; 2048]
    );
}
