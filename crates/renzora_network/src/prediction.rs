//! Reserved client-side prediction API; no prediction or interpolation is implemented.

use bevy::prelude::*;

use crate::components::*;
use crate::status::NetworkStatus;

/// Snap correction threshold: if server correction > this many units,
/// teleport instead of smooth lerp.
pub const SNAP_THRESHOLD: f32 = 2.0;

/// Apply smooth correction for predicted entities when server sends an update.
///
/// If the correction distance is below `SNAP_THRESHOLD`, lerp smoothly.
/// Otherwise, snap immediately to avoid rubber-banding over large distances.
pub fn smooth_correction(
    _query: Query<&mut Transform, With<Networked>>,
    status: Res<NetworkStatus>,
) {
    if !status.is_connected() {
    }
    // Retained as an API hook; the UDP transport does not supply snapshots.
}
