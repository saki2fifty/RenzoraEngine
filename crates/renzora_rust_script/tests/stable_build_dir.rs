//! Tests for the stable build-directory key (correction 14).
//!
//! The build directory name is a 16-hex-character truncated sha256
//! digest of the canonical id's scheme-path. The marker file inside
//! the directory records the canonical id it was generated for, so
//! consumers (editor and exporter) can detect hash collisions or stale
//! directories before trusting the directory's artifact.

#[test]
fn build_dir_name_is_sha256_derived_and_stable() {
    let id_a = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    // sha256(scheme_path) truncated to 8 bytes → 16 hex chars.
    let expected_a = {
        use sha2::{Digest, Sha256};
        let s = id_a.to_scheme_path();
        let digest = Sha256::digest(s.as_bytes());
        let mut out = String::with_capacity(16);
        use core::fmt::Write;
        for byte in digest.iter().take(8) {
            let _ = write!(out, "{byte:02x}");
        }
        out
    };
    assert_eq!(
        renzora_rust_script::script_resolve::build_dir_name(&id_a),
        expected_a,
        "build_dir_name must equal sha256(scheme-path) truncated to 16 hex chars"
    );
}

#[test]
fn marker_round_trip() {
    let id = renzora_identity::CanonicalId::from_rooted(
        renzora_identity::RootKind::Project,
        "enemy/spin.rs",
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    renzora_rust_script::script_resolve::write_build_dir_marker(tmp.path(), &id).unwrap();
    let path = renzora_rust_script::script_resolve::build_dir_marker_path(tmp.path(), &id);
    assert!(path.is_file(), "marker file must be written: {path:?}");
    let read = renzora_rust_script::script_resolve::read_build_dir_marker(&path).unwrap();
    assert_eq!(read.as_deref(), Some(id.to_scheme_path().as_str()));
}

#[test]
fn marker_round_trip_per_id() {
    // Each canonical id has its own build directory; the marker for
    // one id never bleeds into the marker for another.
    let ids = ["enemy/spin.rs", "props/spin.rs", "alone/script.rs"];
    let tmp = tempfile::tempdir().unwrap();
    for rel in ids {
        let id = renzora_identity::CanonicalId::from_rooted(
            renzora_identity::RootKind::Project,
            rel,
        )
        .unwrap();
        renzora_rust_script::script_resolve::write_build_dir_marker(tmp.path(), &id).unwrap();
        let path = renzora_rust_script::script_resolve::build_dir_marker_path(tmp.path(), &id);
        let read = renzora_rust_script::script_resolve::read_build_dir_marker(&path).unwrap();
        assert_eq!(
            read.as_deref(),
            Some(id.to_scheme_path().as_str()),
            "marker round-trip for {rel}"
        );
    }
}
