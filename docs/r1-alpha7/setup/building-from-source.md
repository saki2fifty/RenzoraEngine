# Building from a Checkout

Use the checkout's build helper for local staging, and the platform toolchain containers for cross-compilation. A successful Cargo compile alone does not place every companion file where the running editor needs it.

## Prerequisites

Use the Rust version pinned in `rust-toolchain.toml`. Keep it in lockstep with `docker/base/Dockerfile` when changing the pin. Native builds also need the platform's C/C++ toolchain and system libraries; the Linux image lists its X11/Wayland, audio and device-library development inputs.

Build and test with non-debug profiles. For this fork, ordinary validation uses `--profile dist`. The standalone `xtask` helper uses `--profile release`.

## Local build and staging

From the repository root:

```sh
cargo run --profile release --manifest-path xtask/Cargo.toml -- dist
```

This invokes the same helper behind `cargo renzora dist`, while explicitly selecting its release profile. It synchronizes plugin wiring, builds the normal checkout configuration and stages the platform directory without launching it. Omit `dist` to use the helper's build-and-run mode.

The desktop helper builds `renzora_app` and `renzora_editor_app` together and stages both executables. It opens `renzora-editor` by default; explicit `--server`, `--host` and `--vr` launches use `renzora`. `cargo dist` compiles this pair without staging. Do not substitute a whole-workspace build: retained legacy dylib packages are not part of the supported desktop graph.

Default native builds link the engine statically. The installed engine-plugin build uses the same executable split in an isolated, reproducible workspace, as described below. Old shared libraries are not substitutes for either executable.

Normal staging includes the small `rust-sdk/` source package for live Rust plugins and scripts. It no longer prepares the compiled Bevy metadata SDK or builds Rust-ABI plugins. To refresh only the small package:

```sh
cargo run --profile release --manifest-path xtask/Cargo.toml -- source-sdk --out <installation-directory>
```

Linux and macOS bundles keep this source package beside the executable. Compiler output goes into per-user cache storage, so the installation can remain read-only.

The former `sdk` command no longer builds a compiled Bevy metadata archive. It
exits with a migration message and creates no output. Use `source-sdk` for live
Rust plugins/scripts; restart-required engine extensions use the separate
engine-plugin build kit, not the retired metadata archive.

## Validation

```sh
cargo check --profile dist -p <crate>
cargo test --profile dist -p <crate>
cargo clippy --profile dist -p <crate> --lib --no-deps -- -D warnings -A clippy::too_many_arguments -A clippy::type_complexity
```

Select affected crates together when practical to avoid rebuilding shared dependencies with different feature combinations. The narrow library lint command is not a claim that all integration tests passed.

This fork's workspace-wide tests have known unrelated vendored XR example failures. Per-crate results should identify their scope and any ignored tests; do not report a workspace pass from a smaller suite.

## Building an installed engine-plugin kit

The Phase 5 release helpers separate source preparation, vendoring, packaging and actual editor/runtime building:

1. `prepare_kit_sources` copies the engine into a new workspace, enables the native editor/runtime configuration there, preserves the eleven legacy native plugins through static wiring, and resolves the additional pinned dependencies offline.
2. Vendor the prepared workspace's complete locked dependency graph using the release environment. Missing packages must be acquired before declaring the kit ready; the installed editor does not borrow the developer's Cargo cache.
3. `package_rust_sdk` supplies the small guest source package. Collect platform-reviewed runtime companions, including standalone plugin libraries.
4. `package_kit` produces a new kit with an inventory, content hashes, sizes, engine identity, target, profile and exact compiler identity.
5. `build_installed_kit` builds a staged editor/runtime generation from that kit and the selected plugin root, with a private Cargo home and output cache. For a baseline distribution, provide an empty plugin directory.

These helpers are examples in `renzora_engine_plugins`; run them with `cargo run --profile dist -p renzora_engine_plugins --example <name> -- <arguments>`. Their usage strings name the required paths. Output directories for source preparation and packaging must be new, not the engine checkout or an existing installation.

The resulting native generation contains `renzora-editor`, `renzora`, the small source SDK and verified companions. Distribute the matching kit as `engine-build-kit/` beside the editor, or launch with `--engine-build-kit <directory>`. The embedded kit identity must match. Never edit kit contents after packaging.

The installed build needs the exact Rust toolchain and native system build tools. The kit supplies engine and dependency sources, not the compiler or a large precompiled Bevy cache. First builds are expensive; later builds reuse stable cached engine inputs.

## Cross-platform builds

The `docker/` toolchains support cross-compiling runtime/export artifacts. A cross-check such as the following validates Windows compilation without producing an editor EXE:

```sh
cargo check --profile dist -p renzora_engine_plugins --target x86_64-pc-windows-msvc
```

Run it inside the configured Windows toolchain environment. The target, linker and native libraries must already be available.

A cross-built executable is not by itself a validated installed Tier 2 release. A kit must match the platform and exact toolchain that will rebuild extensions on the user's machine. The legacy metadata SDK has additional host-proc-macro constraints; it must not be substituted for the new source kit.

Validation includes full Linux editor/runtime builds, startup probes under a virtual display and a real Play child-launch test. Windows MSVC compilation checks do not replace graphics and interaction testing on a Windows machine. No macOS GUI run is claimed. See [Cross-compilation](../packaging/cross-compilation.md) and [Export Templates](../packaging/export-templates.md) for the platform-specific packaging paths.

## Runtime and project extensions

The native replacement editor launches the sibling `renzora` runtime for external play. The dedicated server remains the runtime launched with `--server`; it is not a third executable.

Loose Rust plugins and in-editor Rust scripts use the shared cached compiler service and small guest SDK. They do not trigger a full Bevy rebuild. Project directories explicitly declaring `type = "engine"` use the restart-required engine build path.

- [Architecture](architecture.md)
- [Building Plugins](../extending/plugins.md)
- [Native Plugins](../extending/native-plugins.md)
- [Rust Scripts](../scripting/rust-scripts.md)
