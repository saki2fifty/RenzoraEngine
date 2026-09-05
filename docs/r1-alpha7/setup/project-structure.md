# Project Structure

Renzora is a Bevy ECS workspace. Feature crates install systems through plugins; shared contracts keep their data definitions consistent. The editor and game runtime are separate executables, not two roles selected by a neighboring DLL.

## Main directories

```text
engine/
├── Cargo.toml                   # workspace and renzora_app runtime package
├── src/main.rs                  # game, server, host and VR startup
├── crates/
│   ├── renzora/                 # shared engine contract types
│   ├── renzora_runtime/         # runtime assembly and generated plugin wiring
│   ├── renzora_engine/          # game core, assets, scenes and crash reporting
│   ├── renzora_editor/          # static editor assembly and generated wiring
│   ├── renzora_editor_app/      # renzora-editor executable
│   ├── renzora_plugin/          # standalone guest API and C-ABI host
│   ├── renzora_compiler_cache/  # background compiler service
│   ├── renzora_loose_plugins/   # single-file Rust plugin integration
│   ├── renzora_rust_script/     # compiled per-entity Rust scripts
│   ├── renzora_engine_plugins/  # restart-required engine extensions
│   └── renzora_<feature>/      # feature implementations and editor tooling
├── plugins/                     # independent standalone plugin projects
├── xtask/                       # native build, staging and wiring helper
├── docker/                      # platform build environments and scripts
├── templates/                   # platform packaging templates
├── assets/                      # engine assets
├── dist/                        # staged build outputs
└── docs/                        # versioned documentation
```

The root workspace uses `crates/renzora_*` and nested editor globs, with explicit vendored dependencies. Consult `Cargo.toml` for the actual member list. `xtask` is deliberately a separate workspace so it can repair generated dependency wiring even when an engine dependency path is missing.

## Executable and contract boundaries

| Package | Responsibility |
|---|---|
| `renzora_app` | Builds `renzora`: games, dedicated `--server`, listen `--host`, and optional VR |
| `renzora_editor_app` | Builds `renzora-editor` and installs the static editor |
| `renzora_runtime` | Common runtime assembly used by both executables |
| `renzora_editor` | `rlib` containing editor-only plugin installation |
| `renzora` | Single definitions of shared engine-facing types and registries |

Build the two desktop packages together to reuse common dependency compilation. Keep the staged executables together for external Play. Default builds do not require shared Bevy or Renzora contract libraries. Retained legacy packages are not the supported desktop build graph.

## Runtime and editor halves

A feature needing gameplay behavior and authoring tools uses separate Runtime and Editor plugins. Existing nested `editor/` crates remain supported; newer editor crates can be flat siblings such as `renzora_clouds_editor`. Runtime plugins run in both games and the editor's runtime world, while editor-only plugins stay out of the game's dependency graph.

Workspace plugins declare `renzora::add!(MyPlugin, Runtime)` or `renzora::add!(MyToolsPlugin, Editor)` on a top-level source line. The generator maintains dependencies and the committed `renzora_runtime/src/plugins.rs` and `renzora_editor/src/plugins.rs`. Do not create a second manual registration list.

## User extensions

Ordinary standalone plugins are C-ABI libraries that do not link Bevy. Loose `.rs` plugins and in-editor Rust scripts compile through the small guest SDK and shared background service. Engine plugins explicitly declare `type = "engine"` and provide separate runtime/editor crates; they are statically compiled into a replacement executable pair and require restart.

The eleven former first-party Rust-dylib plugins now live in workspace crates. Their optional runtime features and editor halves preserve their roles without loading Bevy across a DLL boundary. The standalone libraries under `plugins/` remain a separate mechanism.

See [Building Plugins](../extending/plugins.md) for crate declarations and [Native Plugins](../extending/native-plugins.md) for project engine extensions. Use `renzora::*` or named contract imports; there is no `renzora::prelude`. Inspector contracts require the contract crate's `editor` feature.

## Related pages

- [Architecture](architecture.md)
- [Building from a Checkout](building-from-source.md)
- [Rust Scripts](../scripting/rust-scripts.md)
- [Exporting](../exporting/overview.md)
