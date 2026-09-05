//! Small, versioned runtime metadata readable without executing foreign code.
//!
//! The runtime executable retains this record even in stripped builds. It is
//! deployment metadata, not an ABI or a signature that makes native code safe.

const PREFIX: &[u8] = b"\0RenzoraRuntimeBuiltinPolicy\0";
const SUFFIX: &[u8] = b"\0EndRenzoraRuntimeBuiltinPolicy\0";
/// Encoded record size, including framing and version.
pub const RUNTIME_CAPABILITIES_LEN: usize = PREFIX.len() + 3 + SUFFIX.len();

/// Built-ins compiled into a runtime that understands game-specific selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeBuiltinCapabilities {
    mask: u8,
}

impl RuntimeBuiltinCapabilities {
    /// Construct from the runtime's actual Cargo feature configuration.
    /// Bit order is the version-one `BUILTIN_RUNTIME_PLUGIN_IDS` order; never
    /// reorder those IDs without introducing a new record version.
    pub const fn new(enabled: [bool; 8]) -> Self {
        let mut mask = 0;
        let mut i = 0;
        while i < enabled.len() {
            if enabled[i] {
                mask |= 1 << i;
            }
            i += 1;
        }
        Self { mask }
    }

    /// Whether this artifact contains the named migrated built-in.
    pub fn contains(self, id: &str) -> bool {
        crate::BUILTIN_RUNTIME_PLUGIN_IDS
            .iter()
            .position(|known| *known == id)
            .is_some_and(|bit| self.mask & (1 << bit) != 0)
    }

    /// Encode the fixed-size, endian-independent deployment record.
    pub const fn encode(self) -> [u8; RUNTIME_CAPABILITIES_LEN] {
        let mut record = [0; RUNTIME_CAPABILITIES_LEN];
        let mut i = 0;
        while i < PREFIX.len() {
            record[i] = PREFIX[i];
            i += 1;
        }
        record[i] = 1;
        record[i + 1] = self.mask;
        record[i + 2] = !self.mask;
        i = 0;
        while i < SUFFIX.len() {
            record[PREFIX.len() + 3 + i] = SUFFIX[i];
            i += 1;
        }
        record
    }

    /// Decode exactly one supported record; reject corrupt or future formats.
    pub fn decode(record: &[u8]) -> Option<Self> {
        (record.len() == RUNTIME_CAPABILITIES_LEN
            && record.starts_with(PREFIX)
            && record.ends_with(SUFFIX)
            && record[PREFIX.len()] == 1
            && record[PREFIX.len() + 2] == !record[PREFIX.len() + 1])
            .then(|| Self {
                mask: record[PREFIX.len() + 1],
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_mask_round_trips_and_keeps_stable_ids() {
        assert_eq!(
            crate::BUILTIN_RUNTIME_PLUGIN_IDS,
            [
                "spline",
                "vignette",
                "auto_exposure",
                "night_stars",
                "procedural_tree",
                "text3d",
                "pool_water",
                "clouds"
            ]
        );
        for mask in 0..=255u8 {
            let capabilities =
                RuntimeBuiltinCapabilities::new(std::array::from_fn(|i| mask & (1 << i) != 0));
            assert_eq!(
                RuntimeBuiltinCapabilities::decode(&capabilities.encode()),
                Some(capabilities)
            );
            for (i, id) in crate::BUILTIN_RUNTIME_PLUGIN_IDS.iter().enumerate() {
                assert_eq!(capabilities.contains(id), mask & (1 << i) != 0);
            }
        }
    }

    #[test]
    fn corrupt_future_and_truncated_records_are_rejected() {
        let original = RuntimeBuiltinCapabilities::new([true; 8]).encode();
        for i in 0..original.len() {
            let mut corrupt = original;
            corrupt[i] ^= 1;
            assert!(RuntimeBuiltinCapabilities::decode(&corrupt).is_none());
        }
        for len in 0..original.len() {
            assert!(RuntimeBuiltinCapabilities::decode(&original[..len]).is_none());
        }
    }
}
