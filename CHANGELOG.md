# Changelog

This page gives an overview of changes made on this fork. The newest changes are always listed first. Times are recorded in UTC.

## September 7, 2026 — 18:18 UTC

- Release packaging now rejects reused output folders and duplicate platform inputs instead of risking mixed or ambiguous downloads.
- Linux folder-based packages now leave out retired SDK files and engine libraries while keeping current plugins and the source SDK.
- Incomplete runtime outputs now stop packaging before any platform archive is created, including missing web runtime files.
- Source downloads now use the commit recorded in release metadata, and metadata safely handles special characters.
- A failed source archive now stops release packaging instead of silently dropping the source download.

## September 7, 2026 — 16:50 UTC

- Added checksum verification before installing the release compression tool.
- Added automated checks for release package contents and download checksums. No new release or Windows build was produced.
- Clarified release and Docker build instructions: the retired SDK no longer prevents cross-building an editor, and the editor is a separate executable.

## September 7, 2026 — 16:21 UTC

- Retired the final three old work branches after verifying their intended improvement is already covered by the current implementation. Only main remains.

## September 7, 2026 — 16:04 UTC

- Removed 15 completed work branches to simplify navigation; their changes remain in main. Kept three unmerged branches for reference.

## September 7, 2026 — 15:48 UTC

- Added a prominent, clickable changelog banner at the top of the README.
- Simplified the changelog's introductory wording.

## September 7, 2026 — 15:43 UTC

- Made the changelog easier to find with a short introduction and link at the very top of the README.

## September 7, 2026 — 14:30 UTC

- Improved automated shutdown checks to verify that compiler processes are cleaned up correctly, including graceful exits.
- Stopped checking the UI Editor's create-canvas button while its panel is hidden.

## September 7, 2026 — 14:03 UTC

- Repaired outdated undo-history and network test setup so automated checks can validate the current engine again.

## September 7, 2026 — 13:18 UTC

- Directed newly built editors' update checks, export downloads, repository links, and GitHub statistics to this independent fork.
- Updated installation guidance and added checks against reconnecting to the original project's repository.

## September 7, 2026 — 13:04 UTC

- Separated this fork's automated checks, container publishing, and documentation archives from the original project's infrastructure.
- Fixed script-identity code checks reported by GitHub validation.

## September 7, 2026 — 04:05 UTC

- Fixed Lumen lighting updates being lost when several meshes change at once.
- Refreshed lighting samples after mesh or material asset edits, and stopped rebaking unchanged empty meshes.

## September 7, 2026 — 03:58 UTC

- Reduced Lumen timing-statistics bookkeeping while preserving the same rolling average.

## September 7, 2026 — 03:54 UTC

- Corrected remaining ragdoll and splat-rendering descriptions that referred to retired plugin loading.
- Brought native-plugin guidance up to date with supported export selection and scripting-free builds.

## September 7, 2026 — 03:51 UTC

- Fixed audio timeline duration discovery when the audio backend starts late.
- Removed repeated clip-path copies and unnecessary timeline refresh signals from settled audio clips.

## September 7, 2026 — 03:47 UTC

- Reused unchanged scene-loading totals instead of repeatedly counting files and looking up archive sizes.

## September 7, 2026 — 03:38 UTC

- Avoided unnecessary cloth wind updates while preserving live wind changes and additional forces.
- Corrected cloth documentation to describe its current built-in engine integration.

## September 7, 2026 — 00:01 UTC

- Removed unused UI-helper warnings from lean builds while preserving full markup UI support.
- Stopped unchanged collision snapshots from triggering unnecessary downstream updates.
- Avoided republishing unchanged loading-status snapshots while preserving elapsed-time updates.

## September 6, 2026 — 23:48 UTC

- Fixed one-shot script timers firing repeatedly and avoided unnecessary updates for settled timers.
- Removed a duplicate hardware-monitoring dependency and its unused Windows support packages.
- Updated the built-in AI assistant's guidance for current Rust scripts and the two plugin types.

## September 6, 2026 — 23:22 UTC

- Kept the editor open and showed an error when a Marketplace restart cannot launch.
- Avoided repeated night-star material and position updates while the scene is unchanged; twinkle animation is preserved.
- Improved reuse of unchanged engine-build files across build-kit updates and cleaned up a Windows build warning.
- Corrected outdated Solari and profiling instructions that still described the removed engine DLL architecture.
- Made multiplayer servers honor their configured listening address and report failed startup accurately.
- Added optional mouse-motion performance logs to help investigate viewport slowdowns without changing mouse controls.

## September 6, 2026 — 23:08 UTC

- Fixed pool-water height and live setting updates, while reusing unchanged surfaces.
- Kept unrelated cameras' effects intact when removing vignette or auto-exposure settings.

## September 6, 2026 — 22:55 UTC

- Fixed a ray-traced lighting startup warning and made shadow suppression respect light settings when switching on or off.

## September 6, 2026 — 21:56 UTC

- Clarified how to build standalone plugins with the existing settings, avoiding libraries that build successfully but cannot load.

## September 6, 2026 — 21:40 UTC

- Made the automatic update check wait for networking during startup, while keeping manual update checks and retries available.

## September 6, 2026 — 20:31 UTC

- Delayed the splash screen's optional GitHub lookup until networking is ready, avoiding an unnecessary timeout during slow startup.

## September 6, 2026 — 15:16 UTC

- Removed unused older FBX import code while retaining the current importer. Lean scripting builds no longer request a JSON dependency used only by blueprints.

## September 6, 2026 — 15:15 UTC

- Bounded multiplayer retry memory and receive work, enforced the configured player limit, and added explicit reporting when messages cannot be queued. Corrected multiplayer documentation to describe the features actually available.

## September 6, 2026 — 15:14 UTC

- Reduced repeated work in unchanged world-space UI and script scene-name lookups. Scene edits, font changes and script cleanup still refresh the affected data.

## September 6, 2026 — 14:32 UTC

- Reduced repeated work in world-space text, script timing, navigation updates and network retries. Added regressions that check retained storage and unchanged-frame behavior.

## September 6, 2026 — 14:30 UTC

- Repaired plugin compatibility tests and updated automated checks for the current architecture, including smaller runtime configurations and separate optimized-build caches.

## September 6, 2026 — 14:22 UTC

- Replacing a font now refreshes world-space text. Custom hierarchy filters and icons follow changes while unchanged frames keep their cached results.

## September 6, 2026 — 14:07 UTC

- Physics impulses now add to existing motion according to body mass, respect locked axes, and leave static/kinematic bodies unchanged in both 2D and 3D.

## September 6, 2026 — 13:58 UTC

- Audio follows plugin replacements and clears obsolete playback handles. Machines without usable audio retry gradually; dedicated servers no longer try to open speakers.

## September 6, 2026 — 13:54 UTC

- Failed directory-plugin replacements now keep their working panels and backend registrations; successful replacements share the checked reload path used by loose Rust plugins.

## September 6, 2026 — 13:42 UTC

- Cleared five warnings in shared engine tests so strict checks can cover those tests again, without changing editor behavior.

## September 6, 2026 — 13:37 UTC

- World-space UI no longer leaves an old background visible after its last background is removed, made transparent, or reduced to zero size.

## September 6, 2026 — 13:27 UTC

- Fixed minimal runtime builds failing on optional scripting helpers, without changing normal editor scripting.

## September 6, 2026 — 09:14 UTC

- Completed and validated this performance-improvement batch across scripts, audio, UI, sprites, tilemaps and other editor/runtime systems.
- Documented the measured reductions in repeated work and the larger improvements still remaining; Windows frame-rate testing has not been performed.

## September 6, 2026 — 09:01 UTC

- Limited tile-collider maintenance to affected layers, including tile moves, sheet removal and disable/reenable changes.

## September 6, 2026 — 08:52 UTC

- Kept unrelated editor rows and tooltips from repeatedly rebuilding the scene hierarchy, while preserving scene-parent and visibility-in-the-tree changes.

## September 6, 2026 — 08:43 UTC

- Shared loading-status snapshots between scripts, avoiding repeated path copies for scripts that never request them.

## September 6, 2026 — 08:39 UTC

- Avoided repeatedly sending unchanged sound positions, while retrying failed movement updates and refreshing replacement audio backends.

## September 6, 2026 — 08:33 UTC

- Combined per-frame audio updates so sound-finished notifications are handled reliably, and reused the update buffers instead of rebuilding them.

## September 6, 2026 — 08:19 UTC

- Avoided rebuilding unchanged audio mixer bus lists while keeping live meters and mixer edits responsive.
- Resent the current mixer settings when an audio backend is newly adopted, even if no fader has changed.

## September 6, 2026 — 08:12 UTC

- Shared common frame data across built-in script executions, avoiding repeated input copies while preserving existing custom backends and script behavior.

## September 6, 2026 — 07:59 UTC

- Stopped recalculating unchanged sprite-sheet crops while preserving late image loading, image replacement, frame edits, and re-enabled objects.

## September 6, 2026 — 07:56 UTC

- Reused one startup graphics capability check for ray tracing and integrated-graphics hints, and corrected stale runtime architecture descriptions.

## September 6, 2026 — 07:45 UTC

- Preserved UI ordering and sprite/tile refresh when objects are disabled, edited, and re-enabled, including custom disabling rules.

## September 6, 2026 — 07:26 UTC

- Recalculated UI sibling ordering only for affected parent groups, retaining updates for reordering, reparenting and component edits.

## September 6, 2026 — 07:17 UTC

- Corrected remaining architecture descriptions for lighting, post-processing and shared engine contracts, and tidied the shared-mesh documentation.

## September 6, 2026 — 07:13 UTC

- Cleaned up optional-build warnings and corrected the physics backend documentation to match current builds.

## September 6, 2026 — 07:10 UTC

- Reused loading-progress filename storage instead of copying unchanged paths every frame, and clarified completion and elapsed-time behavior.

## September 6, 2026 — 07:05 UTC

- Released navigation target records when agents or their required components are removed, including during scene teardown.
- Reused connected gamepads' script-input storage while clearing disconnected slots and preserving button edges.

## September 6, 2026 — 06:58 UTC

- Stopped an unchanged empty scene hierarchy from rebuilding continuously. Later scene edits still refresh the tree.

## September 6, 2026 — 06:49 UTC

- Skipped repeated atlas-region and depth-sorting calculations for settled sprites, while retaining updates after edits and movement.
- Used each tile layer's existing child list for collider rebuilding, avoiding repeated searches through other layers' tiles.

## September 6, 2026 — 06:43 UTC

- Kept settled wind from repeatedly rewriting water settings, while preserving authored wave patterns and live wind changes.

## September 6, 2026 — 06:38 UTC

- Reused world-space UI working buffers and avoided copying unchanged labels. Font and text-opacity edits now participate in its rebuild check.
- Reduced audio timeline bookkeeping copies while preserving active clips, seek handling, and duration trimming.

## September 6, 2026 — 06:31 UTC

- Reused global-illumination geometry sample storage and skipped unchanged uploads, while continuing to check moving objects, camera coverage, and shading edits.

## September 6, 2026 — 06:22 UTC

- Stopped settled tile objects from repeating bake checks, while retaining retries for late atlas loads and keeping tileset sampling crisp after image changes.
- Stopped unchanged water shading values from repeatedly notifying the renderer, while retaining sun and texture updates.

## September 6, 2026 — 06:06 UTC

- Kept script components attached during execution and shared their read-handler name lookup, reducing repeated structural changes and copying.
- Stopped the hidden System Profiler from refreshing its view data; opening it resumes updates.
- Consolidated duplicated mesh generators so the editor and game use one implementation, preserving existing shapes and import paths.

## September 6, 2026 — 05:46 UTC

- Kept existing collision snapshots when contacts stay the same, avoiding repeated set rebuilding during settled contact.

## September 6, 2026 — 05:36 UTC

- Reused collision notification name buffers instead of repeatedly discarding and recreating them.

## September 6, 2026 — 05:27 UTC

- Stopped unchanged physics velocity readings from repeatedly reporting changes, while preserving live updates and backend switching.

## September 6, 2026 — 05:21 UTC

- Stopped unused plugin materials from searching the entire scene for settings, and reused temporary working memory while preserving material updates.

## September 6, 2026 — 04:54 UTC

- Stopped unchanged runtime UI scale values from repeatedly marking layout inputs as changed, while preserving resize and canvas updates.
- Removed unnecessary temporary collision lists while keeping the same contact notifications.

## September 6, 2026 — 01:10 UTC

- Removed the obsolete compiled-engine SDK exporter. Live Rust plugins and scripts keep their small source package; engine extensions keep their separate build kit.
- Removed the retired Rust plugin loader and its duplicate setup windows. Export no longer lists old plugin libraries that the current runtime cannot use.
- Stopped release packaging from including the retired SDK archive and corrected the release setup instructions. Existing input files are left intact.
- Removed the old shared-engine build option and its libraries. Normal staging no longer searches for or copies those retired engine files.
- Removed unused legacy compiler helpers while preserving built-in plugin artwork in editor and engine-extension packages.
- Updated optional compression to recognize the current editor/runtime package layout, and stopped Docker staging from copying retired shared-engine libraries.
- Fixed newly created Rust scripts to use the current script SDK instead of the retired engine interface.
- Removed the retired Rust extension declarations and stopped modding exports from shipping the obsolete large SDK as a fallback.
- Kept retired engine libraries out of game exports and runtime templates while preserving OpenXR support files.
- Removed the obsolete Windows build-profile override; cross-builds now honor the requested profile.
- Removed the final unused legacy script helpers and updated architecture, panel, and export documentation for the two-tier extension system.
- Full editor ZIPs also exclude stale shared-engine libraries without deleting the original staging files.

## September 6, 2026 — 00:00 UTC

- Reduced background disk activity by watching for plugin updates instead of repeatedly scanning every plugin. Recovery checks still catch missed changes.
- Made editor-built plugin updates appear only after copying finishes, keeping the previous file intact if copying fails.

## September 5, 2026 — 23:52 UTC

- Made plugin reload retries use separate files, preventing them from overwriting code that is still running.
- Protected reload files belonging to other running editors. Leftover files from completed sessions are cleaned up when a new session starts.

## September 5, 2026 — 23:06 UTC

- Stopped plugin reloads from accumulating inactive systems and their working memory. A 1,000-reload test keeps one active system rather than growing the list.
- Prevented rejected plugin code from coming back on a later retry, and preserved new systems registered while their schedule is running.

## September 5, 2026 — 22:51 UTC

- Allowed independent Rust plugin systems to run in parallel instead of waiting on services they do not use. Systems that share writable data still run safely in turn.
- Preserved existing plugins' access; rebuilding with the updated plugin SDK enables the narrower scheduling automatically. New plugin builds require a compatible updated editor or runtime.

## September 5, 2026 — 21:34 UTC

- Reduced repeated plugin memory allocation by reusing call tables and command lists, while keeping each call's data separate. Added checks for failed calls and resources being removed and restored.
- Corrected a memory-alignment assumption when reading plugin mesh commands.

## September 5, 2026 — 21:14 UTC

- Began reducing plugin processing overhead by reusing query memory between frames and avoiding temporary allocations for every component read. Broader performance and reload improvements remain in progress.

## September 5, 2026 — 20:53 UTC

- Moved ordinary builds to separate editor and game executables, matching the engine-extension builds. Play starts the companion game executable.
- Removed the startup path that could turn a game into the editor when an old editor library was present beside it.
- Corrected a headless-server startup failure caused by enabling Gaussian-splat rendering without a graphics renderer; the rebuilt server passed its startup check.
- Built a separate Windows test package with both executables and all bundled plugin DLLs, ready for manual testing without replacing the existing installation.

## September 5, 2026 — 19:46 UTC

- Completed the move of all eleven built-in native plugins into the engine and editor, preserving their roles, saved settings and scene data.
- Connected the eight built-in game features to export choices and saved presets. Packaged games keep those choices independently of local editor settings, and lean builds leave unselected code out.
- Added checks that stop older or mismatched runtime templates from silently ignoring export choices, including compressed release templates.
- Preserved executable permissions when creating single-file Linux and macOS games.

## September 5, 2026 — 15:08 UTC

- Made the eight migrated game features optional at build time, while keeping them enabled in normal builds. Export-screen integration is still in progress.
- Added a game-specific built-in selection setting so a packaged game's choices can take precedence over local editor preferences without changing them.

## September 5, 2026 — 14:42 UTC

- Moved mesh-drawing tools into the editor, preserving shortcuts, saved construction recipes and undo/redo while keeping authoring code out of games.
- Moved AI Chat into the editor workspace, retaining its panel, provider settings, streamed replies and manual retrieval.
- Corrected standalone-plugin examples that still described an obsolete version of 3D text.

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
