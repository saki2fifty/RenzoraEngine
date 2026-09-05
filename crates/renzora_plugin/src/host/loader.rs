//! Finds `renzora_plugin` cdylibs on disk and initialises them.
//!
//! Deliberately symbol-dispatched: a library is treated as a C-ABI plugin only
//! if it exports [`sys::INIT_SYMBOL`]. Anything else is skipped silently, which
//! is what lets these live in the same `plugins/` directory as the older
//! `dynamic_plugin_loader` dylibs during the migration — each loader recognises
//! its own and ignores the rest.

use bevy::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use libloading::{Library, Symbol};
#[cfg(target_arch = "wasm32")]
use wasm_dl::{Library, Symbol};
use crate::static_link::StaticPlugin;
use crate::sys;
use renzora_identity::CanonicalId;

/// Derive a stable canonical identity for a directory-plugin
/// (a C-ABI plugin discovered by path under a configured
/// `discovery_root`).
///
/// The identity encodes:
///   * all relative PARENT directories preserved verbatim (so
///     `plugins/a/State.so` and `plugins/b/State.so` produce
///     different identities), and
///   * the LOGICAL plugin name — the final filename's stem with
///     the load-prefix convention applied per extension —
///     so the same logical plugin produces the same `CanonicalId`
///     on Linux, macOS and Windows:
///
///     | on-disk filename       | logical name           |
///     | ---                    | ---                    |
///     | `foo.dll`              | `foo`                  |
///     | `libfoo.so`            | `foo`                  |
///     | `libfoo.dylib`         | `foo`                  |
///     | `libfoo.dll`           | `libfoo`               |
///     | `liblibfoo.so`         | `libfoo`               |
///     | `liblibfoo.dylib`      | `libfoo`               |
///     | `effects/libweather.so`| `effects/weather`      |
///     | `effects/weather.dll`  | `effects/weather`      |
///     | `effects/libfoo.dll`   | `effects/libfoo`       |
///
/// The cross-platform normalisation rule is the single source of
/// truth in [`logical_plugin_name_for_filename`] — this function
/// only adds the discovery-root-relative path prefix on top.
///
/// Returns an error if the path is not under `discovery_root`,
/// contains `..` / empty segments, has a non-UTF-8 component,
/// or its final filename is not a supported dynamic-library
/// extension with a non-empty logical name. Callers MUST
/// propagate the error — silently falling back to a basename-only
/// identity would re-introduce a collision class.
pub fn canonical_id_for_path(
    path: &Path,
    discovery_root: &Path,
) -> Result<CanonicalId, String> {
    let rel = path.strip_prefix(discovery_root).map_err(|e| {
        format!(
            "plugin path `{}` is not under discovery root `{}`: {e}",
            path.display(),
            discovery_root.display()
        )
    })?;
    if rel.is_absolute() {
        return Err(format!(
            "plugin path `{}` is not under discovery root `{}`: the relative path is absolute",
            path.display(),
            discovery_root.display()
        ));
    }
    // Walk every parent component and the final stem. The final
    // stem is the only place that gets cross-platform-normalized.
    let mut logical_components: Vec<String> = Vec::new();
    let mut iter = rel.components();
    let last = iter.next_back().ok_or_else(|| {
        format!(
            "plugin path `{}` (relative to `{}`) has no final component",
            path.display(),
            discovery_root.display()
        )
    })?;
    for component in iter {
        if component == std::path::Component::CurDir {
            continue;
        }
        let seg = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| format!("plugin path `{}` has a non-UTF-8 segment", path.display()))?;
        if seg.is_empty() || seg == ".." {
            return Err(format!(
                "plugin path `{}` (relative `{}`) is not a valid canonical identity path",
                path.display(),
                rel.display()
            ));
        }
        logical_components.push(seg.to_string());
    }
    // Final component: this is the dynamic-library filename. The
    // cross-platform normalisation rule is centralised in
    // [`logical_plugin_name_for_filename`] so the extension-aware
    // `lib`-strip logic lives in exactly one place.
    let last_os = last.as_os_str();
    let last_str = last_os
        .to_str()
        .ok_or_else(|| format!("plugin path `{}` has a non-UTF-8 final segment", path.display()))?;
    if last_str.is_empty() {
        return Err(format!(
            "plugin path `{}` (relative `{}`) has an empty final segment",
            path.display(),
            rel.display()
        ));
    }
    let last_path = Path::new(last_str);
    let last_logical = logical_plugin_name_for_filename(last_path).map_err(|e| {
        format!(
            "plugin path `{}` (relative `{}`) is not a valid plugin filename: {e}",
            path.display(),
            rel.display()
        )
    })?;
    logical_components.push(last_logical);
    let logical = logical_components.join("/");
    let id_str = format!("engine://{logical}");
    CanonicalId::parse(&id_str).map_err(|e| {
        format!(
            "plugin path `{}` produced an invalid canonical identity `{id_str}`: {e:?}",
            path.display()
        )
    })
}

/// Extension-aware logical-name extraction for a dynamic-library
/// filename. The single source of truth for the cross-platform
/// `lib`-strip rule.
///
/// Rules (applied in order):
///
///   1. The extension is one of `so` (Linux), `dylib` (macOS),
///      `dll` (Windows MSVC + Windows GNU). Any other extension
///      returns `Err` so the caller does not silently fall back
///      to a guess.
///   2. The stem (the file name without the extension) MUST be
///      non-empty. A filename like `.dll` has an empty stem and
///      is rejected with `Err`.
///   3. For `.so` and `.dylib`, strip EXACTLY ONE leading `lib`
///      prefix (the Unix load-prefix convention). The remaining
///      characters are preserved verbatim, including any
///      subsequent `lib` characters.
///   4. For `.dll`, do NOT strip a `lib` prefix — Windows does
///      not add one, so a `libfoo.dll` is genuinely a crate
///      named `libfoo`.
///   5. The resulting logical name MUST be non-empty. A
///      `lib.so` filename has stem `lib` and would be stripped to
///      an empty string; that is rejected with `Err`.
///
/// The error message names both the rejected filename and the
/// specific rule that fired so the caller can surface a useful
/// diagnostic.
///
/// Examples:
///
///   | filename            | logical name   | notes                       |
///   | ---                 | ---            | ---                         |
///   | `foo.dll`           | `foo`          | ordinary Windows crate      |
///   | `libfoo.so`         | `foo`          | ordinary Linux crate        |
///   | `libfoo.dylib`      | `foo`          | ordinary macOS crate        |
///   | `liblibfoo.so`      | `libfoo`       | crate genuinely named `libfoo` (Linux) |
///   | `liblibfoo.dylib`   | `libfoo`       | crate genuinely named `libfoo` (macOS) |
///   | `libfoo.dll`        | `libfoo`       | crate genuinely named `libfoo` (Windows) |
///   | `library.dll`       | `library`      | ordinary Windows crate (starts with `lib`) |
///   | `lib.so`            | (rejected)     | stem `lib` would strip to empty |
///   | `.dll`              | (rejected)     | stem is empty               |
///   | `foo.txt`           | (rejected)     | unsupported extension       |
pub fn logical_plugin_name_for_filename(filename: &Path) -> Result<String, String> {
    let stem = filename
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("filename `{}` has a non-UTF-8 stem", filename.display()))?;
    let ext = filename
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| {
            // Catches `lib.so` (which has an empty stem AND
            // `Path::extension()` returning `Some("so")` on
            // Windows but `None` on some platforms where the
            // basename is a single component) and bare `foo` (no
            // extension). The empty-stem check below catches the
            // `.dll` case where `file_stem` returns `Some("")`.
            format!(
                "filename `{}` has no extension; expected one of `so`, `dylib`, `dll`",
                filename.display()
            )
        })?;
    if stem.is_empty() {
        // `.dll` has `Path::extension() == Some("dll")` on
        // Windows and `file_stem() == Some("")` on every host.
        // The empty-stem check is the right place to reject it
        // — the `dll` arm of the match below has no other
        // strip-prefix logic, so an empty stem would otherwise
        // produce an empty logical name and be persisted.
        return Err(format!(
            "filename `{}` has an empty logical name; a plugin crate \
             cannot be empty",
            filename.display()
        ));
    }
    match ext {
        "so" | "dylib" => {
            // Strip exactly one leading `lib`. A second `lib`
            // (e.g. `liblibfoo.so`) is preserved verbatim — the
            // crate is genuinely named `libfoo`.
            let stripped = stem.strip_prefix("lib").unwrap_or(stem);
            if stripped.is_empty() {
                return Err(format!(
                    "filename `{}` strips the Unix load prefix to an empty \
                     logical name; a plugin crate cannot be named `lib`",
                    filename.display()
                ));
            }
            Ok(stripped.to_string())
        }
        "dll" => {
            // Windows does NOT add a `lib` prefix to a cdylib's
            // output filename. The stem is the logical crate
            // name as-is. The empty-stem check above has already
            // rejected `.dll` (which has stem `""`).
            Ok(stem.to_string())
        }
        other => Err(format!(
            "filename `{}` has unsupported extension `.{other}`; \
             expected one of `so`, `dylib`, `dll`",
            filename.display()
        )),
    }
}

/// Backward-compatible wrapper for callers that lack a configured
/// discovery root. The resulting identity is a sanitised bare
/// filename stem — unstable across restarts that change the
/// installation directory and unstable across nested plugins with
/// the same name. Used ONLY by the non-`load_dir` paths that load a
/// single plugin from a caller-supplied path (e.g. tests, the
/// scene-stream loader). DO NOT use this from `load_dir` — use
/// `canonical_id_for_path(path, discovery_root)` instead — and DO
/// NOT use it from the hot-reload path (the watcher looks up the
/// initial identity from the stored slot, so this fallback never
/// runs there either).
pub fn canonical_id_for_legacy_path(path: &Path) -> CanonicalId {
    // The legacy entry point falls back to a sanitised bare
    // filename. The legacy callers (unit tests, the scene-stream
    // loader) already control their input filenames; if a caller's
    // path is malformed, the helper returns `Err` and we fall back
    // to the basename-only legacy identity.
    let safe = match logical_plugin_name_for_filename(path) {
        Ok(name) => name,
        Err(_) => path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("legacy")
            .to_string(),
    };
    CanonicalId::parse(&format!("engine://{safe}"))
        .unwrap_or_else(|_| {
            CanonicalId::from_rooted(
                renzora_identity::RootKind::Engine,
                &format!("legacy://{safe}"),
            )
            .expect("synthetic identity is well-formed")
        })
}

use std::path::{Path, PathBuf};

/// `libloading`'s shape, for a platform that has no dynamic loading at all.
///
/// The web has no `dlopen`, no `LoadLibrary`, and no `plugins/` folder to scan —
/// a wasm build gets its plugins linked in (`static_link`) or not at all. The
/// alternative to this shim was `#[cfg]`-ing the whole module out, which would
/// have meant cfg-ing every caller of [`LoadedPlugins`] across the runtime for a
/// platform where they all correctly find nothing anyway.
///
/// So: opening always fails, `scan_plugins` finds no plugins, and the statically
/// linked ones are unaffected. `Symbol` can never be constructed (`get` only ever
/// returns `Err`), which is what makes its `Deref` unreachable rather than wrong.
#[cfg(target_arch = "wasm32")]
mod wasm_dl {
    use std::ffi::OsStr;
    use std::marker::PhantomData;
    use std::ops::Deref;

    #[derive(Debug)]
    pub struct Error;

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("dynamic library loading is not available on wasm")
        }
    }

    pub struct Library;

    impl Library {
        /// # Safety
        /// Never loads anything, so trivially sound; `unsafe` only to match
        /// `libloading::Library::new`'s signature.
        pub unsafe fn new<P: AsRef<OsStr>>(_path: P) -> Result<Self, Error> {
            Err(Error)
        }

        /// # Safety
        /// Unreachable — a `Library` cannot be constructed on this target.
        pub unsafe fn get<T>(&self, _symbol: &[u8]) -> Result<Symbol<T>, Error> {
            Err(Error)
        }
    }

    pub struct Symbol<T>(PhantomData<T>);

    impl<T> Deref for Symbol<T> {
        type Target = T;
        fn deref(&self) -> &T {
            unreachable!("Library::get never returns Ok on wasm")
        }
    }
}

/// One plugin path, across every load of it.
///
/// A slot's index in [`LoadedPlugins`] is its permanent identity: entries are
/// never removed, so an index stamped on a registration stays valid for the life
/// of the process. That is what the ownership tags on panels, render passes and
/// component schemas refer to.
pub struct PluginSlot {
    pub path: PathBuf,
    /// The canonical identity the directory loader derived for
    /// this path at initial load time. The hot-reload watcher
    /// consults this instead of re-deriving the identity from the
    /// file stem, so a Linux plugin (`libfoo.so`) and a Windows
    /// plugin (`foo.dll`) at the same logical location receive the
    /// same identity both at boot and on reload.
    ///
    /// `None` for slots created by the legacy `load_one` /
    /// scene-stream / unit-test paths that supply their own
    /// identity up front; those callers already know the identity
    /// they want and never re-derive it from the filename.
    pub directory_identity: Option<CanonicalId>,
    /// The scope this path reported, once anything has read it. `None` until a
    /// load got far enough to ask.
    ///
    /// Recorded so [`scan_plugins`] can answer "what scope is this plugin?"
    /// without mapping the image again — which in the editor would map a *second*
    /// instance of it, since the running one is a shadow copy under a different
    /// filename. See [`scan_plugins`] for why a second instance is not merely
    /// wasteful.
    pub scope: Option<sys::PluginScope>,
    /// Shared with every system this slot's plugins registered. Bumping it
    /// retires the previous load's systems — see `host::GenGate`.
    pub generation: super::PluginGeneration,
    /// The generation of the newest load that succeeded.
    pub loaded_at: u32,
    /// How many images have been loaded for this path. Zero means the next load is
    /// the first, which is what distinguishes "generation 0" from "reload to
    /// generation 1" — `loaded_at` alone cannot, since it starts at 0 too.
    pub images: usize,
    /// **Every** library ever loaded for this path, and none of them is ever
    /// dropped.
    ///
    /// Retired systems are removed, but that alone does not prove all panel,
    /// render or backend callbacks and plugin-owned threads have quiesced.
    /// Dropping a `libloading::Library` has also deadlocked in `FreeLibrary`
    /// here before. Keep images mapped until a separate full teardown contract
    /// proves unloading safe; restarting the process reclaims them.
    ///
    /// `ManuallyDrop`, and not merely "we never call `remove`": [`LoadedPlugins`]
    /// is an ECS resource, so a plain `Vec<Library>` is dropped when the World
    /// is — which is every clean shutdown. That ran `FreeLibrary` on every
    /// plugin image at process teardown and exited the game runtime with an
    /// access violation (0xC0000005). The editor never saw it because
    /// `kill_on_app_exit` calls `std::process::exit` before the World unwinds,
    /// so it happened to skip the drop; the runtime unwinds properly and paid
    /// for it. "Never dropped" has to include the last moment of the process,
    /// which is the one moment a `Vec` field does not give you for free.
    ///
    /// This holds libraries from **successful** loads. A failed load
    /// (rolled-back transaction, missing symbol, wrong scope, ABI mismatch,
    /// layout conflict) also opens the image and never closes it — see
    /// `load_one_transactional`, which pushes failed images into
    /// `failed_libraries` so they share the same process-lifetime safety.
    _libraries: Vec<std::mem::ManuallyDrop<Library>>,
    /// Libraries opened by a failed load. Same never-dropped safety as
    /// `_libraries`. Holds every `Library` the loader produced but did not
    /// keep in `_libraries` because the activation did not commit.
    failed_libraries: Vec<std::mem::ManuallyDrop<Library>>,
}

impl PluginSlot {
    /// Construct a fresh slot for `path`. `failed_libraries` starts empty;
    /// `load_one_transactional` populates it for every library it opens but
    /// does not commit.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            directory_identity: None,
            scope: None,
            generation: super::PluginGeneration::default(),
            loaded_at: 0,
            images: 0,
            _libraries: Vec::new(),
            failed_libraries: Vec::new(),
        }
    }
}

/// Every plugin path the loader has seen, indexed by slot.
#[derive(Resource, Default)]
pub struct LoadedPlugins(pub Vec<PluginSlot>);

impl LoadedPlugins {
    /// The slot for `path`, creating one if this is the first sighting.
    fn slot_for(&mut self, path: &Path) -> usize {
        if let Some(i) = self.0.iter().position(|s| s.path.as_path() == path) {
            return i;
        }
        self.0.push(PluginSlot::new(path.to_path_buf()));
        self.0.len() - 1
    }

    /// The scope already read for `path`, if any load has read one.
    ///
    /// What lets [`scan_plugins`] skip mapping an image it has already seen.
    fn scope_of(&self, path: &Path) -> Option<sys::PluginScope> {
        self.0
            .iter()
            .find(|s| s.path.as_path() == path)
            .and_then(|s| s.scope)
    }
}

/// Outcome for one candidate file, for logging and the editor's plugin panel.
#[derive(Debug, Clone)]
pub enum LoadOutcome {
    Loaded,
    /// Turned off in Settings → Editor → Plugins. The file was never opened —
    /// see [`load_dir`] for why declining before `Library::new` matters rather
    /// than merely being tidier.
    Disabled,
    /// Not a C-ABI plugin — no init symbol. Expected for older dylib plugins.
    NotAPlugin,
    /// The plugin needs a newer host than this one.
    VersionTooOld,
    /// The plugin's own init returned a failure, or the library would not open.
    Failed(String),
    /// The plugin belongs in the other binary. Not an error — an editor plugin
    /// sitting in a game's `plugins/` directory is expected when both were
    /// staged from one build.
    WrongScope(sys::PluginScope),
}

/// Load every `renzora_plugin` cdylib in `dir`, except any already linked into
/// this binary.
///
/// Missing or unreadable directories are not an error — a build with no plugins
/// is normal.
///
/// `linked` holds the crate names of plugins compiled in (see [`load_static`]).
/// Loading a second copy of one is not a duplicate that resolves itself: the two
/// get separate slots, so BOTH sets of systems end up in the schedules and every
/// one of the plugin's systems runs twice a frame — and the second copy's
/// first-claim registrations (a script backend's extensions, a panel id) fail
/// with an error that reads like a conflict between two different plugins. An
/// export never produces this, because it skips copying what it linked; a user
/// pointing a game at the editor's `plugins/` folder produces it immediately.
pub fn load_dir(
    world: &mut World,
    dir: &Path,
    discovery_root: &Path,
    is_editor: bool,
    linked: &[&str],
    disabled: &[String],
) -> Vec<(PathBuf, LoadOutcome)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let ext = std::env::consts::DLL_EXTENSION;
    let mut results = Vec::new();
    // Track identities already accepted by THIS pass so two
    // platform-mismatched copies of the same logical plugin
    // (`libfoo.so` AND `foo.so` under the same discovery root)
    // do not both load. The watcher consults the stored slot's
    // identity, and re-running `load_one` with two different
    // paths that map to the same identity would re-allocate the
    // durable namespace.
    let mut accepted_identities: std::collections::HashSet<CanonicalId> =
        std::collections::HashSet::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        // `lib` is stripped because a cdylib is `lib<crate>.so` on Unix and
        // `<crate>.dll` on Windows, while a linked plugin is only ever known by
        // its crate name.
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let name = stem.strip_prefix("lib").unwrap_or(&stem);
        if linked.contains(&name) {
            info!("[plugin] ignoring {stem} in plugins/ — this build links {name} in");
            continue;
        }
        // Skipped **before** `Library::new`, which is the whole point rather
        // than an optimisation. Opening a plugin and then declining to use it
        // means either dropping the handle — the `FreeLibrary` deadlock this
        // loader has hit twice — or leaking an image and running the static
        // initializers of a plugin the user explicitly turned off. Never opening
        // it has neither problem.
        //
        // The name is checked rather than the file's symbols, so a disabled
        // plugin is not even sniffed. That costs one thing: a disabled library
        // that was never a plugin still gets a row in the report. Listing
        // something the user can turn back on is the harmless direction to be
        // wrong in.
        if disabled.iter().any(|d| d == name) {
            info!("[plugin] {name} is disabled — Settings → Editor → Plugins");
            results.push((path.clone(), LoadOutcome::Disabled));
            continue;
        }
        // Derive the canonical identity from the discovery-root-
        // relative path. This is the only point in the loader that
        // does so, so a basename-only fallback cannot sneak in.
        let identity = match canonical_id_for_path(&path, discovery_root) {
            Ok(id) => id,
            Err(reason) => {
                error!("{reason}");
                results.push((path.clone(), LoadOutcome::Failed(reason)));
                continue;
            }
        };
        // Duplicate detection: two platform-mismatched artefacts
        // for the same logical plugin (e.g. a stale `libfoo.so`
        // left in the folder after upgrading from Linux to
        // Windows, or vice versa) would both normalise to the
        // same identity. Reject the second one — the OS's loader
        // wouldn't be able to load it anyway, and the duplicate
        // would split the slot's storage in a way the watcher
        // cannot reconcile.
        if !accepted_identities.insert(identity.clone()) {
            let why = format!(
                "duplicate logical plugin `{identity}` — another file under `{}` \
                 normalises to the same canonical identity. Remove one of them so the \
                 slot has a single on-disk source.",
                discovery_root.display()
            );
            error!("{why}");
            results.push((path.clone(), LoadOutcome::Failed(why)));
            continue;
        }
        // Pin the identity on the slot BEFORE the load runs.
        // `load_one_with_identity` calls `slot_for`, which
        // allocates the slot on first sight; stamping the
        // identity here means the watcher's reload path can
        // re-use it without re-deriving from the file stem.
        {
            let mut loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
            let slot = loaded.slot_for(&path);
            loaded.0[slot].directory_identity = Some(identity.clone());
        }
        let outcome = load_one_with_identity(world, &path, identity, is_editor);
        // Recorded here rather than by the caller so every exit from this loop
        // reaches the report — including the disabled one, which never produces
        // anything for the caller to log.
        if !matches!(outcome, LoadOutcome::NotAPlugin) {
            world
                .get_resource_or_insert_with(PluginLoadReport::default)
                .record(name, &outcome);
        }
        results.push((path, outcome));
    }
    results
}

/// What became of every C-ABI plugin file this process considered.
///
/// Exists so the editor's Settings → Editor → Plugins list is built from what the
/// actually did rather than from a second `read_dir` with a second opinion about
/// what a plugin is. "Is this file a plugin?" has a non-obvious answer here — it
/// must export one specific symbol and must not be a proc-macro dylib — and a
/// panel listing a different set from the one the engine loaded is worse than no
/// panel at all.
///
/// Deliberately holds no `renzora` types. This crate is published to crates.io
/// so a third-party plugin author can `cargo add renzora_plugin`, which rules
/// out a path dependency on the contract crate — so the editor copies this into
/// `renzora::PluginInventory` instead, and the two loaders meet there.
#[derive(Resource, Default)]
pub struct PluginLoadReport {
    /// `(id, outcome)`, in the order the loader reached them. The id is the
    /// library's file stem with any `lib` prefix stripped, which is the same
    /// string the disable list is keyed on.
    pub entries: Vec<(String, LoadOutcome)>,
}

impl PluginLoadReport {
    fn record(&mut self, id: &str, outcome: &LoadOutcome) {
        self.entries.retain(|(existing, _)| existing != id);
        self.entries.push((id.to_string(), outcome.clone()));
    }
}

/// True if the file is a Rust **proc-macro** dylib.
///
/// These compile to a dylib for *rustc* to load, not for us, and `dlopen`ing one
/// into a process that is not the compiler crashes hard — before the splash,
/// with no panic and no crash report, because it is the OS loader failing rather
/// than Rust. We cannot detect that after `Library::new`, so detect it before:
/// every proc-macro dylib exports a `__rustc_proc_macro_decls_*` symbol, and the
/// name appears verbatim in the export table, so a plain byte search finds it
/// with no PE/ELF parsing.
///
/// Staging already filters these out (see xtask's `is_not_a_plugin`), but a dll
/// dropped into `plugins/` by hand must not be able to take the editor down.
fn is_proc_macro_dylib(path: &Path) -> bool {
    contains_symbol(path, b"__rustc_proc_macro_decls")
}

/// Whether this file is a C-ABI plugin, decided WITHOUT loading it.
///
/// Load-bearing, not an optimisation. `plugins/` also holds the older
/// Bevy-linking cdylibs, and those are already mapped by `dynamic_plugin_loader`.
/// Asking the OS to load one *by its original path* was harmless — same path, same
/// module, refcount++. But a plugin is now loaded from a **copy** under a fresh
/// filename (see [`shadow_copy`]), and the OS treats that as a different library:
/// it maps a whole second instance and re-runs its initialisers, including the
/// `inventory::submit!` ctors that register plugins. Doing that to seventy
/// Bevy-linking dylibs at boot is not a slow path, it is a broken one.
///
/// So the question "is this mine?" has to be answered from the bytes, before any
/// copy or load happens. Every C-ABI plugin exports [`sys::INIT_SYMBOL`], and an
/// exported name appears verbatim in the export table, so a byte search settles it
/// with no PE/ELF parsing — the same trick [`is_proc_macro_dylib`] uses.
fn exports_plugin_init(path: &Path) -> bool {
    contains_symbol(path, sys::INIT_SYMBOL.as_bytes())
}

fn contains_symbol(path: &Path, needle: &[u8]) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// Copy a plugin image somewhere private before loading it, and return where.
///
/// **This is what makes reload possible on Windows at all.** A mapped DLL is
/// locked, and the loader never unmaps one (retired systems still point into it),
/// so loading `plugins/drift.dll` directly would leave that file permanently
/// unwritable — `cargo build` could not overwrite it and the staging copy would
/// fail with "file in use". Loading a copy leaves the original free.
///
/// The generation is in the filename because the previous copy is *also* still
/// mapped and locked. Copies accumulate, one per reload, alongside the leaked
/// library images they belong to; the directory is cleared at startup.
///
/// `.reload` has no file extension, so [`load_dir`]'s extension filter skips it
/// and the copies are never mistaken for plugins to load.
///
/// **Editor-only.** A shipped game opens `plugins/<name>.dll` itself — see
/// [`load_one`] for why copying is actively harmful there.
fn shadow_copy(path: &Path, generation: u32) -> std::io::Result<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new(".")).join(".reload");
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = std::env::consts::DLL_EXTENSION;
    let dst = dir.join(format!("{stem}-{generation}.{ext}"));
    std::fs::copy(path, &dst)?;
    Ok(dst)
}

/// Remove shadow copies left by earlier sessions.
///
/// Editor-only, like the copies themselves. Safe here and nowhere else: at
/// `build` time nothing is mapped yet, so no copy is locked. Skipping a file
/// that refuses to delete is deliberate — a stale image is harmless (nothing
/// scans this directory), and failing the whole boot over it would not be.
fn clear_shadow_dir(dir: &Path) {
    let shadow = dir.join(".reload");
    if let Ok(entries) = std::fs::read_dir(&shadow) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn load_one_with_identity(
    world: &mut World,
    path: &Path,
    identity: CanonicalId,
    is_editor: bool,
) -> LoadOutcome {
    load_one(world, path, identity, is_editor)
}

fn load_one(
    world: &mut World,
    path: &Path,
    identity: CanonicalId,
    is_editor: bool,
) -> LoadOutcome {
    if is_proc_macro_dylib(path) {
        return LoadOutcome::NotAPlugin;
    }
    // BEFORE any copy or load: `plugins/` is shared with the older Bevy-linking
    // dylibs, and copying one to a new filename would make the OS map a second
    // instance of it. See `exports_plugin_init`.
    if !exports_plugin_init(path) {
        return LoadOutcome::NotAPlugin;
    }

    // Resolved before the image is opened, because the generation is part of the
    // shadow copy's filename.
    let (slot, counter, generation) = {
        let mut loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
        let slot = loaded.slot_for(path);
        let s = &loaded.0[slot];
        let first = s.images == 0;
        (
            slot,
            s.generation.clone(),
            if first { 0 } else { s.loaded_at + 1 },
        )
    };

    // Only the editor loads a copy. The copy exists so a rebuild can overwrite
    // the original while it is mapped (see [`shadow_copy`]) — a shipped game
    // never reloads a plugin, so it has nothing to buy there and one real cost:
    // `plugins/` is shared with whoever launched it. The editor spawns the game
    // as a child pointed at the same directory, and it already has every shadow
    // copy mapped and locked, so the child's `fs::copy` failed with "used by
    // another process" for EVERY plugin. The runtime window came up with no
    // audio backend, no scripting and no plugins at all — which reads as "audio
    // is broken outside the editor" rather than "the runtime loaded nothing".
    let image = if is_editor {
        match shadow_copy(path, generation) {
            Ok(p) => p,
            Err(e) => return LoadOutcome::Failed(format!("could not stage a copy to load: {e}")),
        }
    } else {
        path.to_path_buf()
    };

    // SAFETY: loading arbitrary native code is inherently unsafe — a plugin can
    // do anything the process can. That is the same trust model as the existing
    // dylib loader; the C ABI buys build-environment independence, not sandboxing.
    let library = match unsafe { Library::new(&image) } {
        Ok(l) => l,
        Err(e) => return LoadOutcome::Failed(format!("could not open: {e}")),
    };

    // Never unmapped, on ANY path out of here — including the ones that decide
    // this image is not wanted. `_libraries` already says a loaded plugin stays
    // mapped for the life of the process; this extends that to a rejected one,
    // because unloading is not merely wasteful, it **hangs**. `FreeLibrary` runs
    // the image's static destructors while holding the Windows loader lock, and
    // an image whose initialisers started a thread waits on that thread — which
    // cannot finish, because finishing needs the lock.
    //
    // Only a build that *rejects* a plugin can hit it, which is why the editor
    // never did and the game runtime always would: the runtime skips every
    // Editor-scope plugin, so it deadlocked partway through the plugins folder,
    // with no window, no message and no crash — a boot that simply stopped.
    let library = std::mem::ManuallyDrop::new(library);

    let init: Symbol<sys::ExtensionInit> =
        match unsafe { library.get(sys::INIT_SYMBOL.as_bytes()) } {
            Ok(s) => s,
            Err(_) => return LoadOutcome::NotAPlugin,
        };
    let init = *init;

    // Read the scope BEFORE calling init, so a plugin for the other binary never
    // gets the chance to register a system, a component or a panel. Checking
    // afterwards would mean unwinding registrations that already happened.
    let scope = match unsafe { library.get::<sys::ScopeEntry>(sys::SCOPE_SYMBOL.as_bytes()) } {
        Ok(f) => unsafe { f() },
        // No declaration means Runtime, matching `renzora::add!`'s default.
        Err(_) => sys::PluginScope::Runtime,
    };
    // Recorded here rather than on the success path, so the rejections below —
    // and a plugin whose own init fails — still leave the answer behind.
    // `scan_plugins` reads it instead of mapping the image a second time.
    world.resource_mut::<LoadedPlugins>().0[slot].scope = Some(scope);
    if !scope.is_known() {
        return LoadOutcome::Failed(format!(
            "declares scope {} which this build does not have",
            scope.0
        ));
    }
    if scope == sys::PluginScope::Editor && !is_editor {
        return LoadOutcome::WrongScope(scope);
    }

    // Take the slot's previous registrations back before the new build adds its
    // own, so a panel or a render pass is replaced rather than duplicated.
    // Keep the prior systems until init succeeds; cancelling them here would
    // stop last-good execution even when the new candidate is refused.
    let prior_loaded_at = world.resource::<LoadedPlugins>().0[slot].loaded_at;
    if generation > 0 {
        super::retire_slot_registrations(world, slot, prior_loaded_at);
    }

    match super::init_plugin_gen_with_non_persistable(
        world,
        init,
        counter.clone(),
        generation,
        slot,
        identity,
        false,
    ) {
        super::InitOutcome::Ok => {
            counter.store(generation, std::sync::atomic::Ordering::Relaxed);
            if generation > 0 {
                super::system_lifecycle::retire(world, slot, prior_loaded_at);
            }
            let mut loaded = world.resource_mut::<LoadedPlugins>();
            let s = &mut loaded.0[slot];
            s.loaded_at = generation;
            s.images += 1;
            // Moved, still wrapped: `into_inner` here used to hand a bare
            // `Library` to a `Vec` inside an ECS resource, which drops with the
            // World and put `FreeLibrary` back on the shutdown path (see
            // `_libraries`). Keeping the `ManuallyDrop` is what makes "never
            // dropped" true at process exit as well as during the run.
            s._libraries.push(library);
            LoadOutcome::Loaded
        }
        super::InitOutcome::VersionTooOld => LoadOutcome::VersionTooOld,
        super::InitOutcome::Failed => {
            LoadOutcome::Failed("plugin init returned Failed".to_string())
        }
        // The version matched and the shape did not, so the two were built from
        // headers that disagree about field order. Say that, rather than leaving
        // an author to wonder why a plugin with the right version number is
        // refused — the fix is a rebuild, not an engine update.
        super::InitOutcome::AbiMismatch => LoadOutcome::Failed(
            "plugin was built against a differently-shaped interface table — its version \
             matches but a field was inserted, reordered or retyped. Rebuild the plugin \
             against this engine's `renzora_plugin`"
                .to_string(),
        ),
        // A value from a newer ABI. Reaching this arm at all is what the newtype
        // bought: as a real enum the match above would have been exhaustive, and
        // an out-of-range discriminant would have been undefined behaviour here
        // rather than a case to handle.
        //
        // Refused rather than assumed successful — a plugin that reports a result
        // this build has no name for has not told us it loaded.
        other => LoadOutcome::Failed(format!(
            "plugin init returned status {:?} which this engine does not know — it was built \
             against a newer ABI. Rebuild it against this engine's `renzora_plugin`",
            other
        )),
    }
}

/// Initialise a plugin that is compiled into this binary.
///
/// The short version of everything [`load_one`] does that this does not: there
/// is no file, so there is nothing to sniff for an init symbol, nothing to copy
/// aside before mapping, and no library to keep alive — the code is already in
/// the binary's `.text` and outlives the process's interest in it. What remains
/// is the part that actually matters: read the scope before init so a plugin for
/// the other binary never registers anything, then run init against a slot.
///
/// The slot is keyed by a synthetic path (`<linked>/<id>`) rather than a real
/// one. It exists because panels, render passes and materials are all tagged with
/// their owning slot; a linked plugin needs an owner tag exactly as much as a
/// loaded one does. `<` and `>` cannot appear in a Windows filename, so the key
/// can never collide with a plugin on disk — and the extension filter in
/// [`load_dir`] and [`PluginWatcher`] means nothing ever tries to stat it.
///
/// Generation stays 0 forever: linked code cannot be swapped, so nothing retires
/// and no system ever goes stale.
pub fn load_static(
    world: &mut World,
    plugin: &StaticPlugin,
    identity: CanonicalId,
    is_editor: bool,
) -> LoadOutcome {
    if !plugin.scope.is_known() {
        return LoadOutcome::Failed(format!(
            "declares scope {} which this build does not have",
            plugin.scope.0
        ));
    }
    if plugin.scope == sys::PluginScope::Editor && !is_editor {
        return LoadOutcome::WrongScope(plugin.scope);
    }

    let path = PathBuf::from(format!("<linked>/{}", plugin.id));
    let (slot, counter) = {
        let mut loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
        let slot = loaded.slot_for(&path);
        loaded.0[slot].scope = Some(plugin.scope);
        let counter = loaded.0[slot].generation.clone();
        (slot, counter)
    };

    match super::init_plugin_gen_with_non_persistable(
        world,
        plugin.init,
        counter,
        0,
        slot,
        identity,
        false,
    ) {
        super::InitOutcome::Ok => {
            let mut loaded = world.resource_mut::<LoadedPlugins>();
            loaded.0[slot].images += 1;
            LoadOutcome::Loaded
        }
        super::InitOutcome::VersionTooOld => LoadOutcome::VersionTooOld,
        super::InitOutcome::Failed => {
            LoadOutcome::Failed("plugin init returned Failed".to_string())
        }
        // Unreachable in practice, and deliberately still handled: a linked
        // plugin was compiled against the very `renzora_plugin` in this build, so
        // its idea of the table's shape cannot differ. If it somehow does, saying
        // so beats reporting success.
        super::InitOutcome::AbiMismatch => LoadOutcome::Failed(
            "plugin was built against a differently-shaped interface table, which should be \
             impossible for a linked-in plugin — the export workspace is out of sync"
                .to_string(),
        ),
        other => LoadOutcome::Failed(format!(
            "plugin init returned status {:?} which this engine does not know",
            other
        )),
    }
}

/// What an integration crate sees after a transactional activation attempt.
///
/// The caller (`renzora_loose_plugins`) supplies the library handle, init
/// symbol and the candidate generation; this function performs every
/// mutation reversibly, and reports exactly one of the two outcomes. On
/// rollback the candidate's mutations are undone and the slot's prior
/// `loaded_at` and counter are restored — the previous generation is the
/// one that stays live.
#[derive(Debug, Clone)]
pub enum TransactionalActivationOutcome {
    Committed {
        slot: usize,
        generation: u32,
        committed_entries: Vec<crate::host::JournalEntry>,
    },
    RolledBack {
        slot: usize,
        proposed_generation: u32,
        failure: ActivationFailure,
        rolled_back_entries: Vec<crate::host::JournalEntry>,
    },
}

/// Why an activation failed to commit. Used by integration crates to set
/// the inventory status kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationFailure {
    /// Library could not be opened, or the required init symbol was missing.
    OpenFailed(String),
    /// The plugin's init returned `InitResult::Failed`.
    InitFailed,
    /// The plugin's init returned `InitResult::VersionTooOld`.
    VersionTooOld,
    /// The plugin's init returned `InitResult::AbiMismatch`.
    AbiMismatch,
    /// The plugin's init returned a status from a newer ABI.
    UnknownInitStatus(u32),
    /// A component or resource registered by the candidate changed
    /// layout relative to the existing live registration. Refused, the
    /// old generation stays active.
    LayoutConflict(String),
}

/// Run a plugin's init under a transactional activation gate.
///
/// The candidate registers systems with `at = proposed_generation`, while
/// the slot's `Generation` counter remains at `loaded_at`. Every system
/// the candidate registers therefore observes `at != counter` on its
/// `GenGate` and is inert for the duration of the transaction. On
/// `Commited` the counter is bumped to `proposed_generation`, which
/// flips the candidate's systems live and retires the prior build's.
/// On `RolledBack` the counter is restored to `loaded_at`, the candidate
/// systems stay permanently stale, and every registry mutation is
/// reversed via the journal.
///
/// `init` is the `ExtensionInit` symbol resolved from the library. `scope`
/// must already have been validated against the binary's `is_editor` flag
/// by the caller (this function performs no scope validation).
///
/// # Safety
///
/// `init` must be a valid `ExtensionInit` exported by the plugin. The
/// caller must keep `library` mapped for the duration of this call (it
/// does not consume the library — the caller still owns it after this
/// returns).
pub unsafe fn activate_with_transaction(
    world: &mut World,
    slot: usize,
    _library: &libloading::Library,
    init: sys::ExtensionInit,
    proposed_generation: u32,
    identity: CanonicalId,
) -> TransactionalActivationOutcome {
    use crate::host::{snapshot_registrations, diff_registrations, apply_journal_rollback};

    let (counter_clone, prior_loaded_at) = {
        let loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
        let s = &loaded.0[slot];
        (s.generation.clone(), s.loaded_at)
    };

    // ── Snapshot restorable state BEFORE init ────────────────────────────
    //
    // The snapshot must be taken before `init_plugin_gen` because a
    // candidate that overwrites an existing resource (or a component
    // whose layout is byte-compatible) commits the candidate's bytes
    // before we ever get a chance to read the prior. Snapshotting post-
    // init would capture the candidate's value rather than the prior
    // value, and a rollback would "restore" the wrong bytes.
    let before = snapshot_registrations(world);
    let mut prior_resource_bytes: std::collections::HashMap<
        bevy::ecs::component::ComponentId,
        Vec<u8>,
    > = std::collections::HashMap::new();
    if let Some(resources) = world.get_resource::<super::PluginResources>() {
        for cid in &resources.0 {
            if let Some(bytes) = super::read_resource_bytes_safe(world, *cid) {
                prior_resource_bytes.insert(*cid, bytes);
            }
        }
    }
    let mut prior_component_schemas: std::collections::HashMap<
        bevy::ecs::component::ComponentId,
        super::PluginComponentInfo,
    > = std::collections::HashMap::new();
    if let Some(schemas) = world.get_resource::<super::PluginComponentSchemas>() {
        for info in &schemas.0 {
            prior_component_schemas.insert(info.id, info.clone());
        }
    }

    // ── Run init ─────────────────────────────────────────────────────────
    //
    // The candidate registers systems with `at = proposed_generation`,
    // while the slot's counter remains at `loaded_at`. Every system the
    // candidate registers therefore observes `at != counter` and is
    // inert for the duration of the transaction. No schedule runs
    // between now and the commit/rollback decision below — this all
    // happens inside one Bevy system — so the candidate's systems never
    // actually execute.
    //
    // `init_plugin_gen` now returns the host-side `InitOutcome` so a
    // layout-conflict reason from `verify_same_layout` reaches us as
    // `InitOutcome::LayoutConflict(reason)` instead of being collapsed
    // to generic `Failed`. The prior review (P3V-3) found that the
    // post-init `detect_layout_conflict` compared the post-init schema
    // to itself (a no-op) because the incompatible candidate returned
    // before installing a replacement schema, leaving the prior's schema
    // visible to it; that has been removed here.
    let result = super::init_plugin_gen_with_non_persistable(
        world,
        init,
        counter_clone.clone(),
        proposed_generation,
        slot,
        identity,
        false,
    );

    // ── Diff after init ──────────────────────────────────────────────────
    //
    // `diff_registrations` filters by `(slot, proposed_generation)` so the
    // journal contains ONLY the candidate's newly-added entries. Older
    // entries at the same slot but `loaded_at` are not in the journal and
    // are not touched by `apply_journal_rollback`.
    let mut journal = diff_registrations(world, &before, slot, proposed_generation);

    // Annotate ResourceRegistered entries that existed pre-init with the
    // prior bytes so rollback restores them. F3-1: do NOT replace the
    // ResourceRegistered entry — registration cleanup and byte restoration
    // are separate operations. Emit a companion `ResourceInserted` entry
    // alongside the original so rollback restores prior bytes AND removes
    // the candidate's ownership/metadata claim.
    //
    // Candidates that introduced a brand-new resource have no pre-init
    // bytes; their ResourceRegistered journal entry's rollback branch
    // (in `apply_journal_rollback`) drops the candidate's claim AND the
    // public metadata entry. The `ResourceInserted` companion is only
    // emitted when prior bytes existed.
    let mut companions: std::collections::HashMap<
        bevy::ecs::component::ComponentId,
        usize,
    > = std::collections::HashMap::new();
    for entry in journal.entries() {
        if let crate::host::JournalEntry::ResourceRegistered { id, .. } = entry {
            if prior_resource_bytes.contains_key(id) {
                companions.insert(*id, journal.entries().len());
            }
        }
    }
    let mut to_append = Vec::new();
    for id in companions.keys() {
        if let Some(b) = prior_resource_bytes.get(id) {
            to_append.push(crate::host::JournalEntry::ResourceInserted {
                id: *id,
                prior_bytes: Some(b.clone()),
            });
        }
    }
    for entry in to_append {
        journal.entries_mut().push(entry);
    }

    match result {
        super::InitOutcome::Ok => {
            // Commit: bump counter, retire ONLY the prior generation's
            // slot-owned registrations. The candidate's entries (at
            // `proposed_generation`) survive untouched. `_libraries` is
            // appended by the caller via `load_one_transactional`.
            {
                let mut loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
                let s = &mut loaded.0[slot];
                counter_clone.store(proposed_generation, std::sync::atomic::Ordering::Relaxed);
                s.loaded_at = proposed_generation;
                s.images += 1;
            }
            super::retire_slot(world, slot, prior_loaded_at);
            // Refresh schemas for any byte-compatible re-registrations
            // (the candidate may have re-registered an existing
            // component with the same layout). Without this the
            // post-init schema would replace the prior's, and live
            // entity instances that were stored under the prior's
            // layout would render against the candidate's (which
            // matches in size + fields — that's the whole point of
            // `refresh_compatible_schemas`).
            refresh_compatible_schemas(world, &prior_component_schemas);

            let committed = journal.entries().to_vec();
            TransactionalActivationOutcome::Committed {
                slot,
                generation: proposed_generation,
                committed_entries: committed,
            }
        }
        super::InitOutcome::LayoutConflict(reason) => {
            apply_journal_rollback(world, &mut journal, slot, proposed_generation);
            restore_slot_after_rollback(world, slot, prior_loaded_at);
            TransactionalActivationOutcome::RolledBack {
                slot,
                proposed_generation,
                failure: ActivationFailure::LayoutConflict(reason),
                rolled_back_entries: journal.entries().to_vec(),
            }
        }
        super::InitOutcome::Failed => {
            apply_journal_rollback(world, &mut journal, slot, proposed_generation);
            restore_slot_after_rollback(world, slot, prior_loaded_at);
            TransactionalActivationOutcome::RolledBack {
                slot,
                proposed_generation,
                failure: ActivationFailure::InitFailed,
                rolled_back_entries: journal.entries().to_vec(),
            }
        }
        super::InitOutcome::VersionTooOld => {
            apply_journal_rollback(world, &mut journal, slot, proposed_generation);
            restore_slot_after_rollback(world, slot, prior_loaded_at);
            TransactionalActivationOutcome::RolledBack {
                slot,
                proposed_generation,
                failure: ActivationFailure::VersionTooOld,
                rolled_back_entries: journal.entries().to_vec(),
            }
        }
        super::InitOutcome::AbiMismatch => {
            apply_journal_rollback(world, &mut journal, slot, proposed_generation);
            restore_slot_after_rollback(world, slot, prior_loaded_at);
            TransactionalActivationOutcome::RolledBack {
                slot,
                proposed_generation,
                failure: ActivationFailure::AbiMismatch,
                rolled_back_entries: journal.entries().to_vec(),
            }
        }
        super::InitOutcome::UnknownStatus(s) => {
            apply_journal_rollback(world, &mut journal, slot, proposed_generation);
            restore_slot_after_rollback(world, slot, prior_loaded_at);
            TransactionalActivationOutcome::RolledBack {
                slot,
                proposed_generation,
                failure: ActivationFailure::UnknownInitStatus(s as u32),
                rolled_back_entries: journal.entries().to_vec(),
            }
        }
    }
}

/// Restore the slot's counter and `loaded_at` after a rolled-back
/// activation. Failed systems have already been irrevocably cancelled and
/// removed, or queued for removal if their schedule is currently running.
/// Reusing a proposed generation cannot reactivate those failed systems.
fn restore_slot_after_rollback(world: &mut World, slot: usize, prior_loaded_at: u32) {
    if let Some(mut loaded) = world.get_resource_mut::<LoadedPlugins>() {
        if let Some(s) = loaded.0.get_mut(slot) {
            s.generation
                .store(prior_loaded_at, std::sync::atomic::Ordering::Relaxed);
            s.loaded_at = prior_loaded_at;
        }
    }
}

/// Re-apply byte-compatible schema refresh for any component the
/// candidate re-registered. The pre-init schema for every component
/// the candidate touched is in `prior_component_schemas`; the post-init
/// schema is in `PluginComponentSchemas`. We replace the post-init
/// entry with a clone that keeps the pre-init per-field layout
/// information, so downstream consumers (inspector, scene writer,
/// scripting bindings) see the stable layout that all existing entity
/// instances were stored under.
///
/// P3V-4 + the P3V-3 review required the comparison to be done before
/// any rollback. The comparison now happens inside `register_component`
/// / `register_resource` via `verify_same_layout`, which sets
/// `HostCtx::layout_conflict_reason` and causes `init_plugin_gen` to
/// return `InitOutcome::LayoutConflict(reason)`. This function only
/// handles the SUCCESSFUL same-layout case.
fn refresh_compatible_schemas(
    world: &mut World,
    prior_component_schemas: &std::collections::HashMap<
        bevy::ecs::component::ComponentId,
        super::PluginComponentInfo,
    >,
) {
    let Some(mut schemas) = world.get_resource_mut::<super::PluginComponentSchemas>() else {
        return;
    };
    for info in schemas.0.iter_mut() {
        if let Some(prior) = prior_component_schemas.get(&info.id) {
            // Keep the candidate's identity (type_path, display_name) but
            // restore the prior's fields + size so the inspector and
            // other consumers read the layout existing entity instances
            // were stored under.
            info.size = prior.size;
            info.fields = prior.fields.clone();
            info.default_value = prior.default_value.clone();
        }
    }
}

/// Load a freshly-staged plugin library through the transactional
/// activation path. Used by Phase 3 `renzora_loose_plugins` to feed the
/// stable staged library through the same dlopen/symbol/ABI checks as
/// the existing `load_one`, but with the snapshot/diff/commit/rollback
/// transaction layered on top.
///
/// **Every** opened library image is retained for the life of the
/// process: committed ones go to `PluginSlot::_libraries`, and every
/// failed one (open succeeded but symbol/scope/ABI/init/layout refused
/// it) goes to `PluginSlot::failed_libraries`. Dropping a `Library`
/// would free the loaded code and turn every function pointer the
/// plugin already registered into a dangling reference, and
/// `FreeLibrary` itself has deadlocked on this platform before. The
/// invariant is therefore: once `Library::new` returns `Ok`, the image
/// stays mapped. See `PluginSlot::_libraries` for the same rule in the
/// non-transactional path.
pub fn load_one_transactional(
    world: &mut World,
    path: &Path,
    is_editor: bool,
    linked: &[&str],
    disabled: &[String],
    identity: CanonicalId,
) -> Result<TransactionalActivationOutcome, LoadOutcome> {
    if is_proc_macro_dylib(path) {
        return Err(LoadOutcome::NotAPlugin);
    }
    if !exports_plugin_init(path) {
        return Err(LoadOutcome::NotAPlugin);
    }

    let slot = {
        let mut loaded = world.get_resource_or_insert_with(LoadedPlugins::default);
        loaded.slot_for(path)
    };
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let id = stem.strip_prefix("lib").unwrap_or(&stem);
    if disabled.iter().any(|d| d == id) {
        return Err(LoadOutcome::Disabled);
    }
    if linked.contains(&id) {
        return Err(LoadOutcome::Failed(
            "statically linked into this binary".to_string(),
        ));
    }

    let prior_loaded_at = {
        let loaded = world.get_resource::<LoadedPlugins>().unwrap();
        loaded.0[slot].loaded_at
    };
    let proposed_generation = prior_loaded_at.saturating_add(1);

    let image = if is_editor {
        match shadow_copy(path, proposed_generation) {
            Ok(p) => p,
            Err(e) => {
                return Err(LoadOutcome::Failed(format!(
                    "could not stage a copy to load: {e}"
                )))
            }
        }
    } else {
        path.to_path_buf()
    };

    let library = match unsafe { Library::new(&image) } {
        Ok(l) => l,
        Err(e) => return Err(LoadOutcome::Failed(format!("could not open: {e}"))),
    };
    let library = std::mem::ManuallyDrop::new(library);

    let init: Symbol<sys::ExtensionInit> = match unsafe { library.get(sys::INIT_SYMBOL.as_bytes()) } {
        Ok(s) => s,
        Err(_) => {
            retain_failed_library(world, slot, &library);
            return Err(LoadOutcome::NotAPlugin);
        }
    };
    let init = *init;

    let scope = match unsafe { library.get::<sys::ScopeEntry>(sys::SCOPE_SYMBOL.as_bytes()) } {
        Ok(f) => unsafe { f() },
        Err(_) => sys::PluginScope::Runtime,
    };
    if !scope.is_known() {
        retain_failed_library(world, slot, &library);
        return Err(LoadOutcome::Failed(format!(
            "declares scope {} which this build does not have",
            scope.0
        )));
    }
    if scope == sys::PluginScope::Editor && !is_editor {
        retain_failed_library(world, slot, &library);
        return Err(LoadOutcome::WrongScope(scope));
    }
    if let Some(mut loaded) = world.get_resource_mut::<LoadedPlugins>() {
        loaded.0[slot].scope = Some(scope);
    }

    let outcome = unsafe {
        activate_with_transaction(
            world,
            slot,
            &library,
            init,
            proposed_generation,
            identity,
        )
    };

    match &outcome {
        TransactionalActivationOutcome::Committed { slot, .. } => {
            if let Some(mut loaded) = world.get_resource_mut::<LoadedPlugins>() {
                // SAFETY: `library` is `ManuallyDrop::new`'d above; reading
                // its inner pointer back moves the `Library` into the
                // slot's never-freed pool. `_libraries` retains it for
                // the life of the process.
                let raw = unsafe { std::ptr::read(&*library) };
                loaded.0[*slot]._libraries.push(std::mem::ManuallyDrop::new(raw));
            }
        }
        TransactionalActivationOutcome::RolledBack { .. } => {
            // P3V-3: a failed candidate's opened image MUST remain
            // mapped for the process lifetime. Freeing it would call
            // `FreeLibrary`, which has deadlocked on this platform, and
            // any function pointer the candidate already registered
            // (against the prior generation's GenGate) would point at
            // freed memory. We move the `Library` into the slot's
            // `failed_libraries` pool, where it sits in `ManuallyDrop`
            // for the rest of the process.
            retain_failed_library(world, slot, &library);
        }
    }

    let _ = id;
    match &outcome {
        TransactionalActivationOutcome::Committed { .. } => Ok(outcome),
        TransactionalActivationOutcome::RolledBack { failure, .. } => Err(match failure {
            ActivationFailure::InitFailed => {
                LoadOutcome::Failed("plugin init returned Failed".to_string())
            }
            ActivationFailure::VersionTooOld => LoadOutcome::VersionTooOld,
            ActivationFailure::AbiMismatch => LoadOutcome::Failed(
                "plugin was built against a differently-shaped interface table — \
                 its version matches but a field was inserted, reordered or retyped. \
                 Rebuild the plugin against this engine's `renzora_plugin`"
                    .to_string(),
            ),
            ActivationFailure::UnknownInitStatus(s) => LoadOutcome::Failed(format!(
                "plugin init returned status {s} which this engine does not know — \
                 it was built against a newer ABI. Rebuild it against this engine's \
                 `renzora_plugin`"
            )),
            ActivationFailure::LayoutConflict(why) => {
                LoadOutcome::Failed(format!("layout conflict: {why}"))
            }
            ActivationFailure::OpenFailed(s) => LoadOutcome::Failed(s.clone()),
        }),
    }
}

/// Append a `ManuallyDrop`-wrapped copy of `library` to the slot's
/// `failed_libraries` pool. See `PluginSlot::_libraries` for the
/// never-dropped safety invariant.
fn retain_failed_library(
    world: &mut World,
    slot: usize,
    library: &std::mem::ManuallyDrop<libloading::Library>,
) {
    if let Some(mut loaded) = world.get_resource_mut::<LoadedPlugins>() {
        let raw = unsafe { std::ptr::read(&**library) };
        loaded.0[slot]
            .failed_libraries
            .push(std::mem::ManuallyDrop::new(raw));
    }
}

/// Runs immediately before [`First`], and exists purely so a reload never mutates
/// a schedule that is mid-run.
///
/// A plugin's systems go into the five main-loop schedules, and `Schedules` hands
/// a schedule *out* while it runs — so a reload triggered from inside `Update`
/// would add the new build's systems to a fresh, empty `Update` that is discarded
/// the moment the real one is put back. The systems would vanish with no error.
/// Registration at `build` time avoided this by happening before any schedule ran;
/// this schedule is the same trick for a running app.
#[derive(bevy::ecs::schedule::ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PluginReload;

/// Plugin paths to reload at the next frame boundary.
///
/// A queue rather than an immediate call because the caller is usually a file
/// watcher or a UI button, neither of which holds `&mut World` at a safe moment.
#[derive(Resource, Default)]
pub struct PluginReloadQueue(pub Vec<PathBuf>);

/// Ask for `path` to be reloaded before the next frame.
///
/// Duplicates collapse: an editor save often produces several filesystem events
/// for one write, and rebuilding the same plugin three times in a frame would be
/// three sets of dead systems for no reason.
pub fn request_reload(world: &mut World, path: impl Into<PathBuf>) {
    let path = path.into();
    let mut queue = world.get_resource_or_insert_with(PluginReloadQueue::default);
    if !queue.0.contains(&path) {
        queue.0.push(path);
    }
}

/// What the initial load was configured with, kept so a reload applies the same
/// filters rather than a looser set.
#[derive(Resource)]
pub struct PluginHostConfig {
    pub is_editor: bool,
    /// Plugin ids the user turned off.
    ///
    /// Kept here for the *reload* path specifically. Without it, saving a
    /// disabled plugin's source would rebuild it, the watcher would notice the
    /// changed file, and it would load — turning "disabled" into "disabled until
    /// you touch it", which is the worst of both.
    pub disabled: Vec<String>,
}

fn apply_reload_requests(world: &mut World) {
    let pending = match world.get_resource_mut::<PluginReloadQueue>() {
        Some(mut q) if !q.0.is_empty() => std::mem::take(&mut q.0),
        _ => return,
    };
    let (is_editor, disabled) = world
        .get_resource::<PluginHostConfig>()
        .map(|c| (c.is_editor, c.disabled.clone()))
        .unwrap_or((false, Vec::new()));

    for path in pending {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        // A disabled plugin stays disabled when its file changes. Checked before
        // `load_one` for the same reason the initial scan does it: declining
        // after opening means either dropping a mapped image or leaking one,
        // and this path is reached repeatedly while an author iterates.
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let id = stem.strip_prefix("lib").unwrap_or(&stem);
        if disabled.iter().any(|d| d == id) {
            debug!("[plugin] ignoring rebuilt {name} — disabled in Settings → Editor → Plugins");
            continue;
        }
        // Look the identity up from the slot stamped at initial
        // load. The initial `load_dir` derives the canonical
        // identity from the discovery-root-relative logical
        // filename (`libfoo.so` and `foo.dll` at the same logical
        // location produce the same `engine://foo`), then writes
        // it on the slot; the reload reuses the SAME identity, so
        // a Linux rebuild that lands as `libfoo.so` and a Windows
        // rebuild that lands as `foo.dll` would still share the
        // slot's `ComponentId`s. Falling back to deriving here
        // would silently change identity and re-allocate every
        // durable schema — which is the bug this branch fixes.
        let stored_identity = world
            .get_resource::<LoadedPlugins>()
            .and_then(|l| {
                l.0.iter()
                    .find(|s| s.path.as_path() == path.as_path())
                    .and_then(|s| s.directory_identity.clone())
            });
        let identity = match stored_identity {
            Some(id) => id,
            None => {
                // The slot has no stored identity: either this is
                // a plugin dropped in mid-session (the watcher
                // path that runs in the editor), or it is a
                // legacy `load_one` call. Derive a fresh one
                // using the legacy basename fallback — the slot
                // gets stamped next load_dir pass.
                warn!(
                    "[plugin] reload of {name} has no stored identity; \
                     deriving a fresh one (initial load bypassed load_dir?)"
                );
                canonical_id_for_legacy_path(&path)
            }
        };
        match load_one(world, &path, identity, is_editor) {
            LoadOutcome::Loaded => {
                let generation = world
                    .get_resource::<LoadedPlugins>()
                    .and_then(|l| l.0.iter().find(|s| s.path == path))
                    .map(|s| s.loaded_at)
                    .unwrap_or(0);
                if generation == 0 {
                    info!("[plugin] loaded {name} (added while running)");
                } else {
                    info!("[plugin] reloaded {name} (generation {generation})");
                }
            }
            // Every failure leaves the previous build running — the generation
            // counter only moves on success — so these are warnings, not errors
            // that need the app to do anything about them.
            LoadOutcome::Failed(why) => {
                warn!("[plugin] reload of {name} failed, keeping the running build: {why}")
            }
            LoadOutcome::VersionTooOld => {
                warn!("[plugin] reload of {name} needs a newer ABI; keeping the running build")
            }
            LoadOutcome::WrongScope(scope) => {
                warn!("[plugin] reload of {name} declares {scope:?} scope, which this binary is not")
            }
            LoadOutcome::NotAPlugin => {
                warn!("[plugin] reload of {name}: no `{}` export", sys::INIT_SYMBOL)
            }
            // Unreachable: `load_one` never returns this, only `load_dir` does,
            // and the hot-reload watcher goes straight to `load_one`. Matched
            // rather than caught by a wildcard so a future outcome is a compile
            // error here instead of a silently ignored case.
            LoadOutcome::Disabled => {}
        }
    }
}

/// Notices a rebuilt plugin and queues it for reload.
///
/// Polls `mtime` + `size` rather than subscribing to filesystem events. Not for
/// lack of a watcher crate — `notify` is already in the tree behind `bevy_asset` —
/// but because polling answers the question that actually matters more directly.
/// A build writes a DLL in pieces, and loading a half-written one is the failure
/// mode to avoid; "the stamp has not changed since the last poll" is a settle test,
/// whereas an event stream needs debouncing to become one. It also keeps a crate
/// that publishes to crates.io from gaining a dependency to stat eight files.
#[derive(Resource)]
pub struct PluginWatcher {
    /// Directory the watcher polls. `pub` so test harnesses can
    /// override the `RenzoraPluginHostPlugin`-chosen default.
    pub dir: PathBuf,
    /// Last-seen `(mtime, size)` per file. `pub` for tests.
    pub seen: std::collections::HashMap<PathBuf, (std::time::SystemTime, u64)>,
    /// Files whose stamp moved on the previous poll, waiting to stop moving.
    /// `pub` for tests.
    pub settling: std::collections::HashSet<PathBuf>,
    /// Seconds until the next poll. `pub` for tests; production sets
    /// it to `POLL_INTERVAL` so the next `poll_plugin_dir` tick fires.
    pub countdown: f32,
}

impl PluginWatcher {
    /// Test-only constructor that builds a watcher pointing at `dir`
    /// with an initial poll cycle primed so the next `Last` schedule
    /// run will stat the directory. `countdown` is initialised to
    /// `0.0` so the watcher fires immediately rather than waiting
    /// `POLL_INTERVAL` seconds after install.
    pub fn for_tests(dir: PathBuf) -> Self {
        let seen = stamp_dir(&dir);
        Self {
            dir,
            seen,
            settling: Default::default(),
            countdown: 0.0,
        }
    }
}

/// How often to stat the plugin directory. Two polls are needed to settle a file,
/// so this is half the reload latency.
const POLL_INTERVAL: f32 = 0.25;

/// `(mtime, size)` for every plugin-shaped file in `dir`.
fn stamp_dir(dir: &Path) -> std::collections::HashMap<PathBuf, (std::time::SystemTime, u64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let ext = std::env::consts::DLL_EXTENSION;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                out.insert(path, (mtime, meta.len()));
            }
        }
    }
    out
}

fn poll_plugin_dir(
    time: Res<Time>,
    mut watcher: ResMut<PluginWatcher>,
    mut queue: ResMut<PluginReloadQueue>,
) {
    watcher.countdown -= time.delta_secs();
    if watcher.countdown > 0.0 {
        return;
    }
    watcher.countdown = POLL_INTERVAL;

    let Ok(entries) = std::fs::read_dir(&watcher.dir) else {
        return;
    };
    let ext = std::env::consts::DLL_EXTENSION;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let stamp = (mtime, meta.len());

        // Copied out rather than matched on in place: the arms below mutate
        // `watcher`, and holding a borrow of `seen` across them does not compile.
        let previous = watcher.seen.get(&path).copied();
        match previous {
            // A file that was not present at boot. `seen` is seeded from the
            // startup scan, so this can only be a plugin dropped in mid-session —
            // pick it up. Recording it and doing nothing (which is what this arm
            // used to do) meant a newly added plugin sat there until the next
            // restart, and "I put a dll in plugins/, why is nothing happening" is
            // the one question that behaviour guarantees.
            //
            // Still goes through the settle check: a file being copied in is
            // exactly as half-written as one being rebuilt.
            None => {
                watcher.seen.insert(path.clone(), stamp);
                watcher.settling.insert(path);
            }
            Some(prev) if prev != stamp => {
                watcher.seen.insert(path.clone(), stamp);
                watcher.settling.insert(path);
            }
            // Unchanged. If it moved last poll, the write has finished.
            Some(_) => {
                if watcher.settling.remove(&path) && !queue.0.contains(&path) {
                    info!(
                        "[plugin] {} changed on disk, reloading",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    queue.0.push(path);
                }
            }
        }
    }
}

/// Host components a plugin is allowed to resolve by type path.
///
/// This exists because Bevy registers components **lazily** — adding
/// `TransformPlugin` registers the *type* for reflection but does not allocate a
/// `ComponentId` until something actually spawns or queries one. A plugin
/// loading at startup would therefore ask for a perfectly real component and get
/// nothing back, which is indistinguishable from a typo.
///
/// Registering eagerly is cheap (it allocates an id and nothing else) and has a
/// useful side effect: this list IS the public component surface. A host type
/// not named here is not reachable from a plugin, which is a decision worth
/// making deliberately rather than by accident of what happened to be spawned
/// before load.
fn register_exposed_components(world: &mut World) {
    world.register_component::<Transform>();
    world.register_component::<GlobalTransform>();
    world.register_component::<Visibility>();
    world.register_component::<Name>();
    world.register_component::<Mesh3d>();
}

/// Loads C-ABI plugins from `<exe-dir>/plugins/` during app build.
///
/// Runs at `build` time rather than in a startup system because plugins insert
/// systems into schedules, and doing that before the first frame avoids any
/// question about mutating a schedule that is mid-run.
pub struct RenzoraPluginHostPlugin {
    /// Whether this binary is the editor. Editor-scope plugins load only when
    /// it is; runtime-scope plugins load either way.
    pub is_editor: bool,
    /// Plugins compiled into this binary, initialised before the ones on disk.
    ///
    /// Empty for every build except a lean export that chose to link its plugins
    /// in — see [`crate::static_link`]. The two paths coexist deliberately: a
    /// game can ship some plugins inside the binary and still read a `plugins/`
    /// folder for anything a player or a mod drops in.
    pub statics: Vec<StaticPlugin>,
    /// Plugin ids the user has turned off — a library stem with any `lib`
    /// prefix stripped, matching what Settings → Editor → Plugins persists.
    ///
    /// Passed **in** rather than read here, because the list lives in the
    /// contract crate's editor preferences and this crate deliberately does not
    /// depend on it: it is published to crates.io so a third-party plugin author
    /// can `cargo add renzora_plugin`, and a path dependency would make that
    /// impossible. The two binaries that construct this plugin do the reading.
    pub disabled: Vec<String>,
}

impl Plugin for RenzoraPluginHostPlugin {
    fn build(&self, app: &mut App) {
        super::install_system_maintenance(app);
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("plugins")))
            .unwrap_or_else(|| PathBuf::from("plugins"));

        register_exposed_components(app.world_mut());
        // Nothing is mapped yet, so last session's shadow copies are still
        // deletable. After this they are not. Editor-only: only the editor
        // makes these, and a game runtime launched from the editor's own
        // directory must not delete images that editor is running on.
        if self.is_editor {
            clear_shadow_dir(&dir);
        }

        // Reload machinery, before the initial load so a plugin that somehow
        // requests a reload during its own init is queued rather than lost.
        // Input is snapshotted in `PreUpdate`, before any plugin system in `First`
        // could read a stale one, and unconditionally — a system declaring
        // `Res<PluginInput>` must find it even on a headless server, where the
        // snapshot stays zeroed and every key reads as up.
        app.init_resource::<super::input::PluginInput>()
            .add_systems(PreUpdate, super::input::collect_input);

        // Service calls are parked for whichever engine crate claims them. The
        // sweep runs at the very end of the frame so a build missing a bridge —
        // a dedicated server, a lean 2D export — clears those calls rather than
        // growing the queue every frame a plugin makes one.
        app.init_resource::<super::PluginServiceCalls>()
            .add_systems(Last, super::discard_unhandled_service_calls);

        app.insert_resource(PluginHostConfig {
            is_editor: self.is_editor,
            disabled: self.disabled.clone(),
        })
            .init_resource::<PluginReloadQueue>()
            .init_schedule(PluginReload)
            .add_systems(PluginReload, apply_reload_requests);
        app.world_mut()
            .resource_mut::<bevy::app::MainScheduleOrder>()
            .insert_before(First, PluginReload);

        // Watching is editor-only. A shipped game has no reason to restat its
        // plugin directory forever, and swapping code under a player is not a
        // feature — it is how a save file gets corrupted by a half-written build.
        if self.is_editor {
            app.insert_resource(PluginWatcher {
                dir: dir.clone(),
                // Seeded with what is on disk right now, which is what lets the
                // poll treat an unseen path as "added since boot" and load it. An
                // empty map would make every plugin look new a quarter-second
                // after startup and reload the lot.
                seen: stamp_dir(&dir),
                settling: Default::default(),
                countdown: POLL_INTERVAL,
            })
            .add_systems(Last, poll_plugin_dir);

            // The other half of the loop: watch plugin SOURCE, rebuild it, and drop
            // the artifact here — where the watcher above then picks it up. Only
            // this ordering works, so the two are installed together.
            super::dev::install(app, dir.clone());

            // Phase 3 loose-plugin host is installed by the editor / runtime
            // binary directly (see `crates/renzora_editor_app/src/main.rs` and
            // `src/main.rs`). Installing it here would create a cyclic
            // dependency: this crate depends on `renzora_loose_plugins`, which
            // depends on this crate's `host` feature. The install order
            // (RenzoraPluginHostPlugin first, then LoosePluginHost) is
            // documented at both call sites.
        }

        // Linked-in plugins first, and their names then suppress any loose copy
        // of the same plugin in `plugins/` — see [`load_dir`] for why loading
        // both is considerably worse than loading either.
        for plugin in &self.statics {
            let id = plugin.id;
            // A linked plugin's canonical identity is supplied by
            // the aggregator. The aggregator's own per-plugin
            // registration is the durable source here.
            let id_str = format!("linked://{id}");
            let identity = CanonicalId::parse(&id_str).unwrap_or_else(|_| {
                CanonicalId::from_rooted(renzora_identity::RootKind::Engine, &id_str)
                    .expect("linked plugin id is well-formed")
            });
            match load_static(app.world_mut(), plugin, identity, self.is_editor) {
                LoadOutcome::Loaded => info!("[plugin] linked {id}"),
                LoadOutcome::WrongScope(scope) => {
                    debug!("[plugin] skipping linked {id} — {scope:?} scope")
                }
                LoadOutcome::VersionTooOld => warn!(
                    "[plugin] linked {id} needs a newer renzora_plugin ABI than this build \
                     (host is {}.{})",
                    sys::VERSION_MAJOR,
                    sys::VERSION_MINOR
                ),
                LoadOutcome::Failed(why) => error!("[plugin] linked {id} failed: {why}"),
                // Neither can happen: there is no file to fail the symbol sniff,
                // and a plugin compiled into the binary is not something the
                // disable list can reach — it is in the executable either way.
                LoadOutcome::NotAPlugin | LoadOutcome::Disabled => {}
            }
        }

        let linked: Vec<&str> = self.statics.iter().map(|p| p.id).collect();
        for (path, outcome) in
            load_dir(app.world_mut(), &dir, &dir, self.is_editor, &linked, &self.disabled)
        {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match outcome {
                LoadOutcome::Loaded => info!("[plugin] loaded {name}"),
                LoadOutcome::NotAPlugin => {}
                // `load_dir` already said so, at the point it decided.
                LoadOutcome::Disabled => {}
                LoadOutcome::VersionTooOld => warn!(
                    "[plugin] {name} needs a newer renzora_plugin ABI than this build \
                     (host is {}.{})",
                    sys::VERSION_MAJOR,
                    sys::VERSION_MINOR
                ),
                // Debug, not warn: a game staged alongside the editor sees every
                // editor plugin in its `plugins/` directory, and saying so at
                // warn level once per plugin per launch is noise about something
                // working correctly.
                LoadOutcome::WrongScope(scope) => {
                    debug!("[plugin] skipping {name} — {scope:?} scope")
                }
                LoadOutcome::Failed(why) => error!("[plugin] {name} failed: {why}"),
            }
        }
    }
}

/// One C-ABI plugin found on disk, without loading it into a `World`.
///
/// The export UI needs this: it lists what a game *could* ship so the user can
/// tick plugins on and off, which happens long before (and independently of)
/// anything being installed into a running app.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// File stem — what the export UI shows and what a selection is keyed on.
    pub id: String,
    pub path: PathBuf,
    pub scope: sys::PluginScope,
}

/// Enumerate the C-ABI plugins in `dir` by probing their exported symbols.
///
/// Replaces `dynamic_plugin_loader::scan_plugins`, which probed for the old
/// Bevy-cdylib symbols (`plugin_create` / `plugin_scope`). Those no longer exist
/// — and note the old scanner read `plugin_scope` while C-ABI plugins export
/// `renzora_plugin_scope`, so every plugin silently came back as Runtime and
/// Editor-scope ones were offered as shippable. Reading the right symbol is the
/// fix.
///
/// A library that does not export `renzora_plugin_init` is simply not a plugin
/// and is skipped, so unrelated DLLs sitting in the folder are ignored.
///
/// **Answered without mapping anything, wherever possible.** This used to
/// `Library::new` every file in the folder and let the handle drop at the end of
/// the iteration, which froze the editor the moment the export dialog opened:
/// `FreeLibrary` runs an image's static destructors under the Windows loader
/// lock, and `tracy.dll` starts a profiler thread at map time, so the unload
/// waited on a thread that needed the lock the unload was holding. It is the same
/// deadlock [`load_one`]'s `ManuallyDrop` exists to prevent — see the comment
/// there.
///
/// Mapping was doubly wrong here, not merely fatal on the way out. The editor
/// runs each plugin from a shadow copy under a different filename (see
/// [`shadow_copy`]), so the OS treats `plugins/tracy.dll` as an unrelated library
/// and maps a **second** live instance of it, initialisers and all — a second
/// Tracy client, from a dialog that only wanted to list filenames.
///
/// So: [`exports_plugin_init`] settles "is this a plugin?" from the file's bytes,
/// and the scope comes from [`LoadedPlugins`], which recorded it when the plugin
/// was loaded for real. Only a plugin this process never loaded — one added to
/// the folder since boot, or one whose load failed before the scope was read —
/// falls through to [`probe_scope`].
pub fn scan_plugins(world: &World, dir: &Path) -> Vec<PluginInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let known = world.get_resource::<LoadedPlugins>();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(std::env::consts::DLL_EXTENSION)
            || is_proc_macro_dylib(&path)
            || !exports_plugin_init(&path)
        {
            continue;
        }
        let Some(scope) = known
            .and_then(|k| k.scope_of(&path))
            .or_else(|| probe_scope(&path))
        else {
            continue;
        };
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(PluginInfo { id, path, scope });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Read a plugin's scope by mapping it, for the case where nothing else knows.
///
/// The image is **never unmapped** — `ManuallyDrop`, for the reason
/// [`load_one`] spells out at length. That makes this the expensive answer, and
/// it is why [`scan_plugins`] asks [`LoadedPlugins`] first: it costs one
/// permanently mapped image per plugin this process has not otherwise loaded.
///
/// `None` means the file would not open at all, which [`scan_plugins`] treats as
/// "not something we can offer to ship".
fn probe_scope(path: &Path) -> Option<sys::PluginScope> {
    let library = std::mem::ManuallyDrop::new(unsafe { Library::new(path) }.ok()?);
    Some(
        match unsafe { library.get::<sys::ScopeEntry>(sys::SCOPE_SYMBOL.as_bytes()) } {
            Ok(f) => unsafe { f() },
            // No declaration means Runtime, matching `renzora::add!`'s default.
            Err(_) => sys::PluginScope::Runtime,
        },
    )
}
