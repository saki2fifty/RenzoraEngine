use std::process::Command;

#[test]
fn retired_compiled_sdk_command_fails_without_creating_output() {
    let destination = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["sdk", "--out"])
        .arg(destination.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("retired"));
    assert!(diagnostic.contains("source-sdk"));
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
}

#[test]
fn source_sdk_command_still_stages_the_small_source_package() {
    let destination = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["source-sdk", "--out"])
        .arg(destination.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sdk = destination.path().join("rust-sdk");
    assert!(sdk.join("Cargo.toml").is_file());
    assert_eq!(
        std::fs::read(
            sdk.join("crates/renzora_plugin")
                .join(renzora_rust_sdk::CONTENT_ROOT_MARKER)
        )
        .unwrap(),
        renzora_rust_sdk::CONTENT_ROOT_LAYOUT
    );
    assert!(!destination.path().join("sdk").exists());
}
