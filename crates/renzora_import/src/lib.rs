//! 3D model import and GLB conversion.
//!
//! Supports GLTF/GLB (passthrough), OBJ, STL, PLY, FBX, and USD/USDZ formats.
//! All formats are converted to GLB for use in Bevy.

pub mod anim_decimate;
pub mod anim_extract;
pub mod compact;
mod convert;
pub mod formats;
pub mod inspect;
pub mod glb_compat;
pub mod optimize;
pub mod prune;
pub mod restructure;
pub mod settings;
pub mod sibling_textures;

mod fbx;
// The FBX backend swaps wholesale on the web: `ufbx` is C and has no wasm
// build. Switching the module here rather than `#[cfg]`-ing call sites keeps
// `crate::fbx_ufbx::{convert, extract_animations}` resolving on both targets.
#[cfg(not(target_arch = "wasm32"))]
mod fbx_ufbx;
#[cfg(target_arch = "wasm32")]
#[path = "fbx_ufbx_web.rs"]
mod fbx_ufbx;
mod glb_build;
mod gltf_pass;
mod obj;
mod ply;
mod stl;
mod abc;
mod blend;
mod bvh;
mod dae;
pub mod units;
pub mod usd;

pub use anim_extract::extract_animations_from_glb;
pub use bvh::extract_animations_from_bvh;
pub use compact::compact_glb;
pub use convert::{
    convert_to_glb, convert_to_glb_with_progress, ExtractedAlphaMode, ExtractedPbrMaterial,
    ExtractedTexture, ImportError, ImportResult, ProgressFn, TextureSource,
};
pub use fbx_ufbx::extract_animations as extract_animations_from_fbx;
pub use formats::{detect_format, supported_extensions, ModelFormat};
pub use inspect::{inspect_glb, GlbStats};
pub use optimize::{optimize_glb, MeshOptSettings};
pub use prune::{prune_glb, PruneSpec, Pruned};
pub use settings::{ImportSettings, SceneStructure, UpAxis};
pub use usd::extract_animations_from_usd;
