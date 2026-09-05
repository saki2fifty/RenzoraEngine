# Changelog

This page gives a plain-English overview of changes made on this fork. The newest changes are always listed first. Times are recorded in UTC.

## September 5, 2026 — 14:42 UTC

- Moved mesh-drawing tools into the editor, preserving shortcuts, saved construction recipes and undo/redo while keeping authoring code out of games.

## September 5, 2026 — 14:31 UTC

- Moved pool-water rendering into the engine with separate editor controls, preserving saved settings, ripple simulation and water shaders.
- Separated volumetric clouds from their editor controls, retaining atmosphere lighting, noise baking, saved settings and quality controls.

## September 5, 2026 — 14:06 UTC

- Separated 3D-text rendering from its editor controls while retaining the bundled font, flat and extruded modes, and saved text settings.

## September 5, 2026 — 13:31 UTC

- Separated vignette rendering from its editor controls while retaining saved scene settings and the existing enable switch.
- Moved automatic exposure into the engine and kept its inspector editor-only, preserving night-darkening settings and cached exposure curves.
- Separated the night-star renderer from its editor controls while preserving its shader, artwork and saved settings.
- Moved procedural trees into runtime and editor crates, preserving seeded shapes, embedded leaf textures, wind settings and saved tree data.

## September 5, 2026 — 13:05 UTC

### Built-in plugin migration

- Moved spline support into a normal engine crate while preserving scene data, curve calculations, and its existing enable/disable setting.
- Added shared startup reporting for migrated built-in features and kept replacement build kits from duplicating them.
- Preserved built-in plugin artwork and prevented old installed copies from being loaded again after an upgrade.
- Moved gamepad diagnostics into the editor while preserving its panel, controller readings, and enable/disable setting.

## September 5, 2026 — 05:59 UTC

### Restart-required engine plugins

- Added a small source package for ordinary Rust plugins and scripts so replacement editors do not depend on a developer checkout.
- Use the same small SDK packager for normal editor builds and replacement builds, keeping both Rust authoring workflows available.
- Prevent installed editors from unexpectedly rebuilding distribution plugins from the directory they were launched in.
- Keep compiler caches in your user cache directory so installed editors do not need write permission beside their executable.
- Connected advanced-plugin status, explicit project approval, and restart controls to the editor.
- Added checks for newly unsaved work during restart and suppressed native-code approval prompts for projects without engine plugins.
- Connected native game exports to current engine-plugin sources and kept their builds separate from editor restart candidates.
- Stop exports when Rust scripts cannot be packaged, instead of silently producing an incomplete game.
- Refresh the Rust modding source package when exporting again to the same folder, and report packaging failures instead of silently dropping it.
- Allow engine-plugin projects to build their game runtime without requiring a separate prebuilt export template.
- Preserved the open project while a different native-plugin setup is prepared for a project switch.
- Replacement builds can now carry verified companion files, rather than only the editor and game executables.
- Exclude temporary plugin-reload copies and compiler caches from release build kits.
- Corrected replacement executable names when building for a different operating system.
- Isolated advanced-plugin build caches from the user's global Cargo cache, with a real small-program build-and-run test.
- Added repeatable build-kit packaging and stable compiler paths to avoid unnecessary rebuilding when a plugin changes.
- Keep the original editor open until its replacement confirms startup, with checks for failed, cancelled or outdated handoffs.
- Preserved the eleven existing advanced plugins in the replacement build, and continued safeguards for project approval and unsaved editor work.
- Connected filesystem-change notifications and combined rapid saves into background builds.
- Verified that the replacement editor runs both plugin types while its game runtime excludes editor-only plugins.
- Hardened compiler selection and added cancellation support for an acknowledged but unfinished editor restart.
- Kept the existing editor launch path available during the migration; old loader removal remains a later cleanup.
- Tightened Linux standalone-plugin builds to reject missing runtime dependencies before packaging, and enabled optimization needed by the small built-in plugins.
- Preserved Linux flocking and widget plugins by including their required runtime support, and refreshed plugin lockfiles without upgrading third-party packages.
- Verified real Linux installed-editor builds, live Rust script execution, restart handoff and runtime-only native exports. Windows compilation checks passed; Windows EXE and macOS GUI testing were not part of this run.
- Updated the plugin and build documentation to describe the current architecture instead of the obsolete registration model.

## September 4, 2026 — 21:51 UTC

### Full engine plugin foundation

- Added an explicit declaration format for advanced plugins that need complete Bevy and Renzora access.
- Kept ordinary live Rust plugins distinct so Renzora never guesses which compilation model a file requires.
- Required separate editor and game/runtime halves to prevent editor-only code from entering exported games.
- Added early checks for invalid identities, duplicate plugins, unsafe paths, and unsupported declarations.
- Added a version-matched build-kit inventory that detects incompatible compilers, altered files, and incomplete installations before rebuilding the editor.
- Added isolated build workspaces that reuse identical inputs, keep plugin source untouched, and rebuild only when the kit or plugin content changes.
- Added immutable editor/runtime generations so a completed build can be verified and offered for restart without replacing the running program or losing the previous candidate.
- Connected advanced plugin runtime and editor halves to their matching executables while rejecting unsafe, missing, or mixed-up declarations before a build starts.
- Added background engine-plugin builds where newer saves replace older work, failures preserve the last working candidate, and the editor remains responsive.
- Fixed cancelled Rust compiler processes on Linux and macOS sometimes remaining stuck as “in progress.”
- Added the editor-facing progress and diagnostics foundation for advanced plugin builds without putting compilation or file-copy work in the frame loop.
- Fixed a timing issue in that foundation that could offer an outdated build for restart after newer work had been requested.
- Clear advanced-plugin build progress when closing a project, and ignore late preparation results from a previous project.
- Added restart groundwork that can launch a selected editor without closing the current one, and verify an older saved build even after a newer build is ready.
- Added startup-confirmation groundwork so only an acknowledged build can replace the saved known-good selection, while retaining the previous build for recovery. Editor integration remains in progress.
- Brought native-editor build preparation into the current migration phase while preserving the existing editor launch path.
- Bind advanced-plugin build records to the copied source and keep delayed results associated with the correct request.
- Resolve new plugin packages without changing the engine's pinned dependencies, and protect shared build outputs while they are staged.

## September 4, 2026 — 19:47 UTC

### Cached Rust scripts

- Rust files in a project can compile in the background and update without restarting the editor.
- Rapid saves keep only the newest submitted version, while a failed edit leaves the last working script active.
- Rust scripts and loose Rust plugins share one compiler and build cache instead of running duplicate services.
- Opening, switching, or closing a project now updates the script file watcher automatically.
- Old script versions remain safely loaded until running work has finished using them.
- Normal exported games and servers do not start the source compiler unless source modding is enabled.
- Updated the Rust scripting and build-cache documentation to describe the supported workflow.

## September 3, 2026 — 22:00 UTC

### Safe live Rust plugin updates

- Added live compilation and replacement for lightweight Rust plugins placed in the plugins folder.
- Prevented failed or outdated builds from replacing the last working plugin.
- Added safer plugin identity, loading, rollback, cleanup, trust, and export handling.
- Kept unrestricted engine extensions available as restart-required engine plugins.

## September 1, 2026 — 19:38 UTC

### Faster Rust plugin compilation

- Added a shared background compiler for Rust plugins and scripts.
- Reused unchanged build results instead of compiling the same source again.
- Prevented outdated builds from replacing newer edits.
- Added safer staging, recovery, cleanup, and storage limits for compiled results.
- Added documentation for how the build cache works and how it is maintained.

## August 31, 2026 — 19:45 UTC

### Reliable plugin and script identity

- Gave every Rust plugin and editor script a stable project-relative identity.
- Allowed files with the same name to coexist safely in different folders.
- Improved handling for renamed, moved, and deleted files.
- Kept editor builds and exported projects in agreement about which script is which.
- Improved protection against invalid paths and accidental identity conflicts.
