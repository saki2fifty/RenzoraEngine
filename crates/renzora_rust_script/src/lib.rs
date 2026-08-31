//! Rust scripts: per-entity native code, compiled from the project, with the
//! same `&mut World` an exclusive system gets.
//!
//! ```ignore
//! // <project>/scripts/spin.rs
//! use bevy::prelude::*;
//!
//! fn update(world: &mut World, me: Entity) {
//!     let dt = world.resource::<Time>().delta_secs();
//!     if let Some(mut t) = world.get_mut::<Transform>(me) {
//!         t.rotate_y(dt);
//!     }
//! }
//!
//! renzora::script!(update);
//! ```
//!
//! Attached exactly like a Lua script: drop it into the entity's **Scripts**
//! component. Routing is by file extension, the same way `.lua`, `.blueprint`
//! and `.bp` already route.
//!
//! Everything Bevy allows is allowed: spawn hierarchies, build UI, insert
//! components, swap materials, reach other entities by [`Entity`]. There is no
//! vocabulary in the way, because the script and the engine share one Bevy.
//!
//! # How this fits the scripting layer, and where it does not
//!
//! A backend normally returns [`ScriptCommand`]s for a queue to apply — safe,
//! interchangeable, and exactly what a Rust script does not want. So this crate
//! splits the two halves a backend usually does together:
//!
//! * [`backend::RustScriptBackend`] **claims** `.rs`, so the Scripts component
//!   accepts one and the execution loop does not flag it as broken.
//! * [`dispatch`] **runs** it, from an exclusive system with the real world.
//!
//! # A script IS a native plugin
//!
//! Not "like one" — the same thing, built by the same compiler driver against
//! the same SDK. The only difference is the convention: a plugin exports a
//! `Plugin` and installs once, a script exports a per-entity function called for
//! each entity carrying it. So the limits are the plugin limits, not new ones —
//! see `crates/renzora_native_plugin`.
//!
//! # Reloading
//!
//! Saving a script rebuilds it, off the main thread, and swaps the function
//! pointer — see [`watch`]. Every reload leaks the old image, because a schedule
//! or a captured closure may still hold pointers into it; a restart reclaims it.
//!
//! # What is not solved yet
//!
//! **Nothing in a lean export.** A static build links no shared images, so a
//! script library has nothing to bind to. The answer is to compile scripts INTO
//! the export, which the lean exporter is already shaped for.
//!
//! **No props.** Lua declares tunables in a table the backend parses; the Rust
//! equivalent is reading attributes off the source. Until then, a script's
//! tunables are ordinary components on the entity, which the inspector already
//! edits.

pub mod backend;
pub mod discovery;
pub mod script_resolve;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use libloading::{Library, Symbol};
use renzora::core::console_log::{console_error, console_success};
use renzora::{CurrentProject, SplashState};
use renzora_identity::{BareAliasIndex, CanonicalId};
use renzora_plugin_build::Sdk;
use renzora_scripting::{scripts_should_run, ScriptComponent};

pub use script_resolve::{build_artifact_path, build_dir_name, ResolvedScript};
pub use watch::PendingRetires;

/// The symbol a script exports, written by [`renzora::script!`].
pub const SCRIPT_SYMBOL: &[u8] = b"renzora_script_update\0";

/// Its signature.
///
/// A plain Rust `fn` taking `&mut World`, sound only because the script and the
/// engine link one shared `bevy_dylib` — the precondition everything here rests
/// on.
pub type ScriptFn = fn(&mut World, Entity);

renzora::add!(RustScriptPlugin, Runtime);

#[derive(Default)]
pub struct RustScriptPlugin;

impl Plugin for RustScriptPlugin {
    fn build(&self, app: &mut App) {
        // ── Compiled-in scripts: no compiler, no libraries, no watching ──────
        //
        // The whole first half of this plugin exists to turn `.rs` files into
        // dylibs and load them. A statically linked build has them already —
        // `renzora_static_scripts::scripts()` is a table the lean exporter
        // generated and the linker resolved — so all that is left is to publish
        // the table and dispatch from it.
        //
        // Deliberately checked BEFORE `dynamic_linking`: an export has neither
        // the shared images nor a Rust toolchain, and would otherwise take the
        // early return below and report `No backend for Some("rs")`.
        #[cfg(feature = "static_scripts")]
        {
            app.init_resource::<LoadedScripts>()
                .add_systems(PreUpdate, (register_backend, load_static_scripts))
                .add_systems(
                    Update,
                    dispatch
                        .run_if(scripts_should_run)
                        .after(renzora_scripting::ScriptingSet::PreScript),
                );
            return;
        }

        #[cfg(not(feature = "static_scripts"))]
        if !cfg!(feature = "dynamic_linking") {
            debug!("rust scripts unavailable: this build links no shared engine image");
            return;
        }
        app.init_resource::<LoadedScripts>()
            .init_resource::<watch::ScriptWatcher>()
            .init_resource::<watch::PendingRetires>()
            // Recompile on save. Unlike `dispatch` these are NOT gated on play
            // mode: a script should build when you save it, so the error is in
            // front of you while you are still looking at the code — not the next
            // time you press play.
            .add_systems(
                Update,
                (watch::watch, watch::compile_for_new_project, watch::finish),
            )
            // Claims `.rs` with the engine. Not done in `build` because the
            // engine is a resource another plugin creates, and plugin build
            // order is not something to depend on.
            // Shipped scripts load here rather than on `SplashState::Editor`,
            // because an exported game never enters that state — and because it
            // has nothing to compile: the libraries were built by the editor at
            // export time and travel beside the executable. In the editor the
            // manifest is absent and this is a single failed file read.
            .add_systems(PreUpdate, (register_backend, load_prebuilt_scripts))
            // Compiling is separate from dispatching so one script failing to
            // build leaves the others running, and so the compile can later move
            // off the main thread without touching the dispatcher.
            .add_systems(OnEnter(SplashState::Editor), compile_and_load)
            // Gated exactly like the Lua path. Without this a script starts
            // running the moment it is dropped on an entity, in edit mode, which
            // is both surprising and destructive — a script that spawns or
            // despawns would do so while you are still arranging the scene.
            //
            // Ordered after `ScriptingSet::PreScript` because that is where
            // `ScriptsActive` — the resource the run condition reads — is filled
            // for the frame. Unordered, this would see last frame's answer
            // whenever the scheduler happened to run it first, so toggling a
            // script's preview button would take effect a frame later here than
            // in the Lua path for no reason anyone could see.
            .add_systems(
                Update,
                dispatch
                    .after(renzora_scripting::ScriptingSet::PreScript)
                    .run_if(scripts_should_run),
            );
    }

    /// Take ownership of `ScriptsActive` when nothing else has.
    ///
    /// `ScriptingPlugin` normally fills that resource once per frame, and the
    /// run condition above reads it. But that plugin sits behind the runtime's
    /// strippable `scripting` feature — a shipped game with no Lua drops the
    /// whole host layer — while this plugin is added unconditionally by the
    /// generated plugin list. In that build the run condition would ask for a
    /// resource with no owner and panic on the first frame, which is the worst
    /// possible place to find out: an exported game, not the editor.
    ///
    /// `.rs` scripts do not need the scripting host to run (they are dispatched
    /// from here against `&mut World`), so the right answer is to keep gating
    /// them rather than to silently stop. Adding `renzora_scripting`'s own
    /// system re-uses the rule instead of restating it.
    ///
    /// In `finish` rather than `build` because it has to observe whether
    /// `ScriptingPlugin` was added, and plugin build order is not something to
    /// depend on — `finish` runs after every `build`.
    fn finish(&self, app: &mut App) {
        if app.is_plugin_added::<renzora_scripting::ScriptingPlugin>() {
            return;
        }
        app.init_resource::<renzora_scripting::ScriptsActive>()
            .configure_sets(Update, renzora_scripting::ScriptingSet::PreScript)
            .add_systems(
                Update,
                renzora_scripting::update_scripts_active
                    .in_set(renzora_scripting::ScriptingSet::PreScript),
            );
    }
}

/// Register the `.rs` backend the first time a [`ScriptEngine`] exists.
///
/// A polled `Local` rather than a one-shot at startup, because the engine may be
/// created after this plugin is built — and if it is not registered, `.rs` is an
/// extension nothing claims: the Scripts component refuses the drop, the picker
/// does not list it, and the execution loop reports `No backend for Some("rs")`.
///
/// The steady-state cost is one `bool` test per frame.
fn register_backend(
    engine: Option<ResMut<renzora_scripting::ScriptEngine>>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(mut engine) = engine else { return };
    engine.add_backend(Box::new(backend::RustScriptBackend::default()));
    *done = true;
}

/// Publish the compiled-in script table into [`LoadedScripts`].
///
/// Runs once. There is nothing to compile, nothing to load and nothing that can
/// fail — the entry points were resolved by the linker, so if the binary started
/// they are valid. That is the whole difference from `compile_and_load`: the
/// same map, filled from an array instead of from `dlopen`.
///
/// Keyed by canonical id (`project://<rel>`) so duplicate-leaf scripts
/// retain their distinct identities; bare-name aliasing is handled by
/// `LoadedScripts::insert`'s idempotent alias update.
#[cfg(feature = "static_scripts")]
fn load_static_scripts(mut loaded: ResMut<LoadedScripts>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;
    let table = renzora_static_scripts::scripts();
    if table.is_empty() {
        return;
    }
    for (id, f) in &table {
        // Lean-exports have no Library at runtime (the script is compiled
        // in), so we use `insert_borrowed`. The alias index is still
        // populated so dispatch's BareAliasIndex lookup works.
        loaded.insert_borrowed(id.clone(), *f);
    }
    info!(
        "[rust-script] {} script(s) compiled into this build",
        table.len()
    );
}

/// Every script image loaded this session, and each one's entry point.
///
/// `ManuallyDrop` because a resource is dropped with the World on every clean
/// shutdown, and unmapping code something may still call has crashed the runtime
/// here before. See `renzora_plugin`'s loader.
///
/// Phase 1 commit 1.3: keyed by [`CanonicalId`]. The previous file-name
/// string key is gone. The dispatch resolves a `ScriptComponent::script_path`
/// to a canonical id via [`script_resolve::LoadedScripts::resolve`], which
/// tries project-relative canonical, then bare-leaf unique, and reports
/// ambiguity rather than failing at build time.
#[derive(Resource, Default)]
pub struct LoadedScripts {
    pub(crate) entries: HashMap<CanonicalId, ScriptFn>,
    alias_index: BareAliasIndex,
    _images: Vec<std::mem::ManuallyDrop<Library>>,
}

impl LoadedScripts {
    pub fn is_loaded(&self, id: &CanonicalId) -> bool {
        self.entries.contains_key(id)
    }

    /// Point the canonical id at a newly loaded image.
    ///
    /// Replacing the entry retires the previous function pointer, but the image
    /// it lived in is kept — see [`crate::watch`] for why unmapping it is not an
    /// option.
    ///
    /// Idempotent on the alias index: re-inserting the same canonical id
    /// does NOT add a duplicate bare-leaf entry. A reload of one script
    /// therefore preserves its bare alias uniqueness.
    pub fn insert(&mut self, id: CanonicalId, f: ScriptFn, lib: Library) {
        let already_present = self.entries.contains_key(&id);
        if !already_present {
            // First insert for this id — record the bare alias.
            self.alias_index.insert(id.clone());
        }
        self.entries.insert(id, f);
        self._images.push(std::mem::ManuallyDrop::new(lib));
    }

    /// Same as [`Self::insert`] but for the case where one `Library` is shared
    /// across multiple canonical ids (the copy-based export's "keys outnumber
    /// libraries" pattern). Records the function pointer under a new id without
    /// taking a second `Library` reference.
    ///
    /// Idempotent on the alias index — see [`Self::insert`].
    pub fn insert_borrowed(&mut self, id: CanonicalId, f: ScriptFn) {
        let already_present = self.entries.contains_key(&id);
        if !already_present {
            self.alias_index.insert(id.clone());
        }
        self.entries.insert(id, f);
    }

    /// Look up by canonical identity.
    pub fn lookup(&self, id: &CanonicalId) -> Option<ScriptFn> {
        self.entries.get(id).copied()
    }

    /// Remove a canonical identity from the index. The mapped `Library`
    /// stays alive — function pointers the engine still calls may
    /// reference symbols inside it. The next dispatch will see the id
    /// resolve to nothing and skip the call.
    ///
    /// Updates the bare-alias index too: a previously-ambiguous leaf
    /// name becomes unique again once one of its canonical ids is
    /// removed.
    pub fn remove(&mut self, id: &CanonicalId) {
        self.entries.remove(id);
        self.alias_index.remove(id);
    }

    /// Resolve a `ScriptComponent::script_path` against the open project.
    pub fn resolve(&self, path: &Path, project_root: &Path) -> ResolvedScript {
        script_resolve::resolve_script_identity(path, project_root, &self.alias_index)
    }

    /// Identity-keyed iterator for tests.
    #[cfg(test)]
    pub fn ids(&self) -> Vec<CanonicalId> {
        let mut v: Vec<CanonicalId> = self.entries.keys().cloned().collect();
        v.sort();
        v
    }
}

/// On entering the editor, build and load every `.rs` in the open
/// project's `scripts/`. Runs once per editor entry; a project — and
/// therefore a `scripts/` directory — does not exist before then.
///
/// The lifecycle transitions for first open, switch, and reopen all
/// route through `compile_and_load_for_project` so the orchestration
/// has one definition.
fn compile_and_load(world: &mut World) {
    let Some(project) = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.clone())
    else {
        return;
    };
    compile_and_load_for_project(world, &project);
}

/// Production orchestration for "open / switch into this project and
/// load its scripts". Used by both `compile_and_load` (the OnEnter
/// system that runs on first editor entry) and the watcher's project
/// lifecycle for switches and reopens.
///
/// Discovery happens against the project root; sources that lack a
/// script-declaration are skipped. Each discovered id is recorded as
/// seen by the watcher BEFORE the build runs, so the next reconcile
/// does not re-build it. A failed compile is recorded as seen too —
/// matching the watcher's own rule that a broken script stays quiet
/// until the next edit.
///
/// The shared SDK is loaded lazily from `<exe-dir>/sdk` because the
/// installation is per-binary; without an SDK nothing compiles and a
/// single warn / console-error covers all scripts (no per-script
/// failures for the same root cause).
pub(crate) fn compile_and_load_for_project(world: &mut World, project: &Path) {
    let sources: Vec<PathBuf> = crate::discovery::collect_rust_scripts(project);
    if sources.is_empty() {
        return;
    }

    let Some(root) = exe_dir() else { return };
    let sdk = match Sdk::load(root.join("sdk")) {
        Ok(sdk) => sdk,
        Err(e) => {
            warn!("rust scripts cannot be built: {e}");
            console_error("Script", format!("Rust scripts cannot be built — {e}"));
            return;
        }
    };

    for src in sources {
        let canonical = crate::discovery::project_relpath_for(project, &src);
        let canonical = match canonical {
            Some(c) => c,
            None => continue,
        };
        let canonical_for_log = canonical.clone();
        let canonical_for_build = canonical.clone();
        let canonical_for_task = canonical.clone();
        world
            .resource_mut::<watch::ScriptWatcher>()
            .mark_seen(canonical_for_log.clone());
        let build_root = project.to_path_buf();
        let task_path = src.clone();
        match build_to_path_with_id(&sdk, &build_root, &task_path, &canonical_for_build)
            .and_then(|p| load_library(&p))
        {
            Ok((f, lib)) => {
                world
                    .resource_mut::<LoadedScripts>()
                    .insert(canonical_for_task, f, lib);
                info!("[rust-script] loaded {canonical_for_log}");
                console_success("Script", format!("compiled {canonical_for_log}"));
            }
            Err(e) => {
                error!("[rust-script] {canonical_for_log}: {e}");
                console_error("Script", format!("{canonical_for_log}\n{e}"));
            }
        }
    }
}

/// Compile one `.rs` into `<project>/.renzora/scripts/<dir>/`, returning the
/// library. The build directory name is keyed off the canonical identity so
/// that two identical canonical ids always produce the same build output —
/// duplicating a script under a different folder no longer collides in
/// `.renzora/scripts/`.
///
/// Split from [`load_library`] so the compile — the second that matters — can run
/// on a task pool while the load stays on the main thread. Nothing here touches
/// the `World`, which is what makes that possible.
///
/// The build directory is hidden inside the project because these are derived:
/// they belong with the project, but nobody should be asked to look at or commit
/// them.
pub fn build_to_path_with_id(
    sdk: &Sdk,
    project: &Path,
    src: &Path,
    id: &renzora_identity::CanonicalId,
) -> Result<PathBuf, String> {
    validate_build_dir_marker(project, id)?;
    let dir_name = script_resolve::build_dir_name(id);
    let build = project.join(".renzora").join("scripts").join(&dir_name);
    std::fs::create_dir_all(build.join("src")).map_err(|e| e.to_string())?;

    std::fs::copy(src, build.join("src").join("lib.rs")).map_err(|e| e.to_string())?;

    let gen = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let out = build.join(format!(
        "{}-{gen}.{}",
        id.bare_leaf(),
        sdk.manifest().lib_ext
    ));

    sdk.compile(&build, &out).map_err(|e| {
        e.to_string().replace(
            &build
                .join("src")
                .join("lib.rs")
                .to_string_lossy()
                .to_string(),
            &src.to_string_lossy(),
        )
    })?;

    script_resolve::write_build_dir_marker(project, id)
        .map_err(|e| format!("write marker: {e}"))?;
    Ok(out)
}

/// Validate the per-directory marker for `id` BEFORE any other file
/// mutation. Returns `Ok(())` when:
/// - the marker does not exist;
/// - the marker names `id`'s scheme-path (an existing successful
///   build of the same canonical id — that build is being rebuilt).
///
/// Returns `Err(...)` when:
/// - the marker names a DIFFERENT canonical id's scheme-path (a real
///   hash collision — the existing directory and its staged source
///   must not be touched);
/// - the marker file is unreadable for any other reason.
///
/// `build_to_path_with_id` calls this before any other mutation.
/// Callers that perform staging on their own (the exporter, for
/// example) can use this as a pre-flight to refuse a colliding
/// directory without taking the SDK dependency.
pub fn validate_build_dir_marker(
    project: &Path,
    id: &renzora_identity::CanonicalId,
) -> Result<(), String> {
    let marker = script_resolve::build_dir_marker_path(project, id);
    match std::fs::symlink_metadata(&marker) {
        Ok(meta) if meta.file_type().is_file() => {
            match script_resolve::read_build_dir_marker(&marker) {
                Ok(Some(text)) if text == id.to_scheme_path() => Ok(()),
                Ok(Some(other)) => Err(format!(
                    "build-directory hash collision: {} already claimed by {other}; \
                     refusing to overwrite the existing build directory",
                    marker.display()
                )),
                Ok(None) => Ok(()),
                Err(e) => Err(format!("read marker {}: {e}", marker.display())),
            }
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("stat marker {}: {e}", marker.display())),
    }
}

/// `dlopen` a built script and find its entry point.
///
/// SAFETY: code compiled from the project's own source, put there by the person
/// running the editor. Same trust model as a plugin.
/// Every Rust script in a project: any `.rs` under it, at any depth, that
/// declares itself one.
///
/// Shared by the editor's compiler and the exporter, deliberately. They used to
/// disagree — the editor read `<project>/scripts/` flat while the exporter walked
/// the whole tree — and a script in a subfolder would then be compiled into a
/// lean export having never once run in the editor. One definition removes the
/// class of bug rather than the instance.
///
/// **Anywhere, not just `scripts/`.** A script is attached by path and nothing
/// requires that path to live in one directory; a project may keep scripts beside
/// the scenes that use them.
///
/// **Declared, not merely `.rs`.** The marker is `renzora::script!`, which every
/// script must call to export an entry point at all.
///
/// Not for native plugins — those live in `<editor>/plugins/`, never inside a
/// project. The reason is the lean export: it compiles every file this returns
/// *into the game binary*, so one `.rs` that is not a script — a helper module, a
/// vendored snippet, anything a user happens to keep — fails the entire export
/// build rather than being skipped. A copy-based export degrades more gently
/// (that file simply has no library) but still reports a script that was never
/// one. Requiring the declaration keeps both failures off files their author
/// never called a script.
///
/// Re-exported for tests and the exporter, which used the old name. The new
/// canonical implementation lives in [`crate::discovery`].
#[deprecated(note = "use `crate::discovery::collect_rust_scripts` instead")]
pub fn collect_project_scripts(project: &Path) -> Vec<PathBuf> {
    crate::discovery::collect_rust_scripts(project)
}

/// Phase 1 commit 1.4: declaration recognition backed by `rustc_lexer`
/// 0.1.0. Returns `true` when the file source declares a script via
/// `renzora::script!(...)` somewhere the lexer treats as code (not a
/// comment, string, byte string, raw string, char literal, or
/// identifier continuation). Truncated source returns `false` and never
/// panics.
pub fn declaration_recognised(source: &str) -> bool {
    matches!(
        renzora_identity::Recogniser::new().scan(source),
        renzora_identity::Declaration::Recognised,
    )
}

/// `declares_script` kept here as a thin wrapper for backward-compatibility
/// with `crates/renzora_plugin_build` callers that still expect it. The
/// real declaration test lives in the watcher / discovery module.
#[allow(dead_code)]
fn declares_script(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    declaration_recognised(&text)
}

/// The manifest a copy-based export ships beside the script libraries.
///
/// One line per key, `key<TAB>library-file`. Deliberately not JSON: the runtime
/// needs no parser for two columns, and the exporter writes it with `format!`.
/// A script is registered under more than one key (its project-relative path and,
/// where unambiguous, its bare file name), so keys outnumber libraries and the
/// same file appears on several lines.
pub const PREBUILT_MANIFEST: &str = "scripts.index";

/// Load the script libraries a copy-based export shipped.
///
/// This is the third way a `.rs` script can run, and the one that makes an
/// exported game work without asking anything of the player:
///
/// | | compiled | loaded by |
/// |---|---|---|
/// | editor | on save, from source | `compile_and_load` |
/// | copy-based export | at export time, by the editor | this |
/// | lean export | into the binary | `load_static_scripts` |
///
/// No SDK and no Rust toolchain are involved — the editor did the compiling and
/// the game ships the result. It works because a copy-based export carries the
/// same `bevy_dylib` and `renzora_dylib` the script was compiled against, so
/// there is one `World` type on both sides of the boundary. (A *lean* export
/// links Bevy statically and shares no image, which is why it compiles scripts
/// in rather than loading them.)
///
/// Runs once. Absent manifest means no scripts were shipped, which is the normal
/// case for a project that has none.
fn load_prebuilt_scripts(mut loaded: ResMut<LoadedScripts>, mut done: Local<bool>) {
    if *done {
        return;
    }
    *done = true;

    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let dir = dir.join("scripts");
    let Ok(index) = std::fs::read_to_string(dir.join(PREBUILT_MANIFEST)) else {
        return;
    };

    // One `Library` per FILE, not per key: two keys naming the same library must
    // share one image, or the second `dlopen` would map a second copy of a
    // library holding its own statics. Same image is shared across keys that
    // point to the same library file (Phase 1 commit 1.3: keys are canonical
    // ids plus an unambiguous bare-name alias).
    let mut opened: HashMap<String, ScriptFn> = HashMap::new();
    let mut count = 0usize;
    for line in index.lines() {
        let Some((key, file)) = line.split_once('\t') else {
            continue;
        };
        let (key, file) = (key.trim(), file.trim());
        if key.is_empty() || file.is_empty() {
            continue;
        }
        let canonical = match renzora_identity::CanonicalId::parse(key) {
            Ok(c) => c,
            Err(_) => {
                // Pre-Phase-1 exports wrote project-relative rows without
                // the `project://` prefix. Interpret them as project
                // canonical ids by prefixing the scheme. If the row
                // still fails parsing, treat it as a true malformed
                // entry and emit an explicit unsupported-version error.
                match renzora_identity::CanonicalId::from_rooted(
                    renzora_identity::RootKind::Project,
                    key,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            "[rust-script] malformed prebuilt manifest row {key:?}: {e}"
                        );
                        continue;
                    }
                }
            }
        };
        if let Some(f) = opened.get(file) {
            loaded.insert_borrowed(canonical, *f);
            continue;
        }
        match load_library(&dir.join(file)) {
            Ok((f, lib)) => {
                opened.insert(file.to_string(), f);
                loaded.insert(canonical, f, lib);
                count += 1;
            }
            Err(e) => {
                // Loud, and not fatal: one unloadable script should not stop the
                // others, and the game is still playable minus that behaviour.
                error!("[rust-script] could not load shipped script {file}: {e}");
                console_error("Script", format!("could not load {file}: {e}"));
            }
        }
    }
    if count > 0 {
        info!("[rust-script] loaded {count} shipped script librar(ies)");
    }
}

pub fn load_library(path: &Path) -> Result<(ScriptFn, Library), String> {
    let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
    let f: Symbol<ScriptFn> = match unsafe { lib.get(SCRIPT_SYMBOL) } {
        Ok(f) => f,
        Err(_) => {
            // Leaked rather than returned to be dropped. `Library::new` already
            // ran the image's static initializers, and unmapping a warmed Rust
            // dylib runs `FreeLibrary` inside the loader lock — the deadlock
            // `renzora_plugin`'s loader hit. A script missing its entry point is
            // an author typo, so this happens while someone iterates: exactly
            // the situation where it would be hit repeatedly.
            std::mem::forget(lib);
            return Err(
                "exports no entry point — did you forget `renzora::script!(update);`?".to_string(),
            );
        }
    };
    let f = *f;
    Ok((f, lib))
}

/// The directory holding the editor, which is where `sdk/` lives.
pub fn sdk_root() -> Option<PathBuf> {
    exe_dir()
}

// Whether scripts should run this frame is `renzora_scripting`'s
// `scripts_should_run`, imported above rather than reimplemented here. This used
// to be a hand-kept copy because the original was private, with a comment noting
// that the two "must agree" — a Rust script that ran in edit mode while the Lua
// script beside it did not would be a confusing bug to chase. It is now `pub`
// and reads a resource computed once per frame, so the copy is gone and the two
// paths cannot drift apart. `finish` below covers the one build where the
// resource would otherwise have no owner.

/// Call each entity's `.rs` scripts once per frame.
///
/// Exclusive, because a script takes `&mut World` and nothing else may be
/// borrowed while it runs — which is also why the pairs are collected first.
pub fn dispatch(
    world: &mut World,
    // Cached rather than `world.query::<…>()` per call. Building a `QueryState`
    // walks every archetype in the world to work out which ones match, and this
    // runs each frame of play mode in a scene with thousands of them — paid to
    // rediscover an answer that changes only when an archetype is created.
    // `Local` keeps one across frames and `iter` updates it incrementally.
    mut q: Local<bevy::ecs::query::QueryState<(Entity, &'static ScriptComponent)>>,
) {
    // Edit-mode preview: the run condition let us through because at least one
    // script has its inspector play button on, not because play mode started. Run
    // ONLY those, so the rest of the scene stays static — same rule as the Lua
    // executor.
    let preview_only = world
        .get_resource::<renzora::PlayModeState>()
        .map(|pm| !pm.is_scripts_running())
        .unwrap_or(false);

    // Resolve every script_path into a canonical id (or Ambiguous / NotFound)
    // by asking LoadedScripts. The dispatch becomes identity-keyed, not leaf-
    // keyed, so two scripts at `enemies/spin.rs` and `props/spin.rs` resolve
    // to two distinct ids while a bare `spin.rs` resolves through the alias
    // index.
    let project_root = world
        .get_resource::<CurrentProject>()
        .map(|p| p.path.as_path())
        .unwrap_or_else(|| Path::new(""));

    let mut resolved: Vec<(Entity, CanonicalId)> = Vec::new();
    let mut unresolved: Vec<(Entity, std::path::PathBuf, ResolvedScript)> = Vec::new();
    for (entity, sc) in q.iter(world) {
        for entry in &sc.scripts {
            if !entry.enabled {
                continue;
            }
            if preview_only && !entry.preview {
                continue;
            }
            let Some(path) = entry.script_path.as_ref() else {
                continue;
            };
            if path.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let outcome = world
                .resource::<LoadedScripts>()
                .resolve(path, project_root);
            match outcome {
                ResolvedScript::Unique(id) => resolved.push((entity, id)),
                other => unresolved.push((entity, path.clone(), other)),
            }
        }
    }

    // Log unresolved once each (per-frame, but the outcome is stable; the
    // Console panel coalesces repeats). A failed resolution is not a panic
    // — the script simply doesn't run this frame.
    for (entity, path, outcome) in &unresolved {
        match outcome {
            ResolvedScript::Ambiguous(ids) => {
                let leaves: Vec<String> = ids.iter().map(|c| c.path().to_string()).collect();
                error!(
                    "[rust-script] ambiguous bare alias '{}' on {entity}: {} candidates",
                    path.to_string_lossy(),
                    leaves.join(", ")
                );
                console_error(
                    "Script",
                    format!(
                        "ambiguous bare alias '{}' — use the full project-relative path; candidates: {}",
                        path.to_string_lossy(),
                        leaves.join(", "),
                    ),
                );
            }
            ResolvedScript::NotFound => {
                warn!(
                    "[rust-script] unresolved script '{}' on {entity}",
                    path.to_string_lossy()
                );
            }
            ResolvedScript::Unique(_) => unreachable!(),
        }
    }

    if resolved.is_empty() {
        return;
    }

    // Snapshot the function pointers so we can drop the resource borrow
    // before invoking anything that mutates the world.
    let mut pairs: Vec<(Entity, ScriptFn)> = Vec::with_capacity(resolved.len());
    {
        let loaded = world.resource::<LoadedScripts>();
        for (entity, id) in resolved {
            if let Some(f) = loaded.lookup(&id) {
                pairs.push((entity, f));
            }
        }
    }

    for (entity, f) in pairs {
        // An earlier script may have despawned this entity — its own, even.
        if world.get_entity(entity).is_err() {
            continue;
        }
        // A panic crossing the boundary is undefined, so it is caught: a broken
        // script stops working rather than taking the editor with it. A segfault
        // is still fatal and nothing here can help with that.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(world, entity))).is_err() {
            error!("[rust-script] panicked on {entity}");
            // Deliberately not per-frame-guarded: a panicking script panics every
            // frame, and a Console that says so sixty times is still better than
            // one that says it once and scrolls away. The panel coalesces repeats
            // into a count.
            console_error("Script", format!("panicked while running on {entity}"));
        }
    }
}

/// The directory holding `sdk/`, which is where scripts are compiled against.
///
/// NOT simply the executable's parent: inside a Linux AppImage that is a
/// read-only temporary mount with no SDK beside it. See
/// [`renzora_plugin_build::install`].
fn exe_dir() -> Option<PathBuf> {
    renzora_plugin_build::install::root()
}
