# Changelog

This page gives a plain-English overview of changes made on this fork. The
newest changes are always listed first. Times are recorded in UTC.

## September 3, 2026 — 13:12 UTC

### Clearer plugin documentation

- Replaced internal development-phase labels with user-facing descriptions.
- Clarified the difference between loose plugins, native plugins, and editor
  Rust scripts.
- Corrected outdated editor run-mode guidance in the contributing guide.

## September 3, 2026 — 12:34 UTC

### Faster, safer live Rust plugins

- Added support for loading and updating Rust plugins while the editor is
  running.
- Made plugin updates safer so a broken replacement does not remove the last
  working version.
- Preserved compatible plugin data when a plugin is reloaded.
- Added clearer plugin status, trust, enable, disable, and retry behavior.
- Improved plugin consistency across Windows, Linux, and macOS.
- Expanded automated coverage for compilation, loading, reloading, failures,
  exports, and saved projects.

## September 1, 2026 — 19:38 UTC

### Faster Rust plugin compilation

- Added a shared background compiler for Rust plugins and scripts.
- Reused unchanged build results instead of compiling the same source again.
- Prevented outdated builds from replacing newer edits.
- Added safer staging, recovery, cleanup, and storage limits for compiled
  results.
- Added documentation for how the build cache works and how it is maintained.

## August 31, 2026 — 19:45 UTC

### Reliable plugin and script identity

- Gave every Rust plugin and editor script a stable project-relative identity.
- Allowed files with the same name to coexist safely in different folders.
- Improved handling for renamed, moved, and deleted files.
- Kept editor builds and exported projects in agreement about which script is
  which.
- Improved protection against invalid paths and accidental identity conflicts.
