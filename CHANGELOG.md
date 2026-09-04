# Changelog

This page gives a plain-English overview of changes made on this fork. The newest changes are always listed first. Times are recorded in UTC.

## September 4, 2026 — 21:51 UTC

### Full engine plugin foundation

- Added an explicit declaration format for advanced plugins that need complete Bevy and Renzora access.
- Kept ordinary live Rust plugins distinct so Renzora never guesses which compilation model a file requires.
- Required separate editor and game/runtime halves to prevent editor-only code from entering exported games.
- Added early checks for invalid identities, duplicate plugins, unsafe paths, and unsupported declarations.

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
