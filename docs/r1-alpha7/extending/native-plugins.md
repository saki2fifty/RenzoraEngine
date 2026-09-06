# Native Plugins: Full Engine Access

Use an engine plugin when a feature needs real Bevy systems, rendering or editor APIs. Renzora builds a new editor and game in the background, then offers a restart. Ordinary loose Rust plugins and in-editor Rust scripts keep their live-update workflow.

## Built-in runtime build options

Normal builds include the migrated built-in game features. The runtime crate also exposes individual Cargo features: `spline`, `vignette`, `auto_exposure`, `night_stars`, `procedural_tree`, `text3d`, `pool_water` and `clouds`. Disabling a feature omits its runtime plugin installation and optional dependency; the generated plugin list preserves these choices. Rendering features select their rendering requirements automatically.

The lean exporter removes the migrated renderers when 3D rendering is disabled, and removes 3D text when UI is disabled, so those plugins do not silently re-enable excluded subsystems. Individual plugin selection integration remains in progress. Disabling scripting entirely is a separate, currently broken runtime feature combination.

The export Plugins tab lists these eight features as **Built-in runtime** choices, separately from plugin files. Export presets save both lists and migrate older native-plugin selections. The exporter writes `builtin_runtime_plugins` into the packed project configuration for both client and server. Game startup uses that list instead of local editor enable preferences; an empty list disables all eight. Missing fields retain the old behavior, and editor sessions ignore this game-only selection.

Copy-based exports disable unselected built-ins at startup; their compiled code remains in the copied runtime. Lean builds also remove the unselected runtime dependencies. Disabling 3D rendering removes the seven rendering built-ins, and disabling UI removes 3D text. Before packaging, the exporter checks the actual runtime's versioned feature record. Older or mismatched templates are rejected instead of silently ignoring the selection. Published compressed templates preserve that record; manually compressed older copies may need UPX to inspect a temporary unpacked copy.

## Choose the right tier

| Need | Use |
|---|---|
| Drop a Rust file into the plugin folder and update it live | [Loose C-ABI plugin](plugins.md#loose-single-file-plugins) |
| Attach Rust behavior to scene objects | [Rust script](../scripting/rust-scripts.md) |
| Extend the engine or editor using real Bevy/Renzora types | An explicit engine-plugin directory |

Renzora does not guess the tier from source code. A directory becomes a Tier 2 engine plugin only through `plugin.toml` with `type = "engine"`.

The retired Rust-ABI directory loader and its first-run compiled-SDK setup are
removed. Export no longer offers or copies libraries built for that loader.
Use the engine-plugin manifest and restart workflow for full engine access;
ordinary C-ABI plugins and scripts keep their existing live-update path.

Engine plugins are trusted native code. They can access the process and crash it. Project approval is consent to compile and execute that code, not a sandbox.

## Project layout

```text
plugins/my-feature/
  plugin.toml
  runtime/
    Cargo.toml
    src/lib.rs
  editor/
    Cargo.toml
    src/lib.rs
```

```toml
schema = 1
type = "engine"
id = "com.example.my-feature"

[runtime]
crate = "runtime"

[editor]
crate = "editor"
```

At least one half is required. Omit the other section and crate when it is not needed. Identities must be unique. Half paths are relative to the plugin directory and cannot escape it or overlap another declaration. Unknown fields and unsupported schema versions are errors.

Runtime behavior and editor tooling are separate crates, not a combined scope. The editor installs runtime behavior too; a game includes only runtime halves.

## A runtime half

`runtime/Cargo.toml`:

```toml
[package]
name = "my_feature_runtime"
version = "0.1.0"
edition = "2021"

[dependencies]
bevy = { workspace = true }
renzora = { path = "../../../crates/renzora", default-features = false }

[lints]
workspace = true
```

The relative engine path above is resolved in the generated integration workspace, where the builder stages the plugin. It does not mean your project must contain an engine checkout. Let the editor's engine builder perform the integration.

`runtime/src/lib.rs`:

```rust
use bevy::prelude::*;

#[derive(Default)]
pub struct MyFeaturePlugin;

impl Plugin for MyFeaturePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, update_feature);
    }
}

fn update_feature() {
    // Add the feature's systems here.
}

renzora::add!(MyFeaturePlugin, Runtime);
```

An editor half uses its own package and `renzora::add!(MyFeatureEditorPlugin, Editor);`. Enable the contract crate's `editor` feature when using editor-only contracts. Plugin types implement `Default`. Keep declarations at the top level on their own lines; the build generator reads them as text.

Shared engine-facing types belong in `renzora`; implementation stays in the owning feature crate. Use components, resources, messages and scheduled systems rather than global mutable state. See [Editor Features from Code](editor-api.md) for editor contracts.

## Installed editor requirements

A downloaded editor can support this workflow without a source checkout. It needs the matching engine build kit, its exact Rust toolchain, and platform build tools.

The kit contains engine sources, pinned vendored dependency sources and companion files. The editor's embedded identity pins its digest; a different kit is rejected even if its own checksums are consistent. By default the editor looks beside its executable for `engine-build-kit/`; `--engine-build-kit <directory>` selects another location.

The kit is platform-specific. A Linux acceptance build or Windows cross-check is not a validated Windows-native kit. A release must provide the kit/toolchain pairing for the platform on which the editor will rebuild extensions.

Third-party dependencies must be available in the kit's offline dependency inventory. The builder can extend the lockfile with compatible available packages but cannot silently replace existing engine pins or fetch arbitrary missing dependencies. Missing inputs produce a build diagnostic.

This kit is separate from `rust-sdk/`, the small source package used by live Rust plugins and scripts. The small package contains no compiled Bevy libraries. Both compiler services use writable per-user caches rather than requiring write access beside installed executables.

## Build, restart and recovery

In **Settings → Editor → Plugins**, approve the project's engine code. Source changes queue a background build; rapid saves are combined and stale results cannot become restart-ready. Progress and errors are shown there, with retry and restart controls.

A successful build does not replace the current executable. It publishes a separate editor/runtime generation. Restart is explicit and checks participating editors for unsaved work. The old editor stays open until the new process confirms the expected build and project.

Failed builds and unsuccessful handoffs preserve the prior working selection. Known-good and rollback identities are retained together. The previous executable is not overwritten. This startup safeguard cannot guarantee that unrestricted plugin code will remain healthy after startup.

Changing projects with a different native plugin set also requires a replacement; the existing native world must not be reused with another project's assumptions.

## Games and exports

Exports build current runtime-half sources, not merely whichever version happens to be running in the editor. Editor halves are not included. Builds use an export cache separate from restart candidates.

Native exports require a kit for the requested target and do not require an unrelated prebuilt runtime template. Lean preparation applies size settings to a disposable workspace, leaving editor generations untouched. Unsupported targets or missing inputs are errors rather than a reason to drop native behavior silently.

Script packaging failures stop export. When shipping the small Rust modding SDK, repeated exports replace the recognized package rather than mixing old and new source files.

## Existing native plugins during migration

The legacy `renzora::plugin!` loader and metadata-SDK source files have been removed. Use the manifest-based Tier 2 workflow above for full Bevy access. Changing an old plugin's filename or library extension does not migrate its API.

Replacement build preparation explicitly preserves the eleven existing native distribution plugins: ai_chat, auto_exposure, clouds, gamepad, mesh_draw, night_stars, pool_water, procedural_tree, spline, text3d and vignette. Their editor/runtime scope is preserved through generated static wiring. The retired loader and shared-engine export paths have now been removed.

All eleven first-party native plugins now live in ordinary workspace crates. Spline support lives in `renzora_spline`, retains its shared `SplinePath` scene type and existing `spline` disable preference, and appears as Built-in in Settings. Build-kit preparation uses migrated crates directly instead of adding duplicate copies. This does not remove third-party C-ABI plugin support or the separate restart-required engine-plugin tier.

Old native folders for migrated built-ins are ignored during startup and export discovery, even if files remain from an earlier installation. Their artwork still ships for the Settings cards; disabling a built-in does not load its old DLL as a fallback.

Gamepad diagnostics now live in `renzora_gamepad_editor` and are linked only into the editor. The `gamepad` panel and preference IDs are unchanged; the controller debug panel is not added to game runtime builds.

Vignette rendering lives in `renzora_vignette`; its inspector controls live separately in `renzora_vignette_editor`. Both use the same settings definition from the contract crate. Existing scenes still identify it as `vignette::VignetteSettings`, and the `vignette` enable preference is preserved.

Auto exposure follows the same split: `renzora_auto_exposure` handles runtime metering and compensation, while `renzora_auto_exposure_editor` owns the inspector. The shared settings type, `auto_exposure` preference, artwork and night-darkening controls are preserved.

The starfield uses `renzora_night_stars` plus editor-only `renzora_night_stars_editor`. Existing `NightStarsData`, the `night_stars` preference and seven inspector fields are unchanged. The renderer embeds its shader under the new workspace crate namespace and retains the existing material type name.

Unchanged star settings and camera positions reuse the existing material and dome transform without republishing them. Twinkle animation still uses the shader clock; edits to the star settings, sun elevation or camera position update the relevant values.

Procedural trees use `renzora_procedural_tree` for mesh generation and wind integration, and `renzora_procedural_tree_editor` for the preset and inspector. Shared tree/settings types live in `renzora::procedural_tree` and keep their original scene names. Embedded leaf textures and generator attribution remain with the runtime crate; the `procedural_tree` preference is unchanged.

3D text uses `renzora_text3d` for flat SDF and extruded glyph rendering, with its preset and six inspector fields in `renzora_text3d_editor`. Both use the shared `renzora::text3d::Text3d` component, preserving its original scene type name and settings. The bundled Noto Sans font, custom font selection and `text3d` enable preference remain available.

Pool water uses `renzora_pool_water` for rendering and ripple simulation and `renzora_pool_water_editor` for its 14 inspector fields. Settings have one definition in `renzora::pool_water`, preserving the scene name `pool_water::PoolWater` and the existing `pool_water` enable preference. Shader contents are unchanged; their embedded lookup uses the runtime crate's namespace.

Pool-water height is local to its container, including translated, rotated and scaled containers. Live edits to water level, damping, wave speed and mesh subdivisions now update the existing surface. Changing simulation resolution restarts the ripples on the new grid while retaining the surface and material; zero resolution or subdivisions uses a minimum of one. Unchanged settings reuse their mesh and simulation storage.

Removing vignette or auto-exposure settings only reevaluates routes using that source. A remaining source in the route supplies the fallback; unrelated cameras keep their effects.

Clouds use `renzora_clouds` for rendering and GPU noise baking, with the 28-field inspector in `renzora_clouds_editor`. Both keep the existing shared `CloudsData` and `clouds` preference. The two embedded shaders use the runtime crate namespace; lighting, quality gating and noise-generation behavior are unchanged.

Mesh drawing is editor-only in `renzora_mesh_draw_editor`. Its three toolbar/shortcut IDs (`mesh_draw.box`, `mesh_draw.polyline`, `mesh_draw.join`), preference and saved recipe type names are unchanged. Generated runtime wiring does not depend on this authoring crate.

AI Chat lives in `renzora_ai_chat_editor`, retaining the `ai_chat` panel, settings and preference IDs. It uses the shared UI framework and HTTP contract, and its native folder-picker dependency is excluded on WebAssembly. It is not linked into exported games.

Its built-in guidance distinguishes Lua/Rust scripts, ordinary C-ABI hot plugins,
and restart-required Bevy engine extensions. It no longer recommends retired
Rhai scripts or treats an old hook list as exhaustive. Exact examples must be
grounded in current retrieved documentation; this guidance is not a guarantee
that every generated answer is correct.

## Validation and limitations

Phase 5 acceptance includes full Linux builds from an isolated installed kit, actual runtime/editor scope probes, rendered Rust script execution, startup/restart checks and native runtime export execution. Windows MSVC checks validate compilation; Windows EXE production and Windows/macOS GUI validation have not been performed in this Linux acceptance run.

Engine rebuilds are substantially heavier than live Rust script/plugin builds. On the local Linux test host, measured cold full builds took roughly ten to eleven minutes; an earlier changed-plugin comparison reached about 42 seconds after eliminating unnecessary engine recompilation. These are measurements, not performance guarantees.

For build and staging commands, see [Building from Source](../setup/building-from-source.md).
