# Architecture

Renzora is built around Bevy's ECS: components hold entity data, resources hold shared state, systems perform work, and plugins install related systems. Runtime behavior and editor tooling are separate so games do not need editor-only code.

## Engine and editor

`renzora_runtime` provides the shared runtime assembly. `renzora_editor` installs the editor on top of it. The native engine-plugin build creates two executables: `renzora-editor` and `renzora`. External play mode launches the sibling runtime; both files and their companion files must be staged together.

Ordinary native builds also use this separate executable pair. The editor library is `rlib`-only, and default builds do not enable Bevy dynamic linking. Placing an old editor library beside `renzora` cannot turn a game into the editor. The retired shared-engine loaders and compiled-SDK utilities have been removed. Live extensions use the small C-ABI SDK; engine extensions use generated static builds.

## Plugin registration and shared contracts

Engine crates declare Bevy plugins with `renzora::add!`. The macro checks the plugin type; a build-time generator reads declarations and writes explicit installation calls. It is not a runtime inventory registry. Keep declarations at the top level on their own lines.

```rust
renzora::add!(GameplayPlugin, Runtime);
renzora::add!(GameplayToolsPlugin, Editor);
```

A declared type implements Bevy's `Plugin` and `Default`. A feature needing both scopes uses separate plugins. Runtime plugins run in games and in the editor's runtime world; editor plugins belong only in the editor.

The `renzora` contract crate owns engine-facing types shared across feature crates. Implementations remain in the owning feature crate. For example, a request and its queue can be shared without moving the subsystem that performs the work into the contract.

## Two Rust extension tiers

| Extension | Integration | Applying changes |
|---|---|---|
| Ordinary loose Rust plugin | Standalone C-ABI library; no Bevy types cross the boundary | Background compilation and live replacement |
| In-editor Rust script | Compiled script interface and normal scripting components | Background compilation and live replacement |
| Engine plugin | Explicit `type = "engine"` manifest; real Bevy/Renzora code statically linked into a new generation | Background build, then an explicit restart |

Tier 2 is selected by the manifest, not guessed from Rust source. Its editor and runtime halves are separate crates. See [Native Plugins](../extending/native-plugins.md) for the layout and prerequisites.

These are trusted native extensions, not a sandbox. Interface checks and controlled replacement do not make arbitrary Rust code safe or provide unrestricted Bevy access through a stable C ABI.

## Installed build inputs and caches

The small `rust-sdk/` source package supports ordinary Rust plugins and scripts. It contains the guest SDK and sibling derive/identity sources, not compiled Bevy libraries. The Bevy-independent `renzora_rust_sdk` utility packages it for normal staging and replacement installations.

Tier 2 uses a separate engine build kit containing engine sources, pinned vendored dependencies and required companion files. Its digest is pinned in the editor's embedded build identity. A different self-consistent kit is rejected. The kit requires the matching Rust toolchain and platform build tools; the engine source checkout and global Cargo cache are not build inputs.

Compiler data is stored under per-user caches, not beside an installed executable. Whole-engine builds use a private Cargo home and target directory, stable working copies, and immutable published generations. New source revisions invalidate stale results; a cache lock covers building and staging.

## Restart and recovery

Bevy resources and messages hold build state, approval, diagnostics and restart requests. Workers handle source preparation, compilation and process monitoring outside the frame loop. Settings exposes project approval, progress, retry and restart.

Restart is never automatic. The final gate checks participating editors for unsaved work. A replacement is launched by absolute path while the current editor stays open. It must acknowledge the expected build, executable and project using a one-time token. Only a live, acknowledged replacement can become known-good. Failed or cancelled handoffs retain the old selection and executable.

Known-good and rollback identities are stored together in a checksummed, atomically replaced record. A successful acknowledgement proves startup at that point, not that unrestricted third-party code can never crash later.

## Game exports

Native exports build the current runtime engine-plugin sources in a cache separate from editor candidates. Editor halves are excluded. Lean export preparation changes a disposable workspace, not the installed editor. A matching target kit is required; an unrelated runtime template cannot substitute for missing native features.

The runtime and project data are then packaged by the normal export pipeline. Rust script packaging errors stop export. Modding-enabled exports carry the small Rust source SDK when it is available; repeated exports refresh a recognized SDK without mixing obsolete files.

## Validation boundaries

Acceptance work includes actual Linux editor/runtime builds, rendered script execution, controlled startup and native export behavior. The ordinary static pair also passes startup probes for the editor, runtime, dedicated server and listen server, plus a real Play child-launch regression. Windows cross-compilation and manual Windows graphics testing are separate checks; a successful build does not establish interactive behavior. macOS runtime execution has not been performed on this Linux host; portable path/process tests are not a substitute for it.

## Related documentation

- [Building from Source](building-from-source.md)
- [Native Plugins](../extending/native-plugins.md)
- [Standalone Plugins](../extending/standalone-plugins.md)
- [Rust Scripts](../scripting/rust-scripts.md)
- [Exporting](../exporting/overview.md)
# Shared mesh primitives

`renzora_mesh_primitives` owns the procedural mesh generators used by both the
engine and the shape browser. Their previous `procedural_meshes` module paths
re-export the same functions for compatibility. The engine still registers the
built-in shapes; the editor browser adds icons and its panel. There is no second
registration table in the browser crate. Both original mesh test suites now run
against the shared implementation.
