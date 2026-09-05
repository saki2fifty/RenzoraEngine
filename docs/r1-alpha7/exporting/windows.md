# Export: Windows

Windows games run through `renzora.exe`. The editor is a different executable, `renzora-editor.exe`. Removing a DLL is not how you switch between them, and an old editor DLL beside a game is not loaded as the editor.

## Build and package

Use the [export workflow](overview.md) to package the runtime, selected plugins and project data. For source builds, see [Building from a Checkout](../setup/building-from-source.md). Native desktop staging includes both executables; Docker cross-compilation produces runtime/export artifacts and is not proof of an installed editor's complete engine-build kit.

The Windows cross-toolchain uses the MSVC target, xwin SDK/CRT files and LLVM tools. Run it in the configured Windows build environment; a plain Linux Cargo invocation does not supply those prerequisites. Keep the pinned Rust toolchain consistent with the build kit.

A typical game export contains:

```text
game/
├── renzora.exe
├── renzora.rpak
└── plugins/         # selected standalone runtime plugins, when needed
```

The archive can instead be appended to the executable. Keep any other runtime companions produced by the exporter, including an OpenXR loader for a build that needs it. Default builds do not require `renzora_editor.dll`, `bevy_dylib`, shared Renzora contract libraries or a toolchain-hashed Rust standard-library DLL.

## Editor, servers and extensions

An editor distribution includes **both** `renzora-editor.exe` and `renzora.exe`: external Play launches the sibling runtime. Games omit the editor executable. The runtime also provides `--server` for a dedicated server and `--host` for a listen server.

Ordinary Rust plugins and scripts use the small C-ABI SDK. Full engine extensions require a matching target build kit and are statically built into the exported runtime; editor-only extension code is excluded. See [Native Plugins](../extending/native-plugins.md).

## Distribution checks

Test the package on Windows with the intended graphics drivers and any required Visual C++ runtime installed. Linux compilation or a portable path test is not a Windows execution test.

Use the exporter's supported compression path so executable capability metadata is preserved. UPX compression can trigger antivirus false positives; an uncompressed build is useful for comparison. Signing and testing the final package are separate release steps, not guarantees provided by a successful compile.

If the editor opens instead of the game, check which executable was launched. If assets are missing, keep the archive name consistent with the runtime name or use `--rpak <path>`. If a default build requests old Bevy/Renzora DLLs, check for a mixed or legacy build rather than copying arbitrary cached libraries into the package.
