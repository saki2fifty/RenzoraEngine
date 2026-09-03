//! Unit tests for the host's `check_identity_digest_collision` and
//! the `PluginIdentityDigests` registry. These tests do not require a
//! real plugin / BuildService; they exercise the comparison logic
//! directly with synthetic inputs so a 256-bit BLAKE3 collision can
//! be simulated without producing one.

#![cfg(all(
    not(target_arch = "wasm32"),
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]

use bevy::prelude::*;
use renzora_identity::CanonicalId;
use renzora_plugin::host::{check_identity_digest_collision, identity_digest, PluginIdentityDigests};

fn new_world() -> World {
    let mut world = World::new();
    world.init_resource::<PluginIdentityDigests>();
    world
}

fn id_a() -> CanonicalId {
    CanonicalId::parse("project://a.rs").unwrap()
}

fn id_b() -> CanonicalId {
    CanonicalId::parse("project://b.rs").unwrap()
}

#[test]
fn collision_check_first_registration_installs() {
    let mut world = new_world();
    let id = id_a();
    let digest = identity_digest(&id);
    // First registration: digest absent → install, Ok.
    assert!(check_identity_digest_collision(&mut world, &digest, &id).is_ok());
    let digests = world.resource::<PluginIdentityDigests>();
    assert_eq!(digests.0.get(&digest), Some(&id));
}

#[test]
fn collision_check_same_identity_is_canonical_aliasing() {
    let mut world = new_world();
    let id = id_a();
    let digest = identity_digest(&id);
    // First registration: install.
    check_identity_digest_collision(&mut world, &digest, &id).unwrap();
    // Second registration with the SAME identity: allowed (hot
    // reload).
    assert!(check_identity_digest_collision(&mut world, &digest, &id).is_ok());
    // The registry entry is unchanged.
    let digests = world.resource::<PluginIdentityDigests>();
    assert_eq!(digests.0.get(&digest), Some(&id));
}

#[test]
fn collision_check_different_digest_is_independent() {
    let mut world = new_world();
    let a = id_a();
    let b = id_b();
    let digest_a = identity_digest(&a);
    let digest_b = identity_digest(&b);
    // Two distinct identities produce distinct digests (BLAKE3-256).
    assert_ne!(digest_a, digest_b, "BLAKE3 must produce distinct digests for distinct identities");
    // Both are installable; neither conflicts with the other.
    check_identity_digest_collision(&mut world, &digest_a, &a).unwrap();
    check_identity_digest_collision(&mut world, &digest_b, &b).unwrap();
    let digests = world.resource::<PluginIdentityDigests>();
    assert_eq!(digests.0.get(&digest_a), Some(&a));
    assert_eq!(digests.0.get(&digest_b), Some(&b));
}

#[test]
fn collision_check_same_digest_different_identity_is_refused() {
    // Simulate a 256-bit BLAKE3 collision by registering one
    // identity with a forged digest, then trying to register a
    // different identity under the same digest. The production
    // failure mode for a real (cryptographic) collision is
    // exactly the same as the simulation.
    let mut world = new_world();
    let a = id_a();
    let b = id_b();
    let forged_digest = "deadbeef".repeat(8); // 64 hex chars
    // Install `a` under the forged digest.
    check_identity_digest_collision(&mut world, &forged_digest, &a).unwrap();
    // Try to install `b` under the same forged digest.
    let result = check_identity_digest_collision(&mut world, &forged_digest, &b);
    let reason = result.expect_err("must reject same-digest different-identity");
    assert!(reason.contains("collision"), "diagnostic must mention the collision: {reason}");
    assert!(reason.contains(&a.to_scheme_path()) || reason.contains("project://a.rs"),
        "diagnostic must name the original identity: {reason}");
    assert!(reason.contains(&b.to_scheme_path()) || reason.contains("project://b.rs"),
        "diagnostic must name the new identity: {reason}");
    // The registry entry is UNCHANGED — the failing call did not
    // overwrite the prior identity.
    let digests = world.resource::<PluginIdentityDigests>();
    assert_eq!(digests.0.get(&forged_digest), Some(&a));
}

#[test]
fn collision_check_registry_state_is_preserved_after_refusal() {
    // The single production function the test exercises is
    // `check_identity_digest_collision` itself — this is a
    // unit test of the comparison logic, not an integration
    // test of `register_component`. The scenario: an identity
    // installs under a forged digest; a second identity under the
    // SAME digest is refused; the failing call does not change
    // the registry. (The actual `register_component` integration
    // is covered end-to-end by the save/restore test in the
    // loose-plugins acceptance suite, which goes through a real
    // plugin cdylib.)
    let mut world = new_world();
    let a = id_a();
    let b = id_b();
    let digest = "feedface".repeat(8);
    assert!(check_identity_digest_collision(&mut world, &digest, &a).is_ok());
    assert!(check_identity_digest_collision(&mut world, &digest, &b).is_err());
    let digests = world.resource::<PluginIdentityDigests>();
    assert_eq!(digests.0.get(&digest), Some(&a));
}
