# Export: macOS

macOS uses the same split as other desktops: `renzora-editor` opens the editor and `renzora` runs the game. Editor distributions keep the pair together for external Play. Game exports omit editor code and do not become games by deleting an editor library.

## Build and package

Use the [export workflow](overview.md) and a template or build kit matching Intel (`x86_64-apple-darwin`) or Apple Silicon (`aarch64-apple-darwin`). These are separate targets; a successful build for one does not validate the other.

Native builds use the [checkout workflow](../setup/building-from-source.md). Configured macOS cross-toolchains can produce runtime artifacts, but require their Apple SDK and compiler prerequisites. A cross-built executable alone does not prove that the installed editor has a usable, matching engine-extension build kit.

An unwrapped game normally contains `renzora`, project data such as `renzora.rpak`, and selected standalone libraries under `plugins/`. An appended archive is also supported. Default builds do not require `librenzora_editor.dylib`, `bevy_dylib`, shared Renzora contract libraries or a toolchain-hashed Rust standard-library dylib. Retain other companions supplied by the exporter.

## Application bundles

The packaging paths can place executables under an `.app` bundle's `Contents/MacOS`. An editor bundle needs both executables there; its launcher selects the editor. A game bundle must select the runtime and include its assets. Preserve executable permissions when copying or archiving the package.

Signing, notarization and testing on the destination Mac are separate release responsibilities. A locally built or cross-built executable is not automatically signed or approved by Gatekeeper. Use the appropriate Apple distribution tooling; do not treat successful compilation as proof of signing or device compatibility.

## Extensions and runtime behavior

Ordinary Rust plugins and scripts use the small C-ABI SDK. Full engine extensions are statically compiled with the matching target kit; editor-only extension code is excluded from game exports. The runtime retains `--server` and `--host` modes.

Test graphics, audio, the minimum supported OS version and the final bundle on the actual target platform. No macOS execution result is implied by Linux validation. If a default package requests old Bevy/Renzora libraries, investigate mixed legacy artifacts instead of supplying arbitrary cached dylibs.

See [Export Templates](../packaging/export-templates.md) and [Asset Packing](../packaging/asset-packing.md) for packaging details.
