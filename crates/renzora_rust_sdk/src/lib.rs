//! Source-SDK layout shared by compilers and release tools, without an engine dependency.

/// Entry-crate marker identifying a complete packaged source workspace.
pub const CONTENT_ROOT_MARKER: &str = "renzora-sdk-layout";
/// Versioned layout: `<root>/crates/renzora_plugin` is the entry crate.
pub const CONTENT_ROOT_LAYOUT: &[u8] = b"renzora-rust-sdk/v1\n";

#[cfg(feature = "packaging")]
mod packaging;
#[cfg(feature = "packaging")]
pub use packaging::{package, stage, PackageError};
