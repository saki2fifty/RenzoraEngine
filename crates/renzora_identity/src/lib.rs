//! Pure, deterministic identity types for plugins and scripts.
//!
//! This crate does **not** cross the C ABI. It is host/build/export tooling.
//!
//! - `default-features = false` produces a `no_std` crate using only
//!   `alloc::string::String` and `alloc::vec::Vec`.
//! - `feature = "std"` adds `std` linkage.
//! - `feature = "discovery"` additionally exposes `DiscoveredPath` and the
//!   symlink-escape check; it implies `std`.
//! - `feature = "rustc_lexer"` enables the script declaration recogniser
//!   used by Phase 1 commit 1.4.
//!
//! Every public type that crosses to a consumer is `repr`-transparent or has
//! stable `PartialEq`/`Eq`/`Hash`/`Ord` defined on bytes alone — no
//! platform-dependent equality, no case-folding.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

#[cfg(feature = "discovery")]
mod discovery;
mod identity;
#[cfg(feature = "rustc_lexer")]
mod recogniser;

pub use identity::{AliasLookup, BareAliasIndex, CanonicalId, IdParseError, RootKind};

#[cfg(feature = "discovery")]
pub use discovery::{DiscoveredPath, DiscoveryError};

#[cfg(feature = "rustc_lexer")]
pub use recogniser::{Declaration, Recogniser};
