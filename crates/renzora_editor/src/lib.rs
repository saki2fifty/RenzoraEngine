//! The editor — every editor-only plugin crate, linked as rlibs and installed
//! by [`install`].
//!
//! This crate is what makes the editor removable. `renzora.exe` links only
//! `renzora_runtime`; `renzora-editor.exe` links this crate on top. Same engine,
//! two binaries, and a shipped game carries none of the editor.
//!
//! Third-party extensions are C-ABI plugins (`renzora_plugin`), which link no
//! Bevy. Full Bevy extensions are linked into a replacement executable pair
//! and require a restart; no `App` reference crosses a library boundary.

#[cfg(feature = "editor")]
mod plugins;

/// Install the whole editor into `app`.
///
/// Called directly by the native editor and wasm builds. Call it AFTER
/// `add_engine_plugins`, so the editor layers
/// on top of the runtime foundation.
///
/// The three foundation plugins below must go first and in this order: they
/// init the shared registries (AssetRegistry → editor registries → KeyBindings)
/// that every Editor-scope plugin reads inside its own `build()`.
#[cfg(feature = "editor")]
pub fn install(app: &mut renzora::bevy::app::App) {
    app.add_plugins(renzora_asset_registry::AssetRegistryPlugin);
    app.add_plugins(renzora_editor_framework::RenzoraEditorPlugin);
    app.add_plugins(renzora_keybindings::KeybindingsPlugin);

    plugins::add_editor_plugins(app);
}
