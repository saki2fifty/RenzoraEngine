//! Lean static build backend — compiles the game's `renzora` binary from source
//! into a single stripped executable (static Bevy + static std, no sibling
//! dylibs), instead of copying the dynamically-linked dev runtime.
//!
//! A project is a separate asset folder, NOT a Rust workspace — so this compiles
//! the **engine source checkout** the editor was built from (located by walking
//! up from the editor's `dist/<platform>/` dir, see [`find_engine_source`]) and
//! the project's assets ride along in the rpak the caller appends. It builds via
//! `--no-default-features --features runtime` so the `dynamic_linking` feature is
//! dropped (see root `Cargo.toml`), under `[profile.dist-lean]`.
//!
//! The one subtlety is `prefer-dynamic`: `.cargo/config.toml` pins it per target
//! to make the *dev* build share one `bevy_dylib`. Cargo takes RUSTFLAGS from a
//! single highest-priority source with no merging, so setting
//! `CARGO_ENCODED_RUSTFLAGS` on the child process makes cargo **ignore** the
//! config rustflags for this one invocation — dropping `prefer-dynamic` without
//! editing any file. The separate `linker` config key is *not* rustflags and
//! survives (so Windows keeps `rust-lld`); on Linux we override it to the
//! near-universal `cc` because a freshly provisioned toolchain may lack the
//! repo's pinned `clang`/`mold`.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::templates::Platform;
use crate::toolchain::Toolchain;

/// Per-host RUSTFLAGS for the lean build, encoded for `CARGO_ENCODED_RUSTFLAGS`
/// (`\x1f`-separated args). An *empty* string still counts as "set", so cargo
/// drops the config's `prefer-dynamic` (and the Linux `mold` link-arg) — exactly
/// what we want for a static binary.
fn encoded_rustflags(platform: Platform) -> String {
    match platform {
        // Static-link the MSVC CRT too: with no dylib/TypeId boundary in a lean
        // binary, the reason it's disabled globally (crt-static perturbs crate
        // disambiguators across the dylib ABI) no longer applies — and it drops
        // the VCRUNTIME140.dll runtime dependency.
        Platform::WindowsX64 => ["-C", "target-feature=+crt-static"].join("\u{1f}"),
        // Drop prefer-dynamic (+ mold/rpath) by overriding with no flags.
        _ => String::new(),
    }
}

/// Drop `prefer-dynamic` from the export copy's cargo config, for a build that
/// runs in a container.
///
/// The host path does this with `CARGO_ENCODED_RUSTFLAGS`, which cargo treats as
/// a single highest-priority source and does not merge with any config file.
/// That is exactly what makes it work natively — and exactly why it cannot be
/// used in a container. The toolchain images supply the cross-linker's library
/// search paths *as rustflags*:
///
/// ```toml
/// [target.x86_64-pc-windows-msvc]
/// linker = "lld-link"
/// rustflags = ["-Lnative=/xwin/crt/lib/x86_64", …]
/// ```
///
/// Setting the env var would replace those, and the link would fail on missing
/// system libraries with nothing pointing at the cause. Editing the copy's own
/// config instead leaves cargo free to merge the two files' `rustflags` arrays
/// the way it normally does, so the image's paths survive.
///
/// Only the copy is touched — the dev tree is never edited (see
/// [`sync_export_workspace`]).
fn patch_cross_cargo_config(
    ws: &Path,
    platform: Platform,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    let path = ws.join(".cargo").join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        // No config in the copy is fine: the image's own config then applies
        // unopposed, which is already what we want.
        return Ok(());
    };

    // Line-wise rather than a TOML round-trip: `prefer-dynamic` appears inside
    // `rustflags` arrays whose other entries must survive verbatim, and dropping
    // one array element is the whole edit.
    let mut out = String::with_capacity(text.len());
    let mut removed = 0usize;
    for line in text.lines() {
        if line.contains("prefer-dynamic") {
            removed += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    // A lean Windows binary also static-links the MSVC CRT, which the host path
    // gets from `encoded_rustflags`. Appended as its own target section so it
    // merges with the image's rustflags rather than replacing them.
    if matches!(platform, Platform::WindowsX64) {
        out.push_str(
            "\n[target.x86_64-pc-windows-msvc]\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n",
        );
    }

    std::fs::write(&path, out).map_err(|e| {
        format!(
            "Could not patch {} for a container build: {e}",
            path.display()
        )
    })?;
    progress(format!(
        "Prepared cargo config for a container build ({removed} dynamic-link flags removed)"
    ));
    Ok(())
}

/// The lean binary's filename under `target/dist-lean/`.
fn bin_filename(platform: Platform) -> &'static str {
    match platform {
        // Both Windows arches now that a cross build can produce arm64 — the
        // extension follows the target, not the machine doing the compiling.
        Platform::WindowsX64 | Platform::WindowsArm64 => "renzora.exe",
        _ => "renzora",
    }
}

/// The lean cargo invocation: a builder of the cargo `Command` for the
/// `renzora` binary in the assembled workspace, with profile, features,
/// package, and target chosen exactly as production [`build_lean`] does.
///
/// `cmd` is the cargo command to amend (with `cargo` set, `current_dir`
/// pointing at `workspace`, and any env vars the caller wants set).
/// `subcommand` is `build` for production and `check` for the
/// integration test that exercises the assembled workspace without
/// linking the binary. `platform` and `cross` decide whether a
/// `--target` flag is appended and whether the Linux linker override
/// is applied. `profile` selects the `--profile` value. `features`
/// is the comma-separated feature string
/// (`runtime[,static_plugins][,static_scripts]`); empty is treated as
/// `runtime`.
///
/// The test that proves `renzora_static_scripts::scripts()` actually
/// reaches the lean binary's feature path uses this same builder with
/// `subcommand = "check"` and `features = "runtime,static_scripts"`
/// against the assembled workspace, so the assembly step is exercised
/// identically.
pub fn lean_cargo_args(
    cmd: &mut std::process::Command,
    workspace: &Path,
    platform: Platform,
    cross: bool,
    profile: LeanProfileArg,
    features: &str,
    subcommand: &str,
) {
    cmd.current_dir(workspace);
    cmd.args([
        subcommand,
        "--profile",
        profile.as_str(),
        "--bin",
        "renzora",
        "--no-default-features",
    ])
    .arg("--features")
    .arg(if features.is_empty() {
        "runtime"
    } else {
        features
    });
    if cross {
        let triple =
            crate::docker::rust_triple(platform).expect("No Rust target for lean build target");
        cmd.args(["--target", triple]);
    }
    if !cross && matches!(platform, Platform::LinuxX64) {
        cmd.args(["--config", "target.x86_64-unknown-linux-gnu.linker=\"cc\""]);
    }
}

/// Lean-build profile selector. Mirrors `[profile.dist-lean]` in the
/// root `Cargo.toml` (which is what production uses) and the standalone
/// `dist` profile used by the test-only check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeanProfileArg {
    /// `dist-lean` — what `build_lean` actually invokes.
    DistLean,
    /// `dist` — used by the test that proves the assembled workspace
    /// compiles under a fast profile; matches the existing
    /// `cargo check --profile dist` path.
    Dist,
}

impl LeanProfileArg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DistLean => "dist-lean",
            Self::Dist => "dist",
        }
    }
}

/// Locate the engine source checkout to compile, by walking up from `start`
/// (the editor's runtime dir, e.g. `<engine>/dist/windows-x64/`).
///
/// A lean build recompiles the engine itself — projects are separate asset
/// folders with no Rust source — so we need the workspace root. We identify it by
/// its signature: a `Cargo.toml` plus a `crates/` dir plus `src/main.rs` (so a
/// sub-crate's `Cargo.toml` can't be mistaken for the root). Returns `None` for a
/// canonical editor release with no source beside it — lean builds there will
/// need the engine source fetched first (future work).
/// The directory the running editor lives in — where a lean build looks for the
/// engine source.
///
/// Deliberately NOT the target platform's runtime directory. That is the
/// *template* dir, and for a cross-platform export it is the per-user download
/// store (`~/.renzora/templates/<version>/<platform>/`), which has no engine
/// source anywhere above it:
///
/// ```text
/// Could not find the engine source to compile
/// (searched up from ~/.renzora/templates/r1-alpha7/linux-x64)
/// ```
///
/// That path was harmless while lean builds were host-only, because the host's
/// template dir *is* `<engine>/dist/<platform>/`. Once a lean build could target
/// another platform it stopped being true — and a lean build never reads the
/// template anyway, since it recompiles the engine from source.
pub fn editor_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// The engine source a lean build compiles: the checkout the editor runs from
/// if there is one, otherwise a downloaded copy under `~/.renzora/src/<version>/`.
///
/// The checkout wins deliberately. A contributor's tree is the one they are
/// editing, and silently compiling a downloaded copy instead would produce a
/// binary that did not contain their changes — the kind of wrong that looks like
/// the build system lying.
///
/// The download exists so a canonical editor — binaries, no source — can still
/// do a lean export. It is fetched on demand by
/// [`crate::download::spawn_source_download`]; `None` here means neither is
/// present, which the UI turns into a Download offer rather than a dead end.
pub fn resolve_engine_source() -> Option<PathBuf> {
    if let Some(checkout) = editor_dir().and_then(|d| find_engine_source(&d)) {
        return Some(checkout);
    }
    let downloaded = crate::templates::user_source_dir()?;
    // Verified by the same signature rather than mere existence: a half-extracted
    // or emptied directory would otherwise be handed to cargo, which fails much
    // later and much less clearly.
    is_engine_source(&downloaded).then_some(downloaded)
}

/// Does this directory look like the engine workspace root?
///
/// `Cargo.toml` + `crates/` + `src/main.rs` together, so a sub-crate's manifest
/// cannot be mistaken for the root.
fn is_engine_source(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
        && dir.join("crates").is_dir()
        && dir.join("src").join("main.rs").is_file()
}

pub fn find_engine_source(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("Cargo.toml").is_file()
            && d.join("crates").is_dir()
            && d.join("src").join("main.rs").is_file()
        {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Compile a lean static `renzora` binary for `platform` from the engine source
/// at `workspace_dir` (the engine checkout, NOT the project). Returns the path to
/// the freshly built binary; the caller embeds the project's rpak into it.
///
/// `static_plugins` is the set the user chose to compile **into** the binary
/// rather than ship as loose libraries beside it (see [`stage_static_plugins`]).
/// Empty is the default and means the same as it always did: the caller copies
/// the selected plugins into `plugins/` and the host `dlopen`s them, which works
/// against a static binary because a C-ABI plugin links no Bevy and so has
/// nothing to share with the host.
///
/// Lean-export workspace assembly: synchronise the engine source into
/// an isolated copy, apply the cross/feature/profile patches, stage
/// the user's static plugins and the project's static scripts. The
/// caller receives the assembled workspace path and the
/// `has_scripts` flag it needs to enable the `static_scripts` cargo
/// feature when invoking the actual build.
///
/// This function is the production assembly boundary used by both
/// `build_lean` and the lean-export validation test. The function is
/// `pub(crate)` so `build_lean` and the crate's `#[cfg(test)]` module
/// can share it without exposing it as a public API.
pub(crate) fn assemble_lean_export_workspace(
    workspace_dir: &Path,
    project_dir: &Path,
    platform: Platform,
    disabled_bevy_features: &[String],
    disabled_runtime_features: &[String],
    profile: LeanProfile,
    static_plugins: &[StaticPluginSrc],
    progress: &mut dyn FnMut(String),
) -> Result<(PathBuf, bool), String> {
    let cross = Platform::current() != Some(platform);
    let ws = sync_export_workspace(workspace_dir, progress)?;
    if cross {
        patch_cross_cargo_config(&ws, platform, progress)?;
    }
    let has_scripts = configure_lean_export_workspace(
        workspace_dir,
        &ws,
        project_dir,
        disabled_bevy_features,
        disabled_runtime_features,
        profile,
        static_plugins,
        progress,
    )?;
    Ok((ws, has_scripts))
}

/// Apply lean settings to an already isolated snapshot, preserving native plugin wiring.
pub(crate) fn configure_lean_export_workspace(
    source: &Path,
    ws: &Path,
    project_dir: &Path,
    disabled_bevy_features: &[String],
    disabled_runtime_features: &[String],
    profile: LeanProfile,
    static_plugins: &[StaticPluginSrc],
    progress: &mut dyn FnMut(String),
) -> Result<bool, String> {
    strip_bevy_features(ws, disabled_bevy_features, progress)?;
    strip_runtime_features(ws, disabled_runtime_features, progress)?;
    patch_lean_profile(ws, profile, progress)?;
    stage_static_plugins(source, ws, static_plugins, progress)?;
    let has_scripts = stage_static_scripts(project_dir, ws, progress)?;
    Ok(has_scripts)
}

/// Native cargo can only target the **host** triple; cross-OS builds are a hard
/// Same-OS targets compile natively with `toolchain`; a different OS compiles in
/// that platform's toolchain container, because cargo can only target the host.
#[allow(clippy::too_many_arguments)]
pub fn build_lean(
    workspace_dir: &Path,
    // The project being exported — where its `scripts/*.rs` are read from. Not
    // the engine source, which `workspace_dir` points at.
    project_dir: &Path,
    platform: Platform,
    // `None` for a cross build, which compiles in a container and needs no
    // local Rust at all.
    toolchain: Option<&Toolchain>,
    progress: &mut dyn FnMut(String),
    disabled_bevy_features: &[String],
    disabled_runtime_features: &[String],
    profile: LeanProfile,
    static_plugins: &[StaticPluginSrc],
    cancel: &Arc<AtomicBool>,
) -> Result<PathBuf, String> {
    // ── Docker only for cross-OS; the host builds natively ───────────────────
    //
    // A container is a cross-compiler, and there is nothing to cross-compile for
    // the machine you are sitting at. Native is also faster (no image pull, no
    // bind mount) and needs no Docker install at all — so someone exporting for
    // their own platform never has to have it.
    //
    // Checked up front, before the workspace sync copies the engine source —
    // that takes long enough to look like the build had already started.
    let cross = Platform::current() != Some(platform);
    if cross {
        if !crate::docker::lean_supported(platform) {
            return Err(format!(
                "No lean build is available for {} — it has no toolchain image.",
                platform.display_name(),
            ));
        }
        match crate::docker::probe() {
            crate::docker::DockerStatus::Ready => {}
            crate::docker::DockerStatus::NotInstalled => {
                return Err(format!(
                    "A lean build for {} is compiled in a container, so it needs Docker. \
                     Install it from {} and try again.",
                    platform.display_name(),
                    crate::docker::INSTALL_URL,
                ));
            }
            crate::docker::DockerStatus::NotRunning(why) => {
                return Err(format!(
                    "Docker is installed but not responding, so the {} build cannot start. \
                     Start Docker Desktop and try again.\n{why}",
                    platform.display_name(),
                ));
            }
        }
    }

    if !workspace_dir.join("Cargo.toml").is_file() {
        return Err(format!(
            "No Cargo.toml at {} — a lean build recompiles the engine, so it needs \
             the engine source checkout.",
            workspace_dir.display()
        ));
    }

    // Build from an ISOLATED COPY of the engine source — never the dev tree, so
    // `cargo renzora` / `renzora run` are completely unaffected. The copy can be
    // patched freely with no restore (it's disposable). It has its own `target/`,
    // so the dev cache and locks are untouched and exports stay incremental
    // across runs.
    //
    // The full assembly (sync + patches + static plugins + static
    // scripts) lives in `assemble_lean_export_workspace` so the test
    // crate can exercise the same code path the production export
    // takes.
    let (ws, has_scripts) = assemble_lean_export_workspace(
        workspace_dir,
        project_dir,
        platform,
        disabled_bevy_features,
        disabled_runtime_features,
        profile,
        static_plugins,
        progress,
    )?;

    let mut features = String::from("runtime");
    if !static_plugins.is_empty() {
        features.push_str(",static_plugins");
    }
    // Only when there is something to link. Turning it on with an empty table
    // would compile the aggregator and the `renzora/static_scripts` variant of
    // `script!` for nothing.
    if has_scripts {
        features.push_str(",static_scripts");
    }

    // Native cargo for the host; the toolchain container for anything else. The
    // cargo arguments after this are identical either way — a container build is
    // the same build, run somewhere that has the cross-linker.
    let mut cmd = if cross {
        let image = crate::docker::image_for(platform)
            .ok_or_else(|| format!("No toolchain image for {}", platform.display_name()))?;
        let image_ref = crate::docker::image_ref(workspace_dir, image).ok_or_else(|| {
            format!(
                "Could not read docker/base/Dockerfile and docker/{image}/Dockerfile under {} \
                 to resolve the toolchain image tag.",
                workspace_dir.display()
            )
        })?;
        progress(format!("Building in {image_ref}"));
        let mut c = crate::docker::build_command(&image_ref, &ws);
        c.arg("cargo");
        c
    } else {
        let tc = toolchain
            .ok_or("Internal error: a same-OS lean build was started without a Rust toolchain.")?;
        let mut c = tc.cargo_command();
        c.current_dir(&ws);
        c
    };
    if !cross {
        cmd.env("CARGO_ENCODED_RUSTFLAGS", encoded_rustflags(platform));
    }
    lean_cargo_args(
        &mut cmd,
        &ws,
        platform,
        cross,
        LeanProfileArg::DistLean,
        &features,
        "build",
    );

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start cargo: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Share the child so a watcher thread can kill it on cancel. Only ever held
    // briefly (kill / wait), never across a blocking read, so cancel can't dead-
    // lock against the reaper.
    let child = Arc::new(Mutex::new(child));

    // Watcher: when Cancel is clicked, kill the build. Stops itself once the
    // build is done (the `done` flag the main thread sets after wait()).
    let done = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        let done = done.clone();
        let child = child.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    if let Ok(mut c) = child.lock() {
                        let _ = c.kill();
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        });
    }

    // Drain stdout on a side thread so a full pipe can't deadlock the build;
    // cargo's human progress goes to stderr, which we forward live and keep a
    // tail of for error reporting. On cancel the watcher kills the child, the
    // pipes hit EOF, and this read loop ends.
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = line;
            }
        });
    }

    let mut tail: Vec<String> = Vec::new();
    if let Some(err) = stderr {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            // Surface the most recent cargo line as progress (compiling crate N…).
            progress(line.clone());
            tail.push(line);
            if tail.len() > 60 {
                tail.remove(0);
            }
        }
    }

    let status = child
        .lock()
        .unwrap()
        .wait()
        .map_err(|e| format!("Failed waiting for cargo: {e}"))?;
    done.store(true, Ordering::Relaxed);

    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    if !status.success() {
        return Err(format!(
            "Lean build failed (cargo exited with {}):\n{}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            tail.join("\n")
        ));
    }

    // `--target` nests the output under the triple; a host build has no
    // `--target` and writes straight into `target/<profile>/`.
    let bin = if cross {
        let triple = crate::docker::rust_triple(platform)
            .ok_or_else(|| format!("No Rust target for {}", platform.display_name()))?;
        ws.join("target")
            .join(triple)
            .join("dist-lean")
            .join(bin_filename(platform))
    } else {
        ws.join("target")
            .join("dist-lean")
            .join(bin_filename(platform))
    };
    if !bin.is_file() {
        return Err(format!(
            "Lean build reported success but the binary is missing at {}",
            bin.display()
        ));
    }
    Ok(bin)
}

/// Sync the engine source into an isolated copy that the export build compiles,
/// so the dev tree is NEVER touched (`cargo renzora` / `renzora run` stay
/// pristine). The copy lives under the gitignored `target/` and has its own build
/// cache, so it's both isolated and incremental. Returns the copy's root.
///
/// This is a copy-if-newer mirror (by size + mtime), not a full re-copy: the
/// first export copies everything, later ones only touch changed files. It does
/// NOT delete files removed from the source — deleting a crate then re-exporting
/// without clearing `target/export-src` is the one case that needs a manual
/// clear (rare); everything else just works.
fn sync_export_workspace(
    engine_src: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<PathBuf, String> {
    let dest = engine_src.join("target").join("export-src");
    std::fs::create_dir_all(&dest).map_err(|e| format!("create export workspace: {e}"))?;
    progress("Syncing engine source into the isolated export workspace…".into());

    // Top-level dirs that are never part of a build. Matched ONLY at the root, so
    // they can't collide with same-named dirs nested inside `crates/` (e.g. a
    // crate's own `docs/`, or the critical `crates/renzora` vs the stray root
    // `renzora/`). `target` also covers `target/export-src` itself, so the sync
    // can't recurse into its own destination.
    //
    // `plugins` is here because those are separate cargo projects that the
    // engine build never reads — they are only ever needed by the linked-in
    // plugin path, which stages the handful it wants itself (and has to patch
    // their manifests, which a blanket copy-if-newer would keep undoing). See
    // `stage_static_plugins`.
    const TOP_SKIP: &[&str] = &[
        "target",
        ".git",
        ".github",
        ".vscode",
        ".idea",
        "dist",
        "docs",
        "node_modules",
        "templates",
        "disabled",
        "docker",
        ".claude",
        ".devcontainer",
        "plugins",
    ];
    // cdylib crates are never linked into the lean binary, so leave them out of
    // the copy entirely.
    let drop_plugins = cdylib_crates(engine_src);
    let mut copied = 0usize;
    for entry in
        std::fs::read_dir(engine_src).map_err(|e| format!("read {}: {e}", engine_src.display()))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let s = entry.path();
        let d = dest.join(&name);
        if ft.is_dir() {
            if TOP_SKIP.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            std::fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            if name.to_string_lossy() == "crates" {
                sync_crates(&s, &d, &drop_plugins, &mut copied)?;
            } else {
                sync_dir(&s, &d, &mut copied)?;
            }
        } else if ft.is_file() && should_copy(&s, &d) {
            std::fs::copy(&s, &d).map_err(|e| format!("copy {}: {e}", s.display()))?;
            copied += 1;
        }
    }
    if !drop_plugins.is_empty() {
        progress(format!(
            "Excluding {} unused distribution plugin(s) from the copy",
            drop_plugins.len()
        ));
    }
    progress(format!("Export workspace ready ({copied} file(s) updated)"));
    Ok(dest)
}

/// Sync `crates/`, but skip (and prune from the copy) the cdylib crates — they
/// are never linked into the lean binary, so copying and resolving them is pure
/// waste. Everything else (core rlib crates, vendored crates) syncs normally.
fn sync_crates(
    src: &Path,
    dest: &Path,
    drop_plugins: &HashSet<String>,
    copied: &mut usize,
) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let s = entry.path();
        let d = dest.join(&name);
        // Wholly written by `stage_static_plugins`. Copying the checked-in stub
        // over the generated version would undo it — and since the two differ in
        // size, the copy-if-newer test would fire on every export and rebuild the
        // aggregator (and relink the binary) whether or not anything changed.
        if name_str == "renzora_static_plugins" {
            continue;
        }
        if ft.is_dir() && drop_plugins.contains(name_str.as_ref()) {
            // Pruned: ensure it's absent (a prior export with a different plugin
            // selection may have copied it).
            if d.exists() {
                let _ = std::fs::remove_dir_all(&d);
            }
            continue;
        }
        if ft.is_dir() {
            std::fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            sync_dir(&s, &d, copied)?;
        } else if ft.is_file() && should_copy(&s, &d) {
            std::fs::copy(&s, &d).map_err(|e| format!("copy {}: {e}", s.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

/// Names of `crates/` entries that build a cdylib — never in the lean binary's
/// link closure, so the copy skips them. Core runtime subsystems are rlib
/// libraries (no `cdylib` crate-type) and never match. The game's own C-ABI
/// plugins live in `plugins/`, not here — they either ship as files beside the
/// binary or are linked in via `stage_static_plugins`, which stages them itself.
fn cdylib_crates(engine_src: &Path) -> HashSet<String> {
    let mut drop = HashSet::new();
    let Ok(rd) = std::fs::read_dir(engine_src.join("crates")) else {
        return drop;
    };
    for entry in rd.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(text) = std::fs::read_to_string(entry.path().join("Cargo.toml")) {
            if is_cdylib_crate(&text) {
                drop.insert(name);
            }
        }
    }
    drop
}

/// Whether a manifest declares a `cdylib` crate-type (a distribution plugin /
/// bundle), as opposed to an rlib core library or the `dylib`+rlib `renzora`
/// contract. Ignores commented-out lines.
fn is_cdylib_crate(manifest: &str) -> bool {
    manifest.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("crate-type") && l.contains("cdylib")
    })
}

/// Recursive copy-if-newer of `src` → `dest`. Inside the tree only build/vcs
/// noise (never source) is skipped, so no needed crate file is missed.
fn sync_dir(src: &Path, dest: &Path, copied: &mut usize) -> Result<(), String> {
    const DEEP_SKIP: &[&str] = &["target", ".git"];
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let s = entry.path();
        let d = dest.join(&name);
        if ft.is_dir() {
            if DEEP_SKIP.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            std::fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            sync_dir(&s, &d, copied)?;
        } else if ft.is_file() && should_copy(&s, &d) {
            std::fs::copy(&s, &d).map_err(|e| format!("copy {}: {e}", s.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

/// Whether `src` should be copied over `dest`: missing, different size, or newer.
fn should_copy(src: &Path, dest: &Path) -> bool {
    let (Ok(sm), Ok(dm)) = (std::fs::metadata(src), std::fs::metadata(dest)) else {
        return true;
    };
    if sm.len() != dm.len() {
        return true;
    }
    match (sm.modified(), dm.modified()) {
        (Ok(st), Ok(dt)) => st > dt,
        _ => true,
    }
}

/// Strip `disabled` Bevy features from the export copy's root `Cargo.toml`
/// (the `[workspace.dependencies] bevy` feature list), so the lean binary doesn't
/// compile capabilities the game doesn't use. Safe because it edits the copy, not
/// the dev source. Format-preserving via `toml_edit`. No-op if `disabled` empty.
fn strip_bevy_features(
    copy_root: &Path,
    disabled: &[String],
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    if disabled.is_empty() {
        return Ok(());
    }
    let manifest = copy_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let arr = doc
        .get_mut("workspace")
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(|d| d.get_mut("bevy"))
        .and_then(|b| b.get_mut("features"))
        .and_then(|f| f.as_array_mut());
    let Some(arr) = arr else {
        // No workspace bevy feature list to trim — nothing to do.
        return Ok(());
    };
    arr.retain(|v| {
        v.as_str()
            .map(|s| !disabled.iter().any(|d| d == s))
            .unwrap_or(true)
    });
    std::fs::write(&manifest, doc.to_string())
        .map_err(|e| format!("write {}: {e}", manifest.display()))?;
    progress(format!(
        "Stripping {} unused Bevy feature(s)",
        disabled.len()
    ));
    Ok(())
}

/// The `[profile.dist-lean]` knobs the export UI can move.
///
/// All three are size-for-something trades that the dev tree deliberately does
/// NOT take (see the profile's notes in the root `Cargo.toml`), so they live
/// here as per-export choices rather than as edits to the checked-in profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeanProfile {
    /// `panic = "abort"` — trades fault isolation for ~24% off the binary.
    pub panic_abort: bool,
    /// `opt-level = "z"` instead of the profile's `"s"`. Same as `s` but with
    /// loop vectorization off too.
    pub opt_level_z: bool,
    /// `codegen-units = 1` instead of the release default of 16 — one LLVM
    /// module per crate, so thin LTO has fewer duplicated inline copies to
    /// merge, at a large cost in build time.
    pub codegen_units_one: bool,
}

/// Patch the export copy's `[profile.dist-lean]` to match `opts`.
///
/// `panic = "abort"` is the largest single lever there is — measured 60.9 MB →
/// 46.7 MB on a cube-and-light project, because dropping unwinding removes the
/// landing pads and cleanup glue from `.text` and the panic message/location
/// strings from `.rdata`, not merely the `.pdata` unwind tables. It is only legal
/// because the copy builds `renzora` as `rlib` only (see
/// `sync_export_workspace`); the dev tree's `dylib` would link the precompiled
/// std's `panic_unwind` and refuse to mix strategies.
///
/// Every key is written on every export, never left to whatever the last one
/// set: the copy persists between exports, so a stale `panic = "abort"` or
/// `codegen-units = 1` would silently apply to a later export that had switched
/// the toggle back off. `opt-level` is *set* to `"s"` rather than removed for
/// the same reason in reverse — removing it would inherit `dist`'s `opt-level =
/// 2`, which is a speed profile, whereas absent `codegen-units` correctly means
/// the release default of 16.
fn patch_lean_profile(
    copy_root: &Path,
    opts: LeanProfile,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    let manifest = copy_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let Some(profile) = doc
        .get_mut("profile")
        .and_then(|p| p.get_mut("dist-lean"))
        .and_then(|p| p.as_table_like_mut())
    else {
        return Ok(());
    };

    if opts.panic_abort {
        profile.insert("panic", toml_edit::value("abort"));
    } else {
        profile.remove("panic");
    }
    profile.insert(
        "opt-level",
        toml_edit::value(if opts.opt_level_z { "z" } else { "s" }),
    );
    if opts.codegen_units_one {
        profile.insert("codegen-units", toml_edit::value(1i64));
    } else {
        profile.remove("codegen-units");
    }

    // Only rewrite when something actually changed: an untouched manifest keeps
    // its mtime, and cargo fingerprints the whole workspace manifest set — a
    // gratuitous write would rebuild every crate on an otherwise no-op export.
    write_if_changed(&manifest, &doc.to_string())?;

    if opts.panic_abort {
        progress("Building with panic = abort (no unwinding)".to_string());
    }
    if opts.opt_level_z {
        progress("Building with opt-level = z (no loop vectorization)".to_string());
    }
    if opts.codegen_units_one {
        progress("Building with codegen-units = 1 (slower, smaller)".to_string());
    }
    Ok(())
}

/// Strip `disabled` subsystem features from the export copy's
/// `renzora_runtime/Cargo.toml` `[features] default`, so a game that doesn't use
/// (e.g.) the sky or post-FX subsystems doesn't compile/register them. Safe: it
/// edits the copy, not the dev source. No-op if `disabled` empty.
fn strip_runtime_features(
    copy_root: &Path,
    disabled: &[String],
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    if disabled.is_empty() {
        return Ok(());
    }
    let manifest = copy_root
        .join("crates")
        .join("renzora_runtime")
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("parse {}: {e}", manifest.display()))?;
    let arr = doc
        .get_mut("features")
        .and_then(|f| f.get_mut("default"))
        .and_then(|d| d.as_array_mut());
    let Some(arr) = arr else {
        return Ok(());
    };
    arr.retain(|v| {
        v.as_str()
            .map(|s| !disabled.iter().any(|d| d == s))
            .unwrap_or(true)
    });
    std::fs::write(&manifest, doc.to_string())
        .map_err(|e| format!("write {}: {e}", manifest.display()))?;
    progress(format!("Stripping {} unused subsystem(s)", disabled.len()));
    Ok(())
}

// ── Statically-linked plugins ────────────────────────────────────────────────

/// One plugin to compile into the binary instead of shipping beside it.
///
/// [`resolve_static_plugins`] builds these by pairing what the export UI listed
/// — which comes from scanning built libraries in `dist/<platform>/plugins/` —
/// with the source directory that produced each one, because linking a plugin in
/// means compiling it, not copying it.
#[derive(Debug, Clone)]
pub struct StaticPluginSrc {
    /// The plugin's crate name — the Rust identifier the generated list names,
    /// and what the host logs the plugin as. Underscored, because that is what
    /// rustc sees; see `package` for what cargo is told.
    pub id: String,
    /// The `[package] name` as written, which may contain dashes. Cargo resolves
    /// a dependency by this and rustc then substitutes underscores, so the
    /// generated manifest must use it verbatim and the generated code must not.
    pub package: String,
    /// The library stem the export UI keys its selection on. Differs from `id`
    /// on Unix, where a cdylib is `lib<crate>.so`, so the two are kept apart:
    /// the caller matches its selection on THIS, and a filter written against
    /// `id` would let every linked plugin be copied beside the binary as well.
    pub library_stem: String,
    /// Its directory under `plugins/`. Usually the same as `id`, but the two are
    /// resolved separately because a package name need not match its folder.
    pub dir: String,
    /// `true` for an Editor-scope plugin. Recorded rather than filtered so the
    /// host applies the same scope rule to a linked plugin as to a loaded one.
    pub editor_scope: bool,
}

/// Pair each wanted plugin id with the source directory that builds it.
///
/// Returns `(resolved, unresolved)`. An id with no matching source is NOT an
/// error: a plugin can perfectly well live in `dist/<platform>/plugins/` without
/// its source being in this checkout — a marketplace download is exactly that —
/// and the right answer there is to ship it as a file, not to fail the export.
/// The caller copies the unresolved ones beside the binary as usual.
pub fn resolve_static_plugins(
    engine_src: &Path,
    wanted: &[(String, bool)],
) -> (Vec<StaticPluginSrc>, Vec<String>) {
    // Package name → directory, read from the manifests rather than assumed from
    // the folder names: the library a plugin produces is named after its
    // `[package] name` (with dashes underscored), and that is what the scan sees.
    // Underscored package name → (package name as written, directory).
    let mut by_package: std::collections::HashMap<String, (String, String)> = Default::default();
    if let Ok(entries) = std::fs::read_dir(engine_src.join("plugins")) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = entry.file_name().to_string_lossy().into_owned();
            let Ok(text) = std::fs::read_to_string(entry.path().join("Cargo.toml")) else {
                continue;
            };
            if let Some(name) = package_name(&text) {
                by_package.insert(name.replace('-', "_"), (name, dir));
            }
        }
    }

    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for (id, editor_scope) in wanted {
        // Unix libraries are `lib<crate>.so`; the scan keeps the stem verbatim,
        // so strip the prefix before matching a crate name against it.
        let crate_name = id.strip_prefix("lib").unwrap_or(id.as_str());
        match by_package
            .get(id.as_str())
            .or_else(|| by_package.get(crate_name))
        {
            Some((package, dir)) => resolved.push(StaticPluginSrc {
                id: crate_name.to_string(),
                package: package.clone(),
                library_stem: id.clone(),
                dir: dir.clone(),
                editor_scope: *editor_scope,
            }),
            None => unresolved.push(id.clone()),
        }
    }
    resolved.sort_by(|a, b| a.id.cmp(&b.id));
    (resolved, unresolved)
}

/// The `[package] name` of a manifest, without pulling in a full parse.
fn package_name(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .find(|l| l.trim_start().starts_with("name = "))
        .and_then(|l| l.split('"').nth(1))
        .map(str::to_string)
}

/// Write the generated `renzora_static_plugins` crate and make each linked
/// plugin buildable as a dependency of it.
///
/// Called on EVERY lean build, including ones linking nothing — the export
/// workspace persists between exports, so a list left over from a previous run
/// would otherwise link plugins the user had since unticked, and the binary
/// would contain code no setting in front of them explained.
///
/// Three edits per plugin, all to the disposable copy:
///
/// 1. **`crate-type` → `rlib`.** A `cdylib` cannot be a Rust dependency; cargo
///    refuses with "found staticlib/cdylib, expected rlib". The cdylib artifact
///    is also pure waste here, so it is replaced rather than appended to.
/// 2. **Drop `[workspace]`.** Each plugin declares itself a workspace root so a
///    standalone `cargo build` in its folder does not inherit the engine's
///    feature unification. As a path dependency that is fatal — cargo reports
///    "multiple workspace roots found in the same workspace" and stops. The root
///    manifest's `exclude = ["plugins"]` keeps them out of the member set anyway,
///    so removing the marker changes nothing except that this now resolves.
/// 3. **Drop `[profile.*]`.** Profiles outside a workspace root are ignored with
///    a warning, and sixty of those warnings buries the build log.
fn stage_static_plugins(
    engine_src: &Path,
    copy_root: &Path,
    plugins: &[StaticPluginSrc],
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    let crate_dir = copy_root.join("crates").join("renzora_static_plugins");
    std::fs::create_dir_all(crate_dir.join("src"))
        .map_err(|e| format!("create {}: {e}", crate_dir.display()))?;

    let mut copied = 0usize;
    for p in plugins {
        let src = engine_src.join("plugins").join(&p.dir);
        let dest = copy_root.join("plugins").join(&p.dir);
        std::fs::create_dir_all(&dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
        // Everything but the manifest, which is written patched below. A plugin
        // that stops being linked leaves its copy behind; nothing references it,
        // and deleting directories a later export may want back is a worse
        // trade than a few hundred KB in a disposable tree.
        sync_dir_except(&src, &dest, "Cargo.toml", &mut copied)?;
        patch_plugin_manifest(&src, &dest.join("Cargo.toml"))?;
    }

    let mut deps = String::new();
    let mut entries = String::new();
    for p in plugins {
        deps.push_str(&format!(
            "{name} = {{ path = \"../../plugins/{dir}\" }}\n",
            name = p.package,
            dir = p.dir
        ));
        entries.push_str(&format!(
            "        StaticPlugin {{\n\
             \x20           id: \"{id}\",\n\
             \x20           scope: PluginScope::{scope},\n\
             \x20           init: {id}::renzora_plugin_init,\n\
             \x20       }},\n",
            id = p.id,
            scope = if p.editor_scope { "Editor" } else { "Runtime" },
        ));
    }

    let manifest = format!(
        "# GENERATED by the lean exporter — see `renzora_export::build`.\n\
         # Lists the C-ABI plugins compiled into this build's binary.\n\
         [package]\n\
         name = \"renzora_static_plugins\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [dependencies]\n\
         # `static_link` strips `#[no_mangle]` from what `renzora_plugin::add!`\n\
         # emits. Without it every plugin below defines `renzora_plugin_init` and\n\
         # the binary fails to link. Cargo unifies features per package, so\n\
         # naming it once here applies it to all of them.\n\
         renzora_plugin = {{ path = \"../renzora_plugin\", features = [\"static_link\"] }}\n\
         {deps}"
    );
    write_if_changed(&crate_dir.join("Cargo.toml"), &manifest)?;

    let body = if plugins.is_empty() {
        "    Vec::new()\n".to_string()
    } else {
        format!("    vec![\n{entries}    ]\n")
    };
    let lib = format!(
        "//! GENERATED by the lean exporter — see `renzora_export::build`.\n\
         //!\n\
         //! The plugins this build compiled in rather than shipping as files.\n\
         //! Overwritten on every lean export; edits here do not survive one.\n\
         \n\
         use renzora_plugin::static_link::StaticPlugin;\n\
         use renzora_plugin::sys::PluginScope;\n\
         \n\
         pub fn plugins() -> Vec<StaticPlugin> {{\n{body}}}\n"
    );
    write_if_changed(&crate_dir.join("src").join("lib.rs"), &lib)?;

    if !plugins.is_empty() {
        progress(format!(
            "Linking {} plugin(s) into the binary: {}",
            plugins.len(),
            plugins
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// Write only when the content differs, so an unchanged plugin selection does
/// not touch the mtime and force cargo to rebuild the aggregator (and relink the
/// whole binary) on every export.
/// Generate the crate that compiles the project's `.rs` scripts into the binary.
///
/// A lean binary links Bevy statically, so there is no shared image for a script
/// dylib to bind to — `RustScriptPlugin` refuses to register a backend and the
/// exported game reports `No backend for Some("rs")`. Compiling the sources in
/// removes the boundary rather than trying to make it safe: no library, no symbol
/// lookup, no second `World` type, because the script is part of the same
/// compilation as everything it touches.
///
/// Each script becomes a `#[path]` module of `renzora_static_scripts`, which is
/// what keeps fifty of them from colliding. Two things would otherwise collide:
///
/// - `renzora::script!` emits `#[unsafe(no_mangle)] fn renzora_script_update`,
///   and fifty of those do not link. The `renzora/static_scripts` feature that
///   the generated manifest turns on drops the attribute; the aggregator then
///   names each entry point by path instead of by symbol.
/// - a script's own items (`#[derive(Component)] struct Spin`) would clash with
///   another script's. Modules namespace them, so two scripts may both define a
///   `Spin` and neither knows.
///
/// Returns whether anything was generated, so the caller only adds the feature
/// when there is something to link.
/// Ship the plugin SDK so the exported game can compile plugins of its own.
///
/// This is what "enable modding" buys. Without it a game loads only the
/// prebuilt libraries the export staged; with it, a player can drop a native
/// plugin's SOURCE into `plugins/` and the game builds it on next launch,
/// exactly as the editor does — same compiler driver, same SDK, same loading.
///
/// Copied rather than repacked. A release ships `sdk.tar.zst` and unpacks it on
/// first run, deleting the archive, so an editor that has been started once has
/// only the extracted tree — and repacking 1.5 GB at `zstd -19` would add
/// minutes to every export to save space in a directory the player never
/// downloads over a network. Whichever form is present is what ships: the
/// archive if the editor has not unpacked it yet, the tree otherwise, and the
/// game's own first-run step handles the archive case.
///
/// Host platform only. An SDK is only correct on the platform it was built for —
/// its proc-macro dylibs belong to whatever ran the compiler — so shipping this
/// editor's SDK inside a game for another OS would hand a player a compiler that
/// cannot run. That is the same rule that makes a cross-built editor unable to
/// compile scripts.
pub fn stage_modding_sdk(
    editor_dir: &Path,
    output_dir: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<bool, String> {
    // Somewhere to put a mod, even when the game shipped no plugins of its own.
    // An empty directory is the instruction: a player who opens the folder can
    // see where a plugin goes, where otherwise they would have to know to create
    // it — and a game with modding enabled and no `plugins/` anywhere looks like
    // modding was not enabled at all.
    let plugins = output_dir.join("plugins");
    std::fs::create_dir_all(&plugins).map_err(|e| format!("create {}: {e}", plugins.display()))?;

    let source_sdk = editor_dir.join("rust-sdk");
    if source_sdk.exists() {
        let entry = source_sdk.join("crates/renzora_plugin");
        if renzora_compiler_cache::sdk::content_root(&entry) != source_sdk {
            return Err("Installed Rust source SDK is incomplete; repair the installation".into());
        }
        renzora_rust_sdk::stage(&source_sdk, output_dir)
            .map_err(|error| format!("stage Rust source SDK: {error}"))?;
        progress("Shipped the small Rust source SDK for modding".into());
        return Ok(true);
    }

    // The archive first: smaller, and the game unpacks it on first launch behind
    // the same progress window the editor uses.
    let archive = editor_dir.join("sdk.tar.zst");
    if archive.is_file() {
        std::fs::copy(&archive, output_dir.join("sdk.tar.zst"))
            .map_err(|e| format!("copy sdk.tar.zst: {e}"))?;
        progress("Shipped the plugin SDK (compressed) for modding".to_string());
        return Ok(true);
    }

    let sdk = editor_dir.join("sdk");
    if !sdk.join("manifest.json").is_file() {
        progress(
            "WARN: modding is on but this editor has no plugin SDK — the game will ship \
             without one and can load only prebuilt plugins."
                .to_string(),
        );
        return Ok(false);
    }

    progress("Copying the plugin SDK for modding (this is ~1.5 GB)…".to_string());
    let copied = copy_dir(&sdk, &output_dir.join("sdk"))?;
    progress(format!(
        "Shipped the plugin SDK for modding ({copied} files)"
    ));
    Ok(true)
}

/// Recursive copy, returning how many files landed.
fn copy_dir(from: &Path, to: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    let mut count = 0;
    let entries = std::fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))?;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = to.join(name);
        if src.is_dir() {
            count += copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .map_err(|e| format!("copy {} → {}: {e}", src.display(), dst.display()))?;
            count += 1;
        }
    }
    Ok(count)
}

/// Ship the `Runtime`-scope native plugins the editor already built, beside a
/// copy-based export.
///
/// A native plugin links the real Bevy, so it can only load into a host that
/// shares the same image — which a copy-based export does, since it carries the
/// very `bevy_dylib` and `renzora_dylib` the plugin was compiled against. (A
/// lean export links Bevy statically and shares nothing, so it takes no native
/// plugins at all.)
///
/// **Scope is read from the built library, not the source.** A
/// `plugin!(.., Runtime)` in `src/lib.rs` describes what the source would build
/// to; what ships is the library, and the two disagree whenever one was edited
/// without rebuilding. Asking the artefact removes the discrepancy.
///
/// Only the library is staged — no `src/`, no stamp. The loader treats a
/// directory holding a built library and nothing else as a plugin it can load
/// but not rebuild, which is exactly a shipped game's situation. Shipping the
/// source would put a plugin author's code inside every game that uses it to
/// satisfy a marker nothing reads.
///
/// Host platform only, for the same reason as the scripts: these libraries are
/// host-shaped.
pub fn stage_runtime_native_plugins(
    editor_dir: &Path,
    output_dir: &Path,
    lib_ext: &str,
    // What the exporter's plugin picker has ticked. `None` means the picker
    // never ran, in which case every eligible plugin ships — the behaviour
    // before it listed native plugins at all.
    selected: Option<&std::collections::HashSet<String>>,
    progress: &mut dyn FnMut(String),
) -> Result<usize, String> {
    let src_root = editor_dir.join("plugins");

    // A plugin switched off in Settings → Editor → Plugins must not ship. It is
    // off because the user turned it off, and an export is the last moment that
    // choice can still be honoured — after this it is in a player's hands with
    // no switch at all.
    let disabled = renzora::load_disabled_plugins();

    let mut shipped: Vec<String> = Vec::new();
    let mut skipped_editor: Vec<String> = Vec::new();
    for plugin in renzora_native_plugin::installed(&src_root, lib_ext) {
        let name = plugin.id;
        if disabled.iter().any(|d| d == &name) {
            continue;
        }
        // Unticked in the picker. Distinct from `disabled` above: that is "not
        // in my editor", this is "not in this build".
        if selected.is_some_and(|s| !s.contains(&name)) {
            continue;
        }
        if plugin.scope != renzora::NativePluginScope::Runtime {
            // Editor-only: belongs in no game, and worth saying so.
            skipped_editor.push(name);
            continue;
        }

        let dest = output_dir.join("plugins").join(&name).join("build");
        std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
        let dest_lib = dest.join(format!("{}.{lib_ext}", name.replace('-', "_")));
        std::fs::copy(&plugin.lib, &dest_lib).map_err(|e| {
            format!(
                "copy {} → {}: {e}",
                plugin.lib.display(),
                dest_lib.display()
            )
        })?;
        shipped.push(name);
    }

    if !shipped.is_empty() {
        progress(format!(
            "Shipped {} runtime native plugin(s): {}",
            shipped.len(),
            shipped.join(", ")
        ));
    }
    // Said out loud, because "my plugin is missing from the build" is otherwise
    // indistinguishable from a bug — and the fix is one word in the source.
    if !skipped_editor.is_empty() {
        progress(format!(
            "{} native plugin(s) are editor-only and were not shipped ({}). Declare \
             `renzora::plugin!(.., Runtime)` to include one in a game.",
            skipped_editor.len(),
            skipped_editor.join(", ")
        ));
    }
    Ok(shipped.len())
}

/// Ship the loose Tier-1 plugin cdylibs the editor already built, beside
/// a copy-based export.
///
/// Mirrors [`stage_runtime_native_plugins`]: each selected Runtime loose
/// plugin's staged cdylib is copied to `<output>/plugins/<safe-name>/build/`.
/// Editor-scoped loose plugins are excluded (the runtime has no editor
/// surface); disabled loose plugins are excluded (the user disabled them).
///
/// `candidates` is the snapshot `run_export` takes of
/// `LoosePluginInventory::export_candidates()` while it still holds
/// `&mut World`; passing it by value lets the background export worker
/// stage without touching a Bevy world.
///
/// C3-6: the filesystem path uses the safe-name encoder (`:` and `/`
/// replaced with `_`) because the canonical id (e.g. `engine://spin.rs`)
/// contains path separators that would create unintended subdirectories
/// and an invalid Windows directory name. The full canonical id is the
/// logical key and selection identity, but the filesystem placement
/// uses the safe name.
///
/// Returns how many were staged.
pub fn stage_loose_plugins_from(
    candidates: &[(String, std::path::PathBuf)],
    output_dir: &std::path::Path,
    selected: Option<&std::collections::HashSet<String>>,
    progress: &mut dyn FnMut(String),
) -> Result<usize, String> {
    let disabled = renzora::load_disabled_plugins();

    let mut shipped: Vec<String> = Vec::new();
    for (name, staged_path) in candidates {
        if disabled.iter().any(|d| d == name) {
            continue;
        }
        // Unticked in the picker. Distinct from `disabled` above: that is
        // "not in my editor", this is "not in this build".
        if selected.is_some_and(|s| !s.contains(name)) {
            continue;
        }
        // Logical key: full canonical id (kept as `name` for the
        // progress / persistence layer). Physical placement: the
        // safe-name encoder's output, which is a valid directory name
        // on every OS. Two loose plugins whose canonical ids share a
        // leaf (e.g. `engine://spin.rs` and `market://spin.rs`) export
        // to two distinct directories under different safe names.
        let safe_name = canonical_to_safe_dir_name(name);
        let dest = output_dir.join("plugins").join(&safe_name).join("build");
        std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
        // The staged file's filename already encodes the canonical id
        // through the same safe-name encoder (see
        // `StableStaging::stable_name_for`); we copy it under that
        // name so a runtime that scans the export tree can resolve the
        // canonical id back to its stable file.
        let staged_name = staged_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        let dest_lib = dest.join(&staged_name);
        std::fs::copy(staged_path, &dest_lib).map_err(|e| {
            format!(
                "copy {} → {}: {e}",
                staged_path.display(),
                dest_lib.display()
            )
        })?;
        shipped.push(name.clone());
    }
    if !shipped.is_empty() {
        progress(format!(
            "Shipped {} loose runtime plugin(s): {}",
            shipped.len(),
            shipped.join(", ")
        ));
    }
    Ok(shipped.len())
}

/// Encode a canonical id's `Display` form (`engine://spin.rs`) as a
/// directory-safe name. Replaces `:` and `/` with `_`, producing a
/// valid filename on every OS. Same encoding as
/// `StableStaging::safe_dir_name_for`, duplicated here to avoid
/// adding `renzora_loose_plugins` as a build-script / public-export
/// dependency on this crate's runtime path.
pub fn canonical_to_safe_dir_name(canonical: &str) -> String {
    canonical.replace([':', '/'], "_")
}

/// Ship the script libraries the editor already built, beside a copy-based
/// export.
///
/// A copy-based export carries the same `bevy_dylib` and `renzora_dylib` the
/// editor compiled these against, so the `World` on both sides of the `dlopen`
/// boundary is one type and they load exactly as they do in the editor. The
/// player needs no SDK and no Rust toolchain — the compiling already happened.
///
/// Copies rather than recompiles. Phase 4 reads the compiled
/// libraries from the shared `BuildService`'s cache (where every
/// successful editor build lands), not from
/// `<project>/.renzora/scripts/<dir>/`. The exporter takes a
/// `BuildService` and walks its cache. A script that never compiled
/// has nothing there and is reported rather than silently omitted.
///
/// **Host platform only.** These are host-shaped libraries, so they belong with
/// an export for the machine that built them. Staging them into an export for
/// another OS would ship a `.dll` to a Linux player — silently useless. A
/// cross-platform copy export therefore has no Rust scripts, which is a real
/// gap and the reason the lean path compiles them in instead.
///
/// The manifest (`scripts.index`) uses the same canonical-id derivation
/// the editor uses for its build directories. Each row's key is a
/// `project://<rel>` canonical id (which the runtime's
/// `LoadedScripts::load_prebuilt_scripts` parses). A row keyed by the
/// bare leaf name is added only when that leaf is unique across the
/// project — duplicate leaves stay reachable through their full
/// canonical paths and dispatch reports the bare alias as Ambiguous.
///
/// Returns how many were staged.
pub fn stage_prebuilt_scripts(
    project_dir: &Path,
    output_dir: &Path,
    lib_ext: &str,
    build_service: &std::sync::Arc<renzora_compiler_cache::BuildService>,
    progress: &mut dyn FnMut(String),
) -> Result<usize, String> {
    // The exporter and the editor MUST use the same canonical-id
    // discovery. `renzora_rust_script::discovery::collect_canonical_scripts`
    // is the single source of truth: one walker, one SKIP list, one
    // declaration recognition, one symlink policy.
    let canonical_ids: Vec<renzora_identity::CanonicalId> =
        renzora_rust_script::discovery::collect_canonical_scripts(project_dir);
    if canonical_ids.is_empty() {
        return Ok(0);
    }

    let dest = output_dir.join("scripts");
    std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;

    let mut index = String::new();
    let mut staged = 0usize;
    let mut missing: Vec<String> = Vec::new();
    let cache = build_service.cache();
    for (i, id) in canonical_ids.iter().enumerate() {
        // The Phase 4 production path: the BuildService owns the
        // cache layout. The exporter reads the latest published
        // artifact for this identity directly from the cache.
        let artifact = read_active_artifact(cache, id, lib_ext);
        let Some(lib) = artifact else {
            missing.push(id.path().to_string());
            continue;
        };
        if !lib.is_file() {
            missing.push(id.path().to_string());
            continue;
        }

        let file = format!("script_{i}.{lib_ext}");
        std::fs::copy(&lib, dest.join(&file))
            .map_err(|e| format!("copy {} → {}: {e}", lib.display(), file))?;

        let scheme_path = id.to_scheme_path();
        index.push_str(&format!("{scheme_path}\t{file}\n"));

        staged += 1;
    }

    if !missing.is_empty() {
        progress(format!(
            "WARN: {} script(s) have no published artifact and were not shipped ({}). \
             Open the project in the editor so they build, then export again.",
            missing.len(),
            missing.join(", ")
        ));
    }
    if staged == 0 {
        let _ = std::fs::remove_dir_all(&dest);
        return Ok(0);
    }
    std::fs::write(dest.join(renzora_rust_script::PREBUILT_MANIFEST), index)
        .map_err(|e| format!("write script index: {e}"))?;
    progress(format!("Shipped {staged} compiled Rust script(s)"));
    Ok(staged)
}

/// Read the active published artifact path for `id` from the
/// `BuildService`'s `ArtifactCache`. Returns `None` when no
/// generation has been published for `id`.
fn read_active_artifact(
    cache: &std::sync::Arc<renzora_compiler_cache::ArtifactCache>,
    id: &renzora_identity::CanonicalId,
    lib_ext: &str,
) -> Option<std::path::PathBuf> {
    let active = cache.read_active(id)?;
    let path = cache.artifact_path(id, active.generation, lib_ext);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// The project's scripts, as the EDITOR defines them.
///
/// Delegated rather than reimplemented. The two had their own scans for a while
/// and disagreed — the editor read `<project>/scripts/` flat, this walked the
/// whole tree — so a script in a subfolder was compiled into a lean export
/// having never run in the editor once. Sharing the definition means an export
/// can only ever ship what the editor would have built.
fn collect_scripts(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    out.extend(renzora_rust_script::discovery::collect_rust_scripts(dir));
    Ok(())
}

/// Lean-export assembly: write the project's Rust scripts into the
/// generated `crates/renzora_static_scripts/` crate at `copy_root`.
/// Returns `Ok(true)` when at least one script was assembled.
///
/// This is the production entry point for lean-script generation.
/// `build_lean` calls it; the lean-compilation test calls it.
fn stage_static_scripts(
    project_dir: &Path,
    copy_root: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<bool, String> {
    let crate_dir = copy_root.join("crates").join("renzora_static_scripts");

    // The WHOLE project, recursively — not just `scripts/`. A script is attached
    // by path, and nothing requires that path to live in one directory: a project
    // may keep them beside the scenes that use them, or grouped under
    // `scripts/enemies/`. Scanning one folder would omit the rest and the export
    // would build cleanly, ship, and report `No backend` for exactly the scripts
    // that were somewhere else.
    let mut sources: Vec<PathBuf> = Vec::new();
    collect_scripts(project_dir, &mut sources)?;
    // Sorted so a rebuild with no source change produces byte-identical output
    // and `write_if_changed` keeps cargo from recompiling the crate.
    sources.sort();

    if sources.is_empty() {
        return Ok(false);
    }

    std::fs::create_dir_all(crate_dir.join("src"))
        .map_err(|e| format!("create {}: {e}", crate_dir.display()))?;

    // Copied beside the generated lib rather than referenced where they live:
    // a `#[path]` pointing outside the workspace works, but then the export's
    // build depends on the project directory staying put mid-compile, and the
    // throwaway copy stops being self-contained.
    // Leaf names are no longer unique now the scan is recursive — a project may
    // hold `enemies/spin.rs` and `props/spin.rs`. Count them so an ambiguous leaf
    // can be left out of the table rather than silently resolving to whichever
    // was written last.
    let mut leaf_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for src in &sources {
        if let Some(leaf) = src.file_name().and_then(|n| n.to_str()) {
            *leaf_counts.entry(leaf.to_string()).or_default() += 1;
        }
    }

    let mut mods = String::new();
    let mut entries = String::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for (i, src) in sources.iter().enumerate() {
        let leaf = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("unreadable script name: {}", src.display()))?;
        // Project-relative, forward-slashed: the spelling a `ScriptComponent`
        // holds, and unique by construction where a leaf is not.
        let rel = src
            .strip_prefix(project_dir)
            .unwrap_or(src)
            .to_string_lossy()
            .replace('\\', "/");

        // Staged under an index, not its leaf, or two `spin.rs` would overwrite
        // each other in `src/` and one script would silently become the other.
        let staged_name = format!("script_{i}.rs");
        let text =
            std::fs::read_to_string(src).map_err(|e| format!("read {}: {e}", src.display()))?;
        write_if_changed(&crate_dir.join("src").join(&staged_name), &text)?;

        mods.push_str(&format!("#[path = \"{staged_name}\"]\nmod script_{i};\n"));
        // Correction H: one canonical row per script. LoadedScripts::insert
        // derives the bare-leaf alias from the canonical id; a duplicate
        // bare row would create a second competing canonical id and make
        // the bare alias ambiguous.
        renzora_identity::CanonicalId::from_rooted(renzora_identity::RootKind::Project, &rel)
            .map_err(|e| format!("bad script relpath {rel:?}: {e}"))?;
        entries.push_str(&format!(
            "        (renzora_identity::CanonicalId::from_rooted(\
             renzora_identity::RootKind::Project, \"{rel}\").unwrap(), \
             script_{i}::renzora_plugin_tier1_script_call),\n"
        ));
        if leaf_counts.get(leaf).copied().unwrap_or(0) != 1
            && !ambiguous.contains(&leaf.to_string())
        {
            ambiguous.push(leaf.to_string());
        }
    }

    // Named rather than silently dropped: a script attached by bare leaf that
    // now resolves to nothing would otherwise look like the export lost it.
    if !ambiguous.is_empty() {
        progress(format!(
            "Note: {} script name(s) appear in more than one folder ({}). Those are \
             reachable only by their full project-relative path — attaching one by \
             file name alone is ambiguous and will not resolve.",
            ambiguous.len(),
            ambiguous.join(", ")
        ));
    }

    // Phase 4: the generated scripts target the versioned Tier 1
    // C-ABI in `renzora_plugin::script`. Each `script_<i>.rs`
    // imports `renzora_plugin::script::*` and emits
    // `pub fn __tier1_entry(...) -> Result<(), String>` so the
    // generated aggregator can collect them as a `CompiledScript`
    // table without linking Bevy, the engine, or the editor.
    let manifest = String::from(
        "# GENERATED by the lean exporter — see `renzora_export::build`.\n\
         # The project's Rust scripts, compiled into this build's binary.\n\
         [package]\n\
         name = \"renzora_static_scripts\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [dependencies]\n\
         renzora_plugin = { path = \"../renzora_plugin\", default-features = false, features = [\"script\"] }\n\
         renzora_identity = { path = \"../renzora_identity\" }\n\
         \n\
         [lints]\n\
         workspace = true\n"
    );
    write_if_changed(&crate_dir.join("Cargo.toml"), &manifest)?;

    let lib = format!(
        "//! GENERATED by the lean exporter — see `renzora_export::build`.\n\
         //!\n\
         //! The project's Rust scripts, compiled into this binary. Overwritten on\n\
         //! every lean export; edits here do not survive one.\n\
         \n\
         use renzora_identity::{{CanonicalId, RootKind}};\n\
         use renzora_plugin::script::ScriptEntry;\n\
         \n\
         {mods}\n\
         /// Every script compiled in, as `(canonical id, ScriptEntry)`.\n         /// F4-1: the typed user fn never crosses the dynamic-library\n         /// boundary; the per-cdylib entry is what the host sees, and\n         /// `LoadedScripts::insert_borrowed` registers it.\n\
         pub fn scripts() -> Vec<(CanonicalId, ScriptEntry)> {{\n\
         \x20   vec![\n{entries}    ]\n\
         }}\n"
    );
    write_if_changed(&crate_dir.join("src").join("lib.rs"), &lib)?;

    progress(format!(
        "Compiling {} Rust script(s) into the binary: {}",
        sources.len(),
        sources
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(true)
}

fn write_if_changed(path: &Path, content: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Copy-if-newer of `src` → `dest`, skipping build/vcs noise and one named file
/// (the manifest, which [`stage_static_plugins`] writes patched instead).
///
/// `Cargo.lock` rides along harmlessly: a path dependency's lockfile is ignored,
/// the workspace's own is what resolves the build.
fn sync_dir_except(
    src: &Path,
    dest: &Path,
    skip_file: &str,
    copied: &mut usize,
) -> Result<(), String> {
    const DEEP_SKIP: &[&str] = &["target", ".git"];
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let s = entry.path();
        let d = dest.join(&name);
        if ft.is_dir() {
            if DEEP_SKIP.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            std::fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            sync_dir(&s, &d, copied)?;
        } else if ft.is_file() && name.to_string_lossy() != skip_file && should_copy(&s, &d) {
            std::fs::copy(&s, &d).map_err(|e| format!("copy {}: {e}", s.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

/// Read the plugin's real manifest, apply the three edits described on
/// [`stage_static_plugins`], and write the result into the copy.
///
/// Reads the pristine source and writes only on a difference, so an unchanged
/// plugin selection leaves the copied manifest's mtime alone — patching in place
/// over a synced file could not do that, because the patched and pristine
/// versions differ in size and the sync would keep clobbering it.
fn patch_plugin_manifest(src_dir: &Path, dest_manifest: &Path) -> Result<(), String> {
    let src = src_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&src).map_err(|e| format!("read {}: {e}", src.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("parse {}: {e}", src.display()))?;

    doc.remove("workspace");
    doc.remove("profile");
    let lib = doc
        .entry("lib")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    if let Some(lib) = lib.as_table_like_mut() {
        let mut kinds = toml_edit::Array::new();
        kinds.push("rlib");
        lib.insert("crate-type", toml_edit::value(kinds));
    }

    write_if_changed(dest_manifest, &doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_modding_sdk_preserves_siblings_without_legacy_metadata() {
        let installation = tempfile::tempdir().expect("installation");
        let output = tempfile::tempdir().expect("export");
        let sdk = installation.path().join("rust-sdk");
        let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        renzora_rust_sdk::package(&engine, &sdk).expect("package source SDK");
        assert!(stage_modding_sdk(installation.path(), output.path(), &mut |_| {}).expect("stage"));
        assert!(output
            .path()
            .join("rust-sdk/crates/renzora_identity/src/lib.rs")
            .is_file());
        let obsolete = output.path().join("rust-sdk/obsolete.rs");
        std::fs::write(&obsolete, "old exported source").expect("obsolete file");
        assert!(
            stage_modding_sdk(installation.path(), output.path(), &mut |_| {}).expect("refresh")
        );
        assert!(!obsolete.exists());
        assert!(!output.path().join("sdk").exists());
        assert!(!output.path().join("sdk.tar.zst").exists());
    }

    #[test]
    fn incomplete_source_modding_sdk_does_not_fall_back_to_legacy_archive() {
        let installation = tempfile::tempdir().expect("installation");
        let output = tempfile::tempdir().expect("export");
        std::fs::create_dir(installation.path().join("rust-sdk")).expect("broken SDK");
        std::fs::write(installation.path().join("sdk.tar.zst"), "old metadata").expect("legacy");
        assert!(stage_modding_sdk(installation.path(), output.path(), &mut |_| {}).is_err());
        assert!(!output.path().join("sdk.tar.zst").exists());
    }

    /// The minimum that counts as a script: it calls `renzora_plugin::rust_script!`,
    /// which is what `collect_project_scripts` looks for.
    const SCRIPT: &str = "use renzora_plugin::script::*;\nfn update(_c: &Ctx, _r: &mut ScriptReply) -> Result<(), String> { Ok(()) }\nrenzora_plugin::rust_script!(update);\n";

    /// A project with two scripts produces a module and a table entry for each,
    /// and copies the sources beside the generated lib.
    ///
    /// The module-per-script is what keeps them from colliding: both may define
    /// a `renzora_script_update` (the `no_mangle` is dropped under
    /// `static_scripts`) and both may define their own `Spin` component.
    #[test]
    fn static_scripts_generate_a_module_and_entry_per_file() {
        let project =
            std::env::temp_dir().join(format!("renzora_static_scripts_{}", std::process::id()));
        let scripts = project.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        // Written out of order to prove the sort: `orbit` must come first, so a
        // rebuild with no source change regenerates byte-identical output.
        std::fs::write(scripts.join("spin.rs"), SCRIPT).unwrap();
        std::fs::write(scripts.join("orbit.rs"), SCRIPT).unwrap();
        // Not a script — must not be compiled in.
        std::fs::write(scripts.join("notes.txt"), "ignore me").unwrap();
        // Rust, but not a script: no `script!`, so nothing to call. Compiling it
        // would fail on a file its author never called a script.
        std::fs::write(scripts.join("helper.rs"), "pub fn helper() {}").unwrap();

        let ws = project.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let staged = stage_static_scripts(&project, &ws, &mut |_| {}).unwrap();
        assert!(staged, "two scripts is something to link");

        let crate_dir = ws.join("crates").join("renzora_static_scripts");
        let lib = std::fs::read_to_string(crate_dir.join("src").join("lib.rs")).unwrap();
        assert!(
            lib.contains("#[path = \"script_0.rs\"]\nmod script_0;"),
            "{lib}"
        );
        assert!(
            lib.contains("#[path = \"script_1.rs\"]\nmod script_1;"),
            "{lib}"
        );
        // Correction H: the generator emits exactly one canonical row per
        // script. Bare-leaf aliases are derived by LoadedScripts::insert
        // at load time, so no second canonical row is written here.
        assert!(
            lib.contains("\"scripts/orbit.rs\").unwrap(), script_0::"),
            "{lib}"
        );
        assert!(
            !lib.contains("(\"orbit.rs\","),
            "bare-leaf row must NOT be emitted (correction H): {lib}"
        );
        assert!(
            lib.contains("\"scripts/spin.rs\").unwrap(), script_1::"),
            "{lib}"
        );
        assert!(
            !lib.contains("notes"),
            "non-scripts must not be linked in: {lib}"
        );

        // Sources copied beside the generated lib under their index, so the
        // export copy stays self-contained and two same-named scripts in
        // different folders cannot overwrite each other.
        assert_eq!(
            std::fs::read_to_string(crate_dir.join("src").join("script_1.rs")).unwrap(),
            SCRIPT
        );
        // Rust that is not a script stays out — a lean export compiles every
        // collected file into the binary, so one non-script would fail the whole
        // build rather than be skipped.
        assert!(
            !lib.contains("helper"),
            "non-scripts must not be linked in: {lib}"
        );

        // The manifest must turn the `script` feature on for
        // `renzora_plugin`, or the script cdylib won't compile against
        // the `renzora_plugin::script::*` surface.
        let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();
        assert!(
            manifest.contains("features = [\"script\"]"),
            "F4-1: manifest must enable the `script` feature on `renzora_plugin`: {manifest}"
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    /// Scripts are found anywhere in the project, build output is skipped, and a
    /// name used in two folders stays reachable by path without either script
    /// silently becoming the other.
    #[test]
    fn static_scripts_scan_the_whole_project() {
        let project =
            std::env::temp_dir().join(format!("renzora_scan_project_{}", std::process::id()));
        for sub in ["scripts", "enemies", "props", ".renzora/scripts", "target"] {
            std::fs::create_dir_all(project.join(sub)).unwrap();
        }
        std::fs::write(project.join("scripts/orbit.rs"), SCRIPT).unwrap();
        // Same leaf in two different folders — legal, and ambiguous by leaf.
        std::fs::write(project.join("enemies/spin.rs"), SCRIPT).unwrap();
        std::fs::write(project.join("props/spin.rs"), SCRIPT).unwrap();
        // Build output must never be compiled in: `.renzora/` holds the editor's
        // own staged copies, and `target/` is cargo's.
        std::fs::write(project.join(".renzora/scripts/orbit.rs"), SCRIPT).unwrap();
        std::fs::write(project.join("target/leftover.rs"), SCRIPT).unwrap();

        let ws = project.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(stage_static_scripts(&project, &ws, &mut |_| {}).unwrap());

        let lib =
            std::fs::read_to_string(ws.join("crates/renzora_static_scripts/src/lib.rs")).unwrap();

        // Found outside `scripts/`.
        assert!(lib.contains("\"enemies/spin.rs\").unwrap(),"), "{lib}");
        assert!(lib.contains("\"props/spin.rs\").unwrap(),"), "{lib}");
        // Correction H: bare-leaf rows are never emitted. The bare
        // alias for `orbit.rs` is derived from the canonical row by
        // LoadedScripts::insert at load time.
        assert!(
            !lib.contains("(\"spin.rs\""),
            "ambiguous leaf must not resolve: {lib}"
        );
        assert!(
            !lib.contains("(\"orbit.rs\""),
            "bare-leaf row must NOT be emitted (correction H): {lib}"
        );
        // Build output excluded.
        assert!(!lib.contains("leftover"), "{lib}");
        assert!(!lib.contains(".renzora"), "{lib}");

        let _ = std::fs::remove_dir_all(&project);
    }

    /// A project with no scripts generates nothing, so the caller leaves the
    /// feature off rather than compiling an empty aggregator.
    #[test]
    fn no_scripts_generates_nothing() {
        let project =
            std::env::temp_dir().join(format!("renzora_no_scripts_{}", std::process::id()));
        std::fs::create_dir_all(project.join("scripts")).unwrap();
        let ws = project.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        assert!(!stage_static_scripts(&project, &ws, &mut |_| {}).unwrap());
        assert!(!ws.join("crates").join("renzora_static_scripts").exists());

        let _ = std::fs::remove_dir_all(&project);
    }

    /// The two lines of `[profile.dist-lean]` the patcher edits, as they appear
    /// in the real root manifest.
    const MANIFEST: &str = "\
[profile.dist-lean]
inherits = \"dist\"
lto = \"thin\"
opt-level = \"s\"
strip = \"symbols\"
";

    fn patch(opts: LeanProfile) -> String {
        // Unique per test so a parallel run can't share a manifest. No tempfile
        // dev-dependency in this crate, and one directory per (pid, opts) is
        // enough — the values differ in every call site below.
        let dir = std::env::temp_dir().join(format!(
            "renzora_lean_profile_{}_{}{}{}",
            std::process::id(),
            opts.panic_abort as u8,
            opts.opt_level_z as u8,
            opts.codegen_units_one as u8,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), MANIFEST).unwrap();
        patch_lean_profile(&dir, opts, &mut |_| {}).unwrap();
        let out = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn all_off_leaves_the_profile_at_its_checked_in_settings() {
        let out = patch(LeanProfile::default());
        assert!(out.contains("opt-level = \"s\""), "{out}");
        assert!(!out.contains("panic"), "{out}");
        assert!(!out.contains("codegen-units"), "{out}");
        // The knobs must not disturb what the profile already says.
        assert!(out.contains("lto = \"thin\""), "{out}");
        assert!(out.contains("strip = \"symbols\""), "{out}");
    }

    #[test]
    fn all_on_writes_every_knob() {
        let out = patch(LeanProfile {
            panic_abort: true,
            opt_level_z: true,
            codegen_units_one: true,
        });
        assert!(out.contains("panic = \"abort\""), "{out}");
        assert!(out.contains("opt-level = \"z\""), "{out}");
        assert!(out.contains("codegen-units = 1"), "{out}");
    }

    /// The export copy persists between exports, so turning a knob back off has
    /// to actually remove what the last export wrote — otherwise a stale
    /// `panic = "abort"` silently applies to a build that asked for unwinding.
    #[test]
    fn turning_a_knob_back_off_reverts_the_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "renzora_lean_profile_revert_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), MANIFEST).unwrap();

        let on = LeanProfile {
            panic_abort: true,
            opt_level_z: true,
            codegen_units_one: true,
        };
        patch_lean_profile(&dir, on, &mut |_| {}).unwrap();
        patch_lean_profile(&dir, LeanProfile::default(), &mut |_| {}).unwrap();

        let out = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!out.contains("panic"), "{out}");
        assert!(!out.contains("codegen-units"), "{out}");
        // `opt-level` is set rather than removed: removing it would inherit
        // `dist`'s speed-tuned `opt-level = 2`.
        assert!(out.contains("opt-level = \"s\""), "{out}");
    }
}

#[cfg(test)]
mod lean_assembly_tests {
    //! End-to-end validation of the lean export. Drives the production
    //! [`assemble_lean_export_workspace`] and then runs `cargo check`
    //! on the assembled workspace, using the same
    //! [`lean_cargo_args`] builder that [`build_lean`] issues after
    //! assembly. The test lives in-crate (not in `tests/`) so it can
    //! call the `pub(crate)` assembly function without exposing it as
    //! public API.
    use super::*;

    fn script_body() -> &'static str {
        "use renzora_plugin::script::*;\nfn update(_: &Ctx, _r: &mut ScriptReply) -> Result<(), String> { Ok(()) }\nrenzora_plugin::rust_script!(update);\n"
    }

    /// Find the engine workspace root by walking up from this crate's
    /// `Cargo.toml`.
    fn engine_root() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR must be inside the engine workspace")
            .canonicalize()
            .expect("engine root must be canonicalizable")
    }

    /// Drive the production lean workspace assembly, then run the
    /// same `cargo check --bin renzora --features runtime,static_scripts`
    /// that the export's runtime integration path requires.
    ///
    /// Substitutes `check` for `build` because the test runs far more
    /// often than a real export. The package (`--bin renzora`),
    /// features (`runtime,static_scripts`), `--no-default-features`,
    /// and the assembled workspace path all match production; the
    /// profile is `dist` for a faster test, not `dist-lean`. Every
    /// other flag is shared with `build_lean` via the
    /// [`lean_cargo_args`] builder.
    #[test]
    fn generated_lean_workspace_compiles_in_production_assembly() {
        // Required tool: cargo. Missing cargo is a failure, not a skip.
        let cargo_status = std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            cargo_status,
            "cargo is required for the lean-compilation validation but is not available on PATH"
        );

        // Build a synthetic project with two scripts. The export path
        // for these exercises both the static-scripts feature (when
        // scripts exist) and the runtime integration that consumes
        // `renzora_static_scripts::scripts()`.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        std::fs::create_dir_all(project.join("enemy")).unwrap();
        std::fs::write(project.join("enemy/spin.rs"), script_body()).unwrap();
        std::fs::create_dir_all(project.join("props")).unwrap();
        std::fs::write(project.join("props/spin.rs"), script_body()).unwrap();

        let engine = engine_root();
        let mut progress = |s: String| eprintln!("[lean] {s}");
        let profile = LeanProfile {
            panic_abort: false,
            opt_level_z: false,
            codegen_units_one: false,
        };
        let platform = Platform::current().expect("host platform known");
        let (workspace, has_scripts) = assemble_lean_export_workspace(
            &engine,
            project,
            platform,
            &[],
            &[],
            profile,
            &[],
            &mut progress,
        )
        .expect("assemble_lean_export_workspace");
        assert!(
            has_scripts,
            "the assembled workspace must report has_scripts = true; \
             `build_lean` keys `static_scripts` off this flag"
        );

        // Build the exact production command for the assembled
        // workspace: same package (`--bin renzora`), same feature set,
        // same `--no-default-features`, same workspace path. The only
        // substitution is `check` for `build` and `dist` for
        // `dist-lean`, both via the production builder.
        let cross = Platform::current() != Some(platform);
        let mut cmd = std::process::Command::new("cargo");
        lean_cargo_args(
            &mut cmd,
            &workspace,
            platform,
            cross,
            LeanProfileArg::Dist,
            "runtime,static_scripts",
            "check",
        );
        // The lean exporter pins RUSTFLAGS through CARGO_ENCODED_RUSTFLAGS
        // to drop the dev-time `prefer-dynamic`. The test mirrors it so the
        // two paths exercise the same rustflags side of the build.
        if !cross && matches!(platform, Platform::LinuxX64) {
            cmd.env(
                "CARGO_ENCODED_RUSTFLAGS",
                // `build_lean` uses the empty string for non-Windows to
                // *override* (not merge) the config's `prefer-dynamic` /
                // `mold` flags. The test does the same.
                "",
            );
        }
        cmd.env("CARGO_TARGET_DIR", workspace.join("target"));
        cmd.env_remove("RUSTC_WRAPPER");

        let output = cmd.output().expect("cargo check spawn");
        assert!(
            output.status.success(),
            "cargo check on the assembled lean workspace must succeed.\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
