# Export: Linux

Linux games use `renzora`; the editor uses `renzora-editor`. An editor distribution keeps both files together because external Play starts the sibling runtime. A game export contains the runtime, selected runtime plugins and project data, not the editor executable.

## Build and package

Use the [export workflow](overview.md) for project packaging and [Building from a Checkout](../setup/building-from-source.md) for native builds. `cargo renzora dist` builds and stages the desktop pair without launching it. Native staging does not require Docker; configured containers remain useful for cross-compiling runtime/export targets.

Select the template or build kit matching the destination architecture. Do not treat an x64 binary as an ARM64 binary merely by moving it into another platform directory.

```text
game/
├── renzora
├── renzora.rpak
└── plugins/         # selected standalone runtime libraries, when needed
```

The archive can instead be appended to the executable. Default builds statically link Bevy and the engine contract; they do not require an editor `.so`, `bevy_dylib` or a toolchain-hashed Rust standard-library `.so`. Keep other companions supplied by the exporter.

## Assets and permissions

An adjacent archive follows the executable stem: `renzora.rpak` for `renzora`, or `mygame.rpak` if the executable is renamed to `mygame`. An explicit `--rpak <path>` can select the archive instead. See [Asset Packing](../packaging/asset-packing.md).

The build and export paths preserve executable permissions, including appended-archive exports. A manual copy through a filesystem that loses permissions may require restoring the executable bit.

## AppImage and runtime requirements

The packaging tools can wrap an editor pair in an AppDir/AppImage when their prerequisites are available. A game AppDir must launch the runtime and contain its project data; deleting an old editor library is not a conversion procedure. Test the final wrapper, not only the unwrapped binary.

Static engine linking does not eliminate operating-system dependencies. A graphical game still needs a working graphics backend and drivers, an available window system, and the system libraries required by its enabled features. Audio and XR have their own device/runtime requirements.

## Extensions and servers

Ordinary Rust plugins and scripts use the small C-ABI SDK. Full engine extensions require the matching target kit and are statically included in the game; their editor halves stay out. The runtime supports dedicated `--server` and listen `--host` modes.

If a default package requests old Bevy/Renzora shared libraries, check for mixed legacy artifacts. If the wrong window opens, verify that you launched `renzora`, not `renzora-editor`. For missing assets, check the archive name and project configuration before changing build features.
