# Building Plugins

Almost every feature in Renzora is its own Bevy plugin; this page shows how to write one and register it with a single macro.

## The plugin model

A Renzora plugin is just a Bevy `Plugin` (anything that implements `bevy::app::Plugin`). You declare it once with `renzora::add!(...)` and the engine wires it in automatically — there is no central list of plugins to edit and no `app.add_plugins(...)` call to make by hand.

> **There are now three kinds of plugin.** This page covers the two that existed first — the workspace plugin compiled into the binary, and the standalone C-ABI plugin. The third, a **[native plugin](native-plugins.md)**, is an ordinary Bevy plugin shipped as *source* and compiled on the machine that installs it: it gets full `&mut World`, can add [editor panels](panels.md), and needs no engine source edits. That is the one to reach for when extending the **editor**. A C-ABI plugin is still the only kind that ships inside a **game**.

There are exactly two kinds of plugin, and the difference is purely how the crate is compiled and linked:

| Kind | Crate type | Linked | Registers via | Ships in |
|------|-----------|--------|---------------|----------|
| **Workspace plugin** | `rlib` (default) | statically, into the `renzora` binary | inventory constructor at process start | the binary itself |
| **Distribution plugin** | `cdylib` (own `dlopen` feature) | dynamically, `dlopen`'d at runtime | `extern "C"` FFI exports | a `.dll`/`.so`/`.dylib` dropped into `<exe>/plugins/` |

> Recap of the engine shape: there is **one** binary (`renzora`) and it is always runtime-shaped. The editor is itself a removable `renzora_editor` cdylib bundle that sits beside the executable. Your plugins slot into the same model — either compiled into the binary, or loaded beside it.

## A minimal plugin

Both plugin kinds share identical Rust. The plugin type must implement `Default` (the macro constructs it with `Default::default()`):

```rust
use bevy::prelude::*;

#[derive(Default)]
pub struct MyPlugin;

impl Plugin for MyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MyState>()
            .add_systems(Update, my_system);
    }
}

#[derive(Resource, Default)]
pub struct MyState {
    pub counter: u32,
}

fn my_system(mut state: ResMut<MyState>) {
    state.counter += 1;
}

// Register with the engine. Runtime scope by default.
renzora::add!(MyPlugin);
```

> There is **no `renzora::prelude`**. Import the SDK surface with `use renzora::*;`, or pull individual items (`use renzora::Inspectable;`). For ECS types (`Plugin`, `App`, `Query`, `Commands`, …) use Bevy's own `use bevy::prelude::*;`.

## The `add!` macro

`renzora::add!` (defined in `crates/renzora/src/plugin_meta.rs`) is the one registration point. Its forms:

```rust
renzora::add!(MyPlugin);                          // Runtime scope (default)
renzora::add!(MyEditorTool, Editor);              // Editor scope
renzora::add!(MyGameplay, Runtime);               // Runtime scope, stated
renzora::add!(MyFoundation, Runtime, priority = -100); // with explicit order
```

The macro expands to **two parallel registration paths**, and the build target decides which one is live:

1. **Inventory (always, every platform).** It emits an `inventory::submit!{ StaticPlugin { name, scope, priority, install } }`. At startup the host iterates the global registry with `for_each_static_plugin(host_scope, …)` and installs every matching plugin in priority order. This is the path for statically-linked workspace plugins on desktop, iOS, Android, and wasm.

2. **FFI exports (desktop only, `dlopen` feature only).** Under `#[cfg(all(feature = "dlopen", not(any(target_os = "ios", target_os = "android", target_arch = "wasm32"))))]` it emits three `#[no_mangle] extern "C"` functions the dynamic loader reads:

   ```rust
   #[no_mangle] pub extern "C" fn plugin_create() -> *mut dyn Plugin;
   #[no_mangle] pub extern "C" fn plugin_scope()  -> u8;       // PluginScope discriminant
   #[no_mangle] pub extern "C" fn plugin_bevy_hash() -> [u64; 2]; // ABI guard
   ```

   The `cfg(feature = "dlopen")` gate resolves against the **calling crate**, not against `renzora`. That is why a distribution plugin declares its own `dlopen` feature rather than enabling one on the `renzora` dependency.

Because those FFI symbols are unmangled, a distribution cdylib may contain **exactly one** `add!` — two would collide on `plugin_create`. Workspace plugins never enable `dlopen`, so the symbols stay off and multiple `add!`s link cleanly into the host.

## Scopes

`PluginScope` has exactly two variants and matching is **exact equality** — there is no "both" scope:

```rust
pub enum PluginScope {
    Editor  = 0,
    Runtime = 1,
}
```

| Scope | Loads in the editor session | Loads in the shipped game / server | Use for |
|-------|:---:|:---:|---------|
| `Runtime` (default) | yes | yes | gameplay, rendering, UI, audio, networking — anything that runs in the actual game |
| `Editor` | yes | no | panels, inspectors, gizmos, import tools — editor-only tooling |

`Runtime` plugins load in the runtime pass, which the editor host runs too, so they appear in the editor viewport **and** the exported game. `Editor` plugins load only when the editor bundle is present.

> There is no `EditorAndRuntime` scope. A feature that needs editor tooling *on top of* runtime behaviour ships **two** plugins — one of each scope (the convention in the engine is e.g. `GameUiPlugin` + `GameUiEditorPlugin`).

### Priority

`priority` is an `i32` order hint (default `0`, lower installs earlier). Reach for it only when a plugin must initialise a resource another plugin reads at install time. For ordinary system ordering, prefer Bevy's own `.before()`/`.after()`/`.chain()` and system sets instead.

## Workspace plugins (static)

A workspace plugin is the default: a plain `rlib` crate under `crates/`, statically linked into the binary. Minimal `Cargo.toml`:

```toml
[package]
name = "renzora_myplugin"
version = "0.1.0"
edition = "2021"

[dependencies]
bevy = { workspace = true }
renzora = { path = "../renzora", default-features = false }
```

To get linked into the binary, add the crate as a dependency of `renzora_runtime` (`renzora_myplugin = { path = "../renzora_myplugin" }`). Its `inventory` constructor then self-registers at process start — you do **not** call `add_plugins` anywhere. Build via the renzora CLI (which runs the build inside the pinned Docker toolchain):

```bash
renzora build    # binary + editor bundle (--workspace)
renzora build    # lean game binary only (--bin renzora)
```

> An `Editor`-scope workspace plugin is added to `renzora_runtime` as an **optional** dependency under its `editor` feature, so it is excluded from the lean runtime build.

## Distribution plugins (dynamic)

A distribution plugin ships as a standalone `cdylib` that the engine `dlopen`s from `<exe>/plugins/` — no rebuild of the engine required. The crate declares its **own** `dlopen` feature (default-on) so `add!` emits the FFI exports:

```toml
[package]
name = "renzora_myplugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[features]
default = ["dlopen"]
dlopen = []

[dependencies]
bevy = { workspace = true }
renzora = { path = "../renzora", default-features = false }
```

The Rust is identical to a workspace plugin (one `Default` plugin type, one `renzora::add!`). Build it via the renzora CLI (the build runs inside the Docker toolchain) and copy the artifact into the engine's `plugins/` directory:

```bash
renzora build
# -> target/dist/renzora_myplugin.{dll,so,dylib}
# copy that file into  <renzora-binary>/plugins/
```

At startup `dynamic_plugin_loader` scans `<exe>/plugins/`, verifies the ABI guard, reads `plugin_scope` to decide whether to load (Editor → only in an editor session; Runtime → always), then calls `plugin_create` and runs the plugin's `build(app)`. The loaded `Library` is kept alive in a `DynamicPluginRegistry` for the life of the process.

`renzora_hot_demo` (`crates/renzora_hot_demo`) is a complete, working example of a distribution plugin that spawns and animates entities to prove a `dlopen`'d cdylib gets the same full `&mut App` / ECS access as a built-in plugin.

## Scaffolding with `renzora add`

`renzora add` generates a plugin skeleton (`crates/renzora_<name>/` with `Cargo.toml` + `src/lib.rs`):

```bash
renzora add <name>            # static workspace plugin
renzora add <name> --editor   # Editor-scope, optional dep under [features].editor
renzora add <name> --dylib    # distribution cdylib (default = ["dlopen"])
```

| Flag | Crate type | Scope | Wiring it does |
|------|-----------|-------|----------------|
| *(none)* | `rlib` | Runtime | adds a non-optional dep to `renzora_runtime` |
| `--editor` | `rlib` | Editor | adds an optional dep under `renzora_runtime`'s `editor` feature |
| `--dylib` | `cdylib` (`dlopen`) | Runtime | none — auto-included by the `crates/*` glob, loaded at runtime |

`--editor` and `--dylib` are mutually exclusive.

> ⚠️ The script's **no-flag** default currently writes `renzora::add!(<Name>Plugin, EditorAndRuntime);` into the skeleton. `EditorAndRuntime` is **not** a real `PluginScope` variant, so the generated crate will not compile as-is — change that line to `renzora::add!(<Name>Plugin);` (Runtime) or `renzora::add!(<Name>Plugin, Editor);` after scaffolding. The `--editor` and `--dylib` paths emit valid scopes.

## The ABI guard

A `dlopen`'d plugin and the host share the **same compiled `bevy_dylib` and `renzora` dylib** (via `prefer-dynamic` + `bevy/dynamic_linking`), so their `TypeId`s line up across the boundary. `plugin_bevy_hash()` enforces this at load time:

```rust
#[no_mangle]
pub extern "C" fn plugin_bevy_hash() -> [u64; 2] {
    let id = std::any::TypeId::of::<bevy::ecs::world::World>();
    unsafe { std::mem::transmute(id) }
}
```

The loader compares this against its own value and **rejects any plugin whose hash does not match** — a mismatch means the plugin was built against a different Bevy/engine and would corrupt ECS access. In practice this means: build distribution plugins with the **same engine version and toolchain** as the host (the `docker/base/Dockerfile` image, pinned to one Rust version, is the canonical build environment). The build also stamps a `RENZORA_BUILD_HASH` (version + rustc + Bevy) used for the same compatibility checks.

## Hot-loading

`HotPluginPlugin` watches `<exe>/plugins/` (~1s interval, on the `Last` schedule) and builds newly dropped dlls into the **live** `World`, so a main-world plugin activates on the next frame without a restart. Plugins that touch the render world (post-process effects, custom render-graph nodes) can't be spliced into an already-initialised renderer; they load as far as the main world allows and report `NeedsReload` so the editor can prompt for a restart.

## The editor bundle

The editor ships as a single bundle cdylib (`renzora_editor`) that exports `plugin_install_scope` instead of the `plugin_create` trio. It is produced by `renzora::export_plugin_bundle!(foundation = [...])`, which installs an ordered foundation and then replays every `Editor`-scope plugin from the one global inventory:

```rust
renzora::export_plugin_bundle!(foundation = [
    renzora_asset_registry::AssetRegistryPlugin,
    renzora_editor_framework::RenzoraEditorPlugin,
    renzora_keybindings::KeybindingsPlugin,
]);
```

You rarely write this yourself — it is how the editor itself is assembled. The dynamic loader deliberately **skips** any cdylib exporting `plugin_install_scope` when scanning `plugins/`, so a bundle only ever loads beside the exe and the editor is never accidentally shipped inside a game. Normal community plugins use `add!`.

## What a plugin can do

Inside `build(&self, app)` you have the full `&mut App` surface — exactly what a built-in plugin has. Common additions:

### Components and scene serialization

Derive the reflection traits and register the type so it survives scene save/load (Renzora serializes scenes to RON):

```rust
use bevy::prelude::*;
use serde::{Serialize, Deserialize};

#[derive(Component, Reflect, Serialize, Deserialize, Default)]
#[reflect(Component, Serialize, Deserialize)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Plugin for MyPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Health>();
    }
}
```

### Inspector UI

Custom inspectors are **not** egui (egui has been removed from the engine entirely — there is no `EditorPanel` trait and no `register_panel`). They use the `renzora` editor contract, which is gated behind the crate's `editor` feature (default features are empty):

```toml
renzora = { path = "../renzora", default-features = false, features = ["editor"] }
```

```rust
#[derive(Component, Reflect, Default, renzora::Inspectable)]
#[reflect(Component)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}
```

### Viewport tools

`App::register_tool(ToolEntry)` adds a button to the viewport. A `ToolEntry` is an icon, a tooltip, and three closures: `visible_if` (show it at all), `active_if` (draw it highlighted) and `on_activate` (what clicking does). The **section** decides which surface it renders on:

| Section | Where it renders |
|---|---|
| `ToolSection::Transform` | the horizontal strip across the viewport's top edge, with Select / Move / Rotate / Scale |
| `ToolSection::Terrain` | the same strip, after a divider |
| `ToolSection::Custom(id)` | the same strip, after the built-in sections; `id` groups and sorts |
| `ToolSection::Shelf(group)` | the **two-column vertical shelf** down the viewport's left edge |

Everything else about an entry is identical either way, so moving a tool between surfaces is a one-word change.

> **The two surfaces split by depth, not by feature.** A tool that *opens* other tools stays on the strip — the gizmo modes, the terrain modes (Sculpt / Paint / Foliage), mesh Edit Mode — so there is always one visible row saying what the viewport is set to do. What each of those reveals goes on the shelf: the 17 terrain sculpt brushes, the paint brushes, the foliage types, and in Edit mode the two draw tools, the select modes and the ops. A tool that opens nothing and has no palette under it is better on the shelf with its neighbours than alone on the strip — that's why Generate Terrain and Resize Terrain sit with the terrain size controls rather than in the terrain mode row.
>
> **Keep shelf groups even.** The shelf is two buttons wide and every group starts on a fresh row, so an odd group ends on a row with a hole in it that reads as a missing button — and a group of one reads as a mistake. Where a group won't come out even, move a member to the neighbouring group where it also makes sense, or pair it with the control it belongs next to.

```rust
app.register_tool(
    ToolEntry::new("mytool.brush.smooth", "waves", "Smooth", ToolSection::Shelf("mytool.brushes"))
        .order(3)
        .visible_if(|w| /* only while my tool is active */ true)
        .active_if(|w| /* is this the chosen brush? */ false)
        .on_activate(|w| { /* choose it */ }),
);
```

Use the **strip** for the mode that turns your tool on, and the **shelf** for what that mode opens. The strip runs out of room past a few buttons and wraps into a second row, taking Play and the view controls down with it; the shelf grows downward where nothing competes for the space.

Shelf groups render top to bottom in **alphabetical order of the group string**, separated by a rule, and the whole shelf collapses when none of its entries are visible. That sort is *global*, across every crate that registers a group — so if your feature has several groups that must stay in a fixed order, encode it in the id. The terrain toolset does exactly this: `terrain.a-region` → `terrain.b-sculpt` → `terrain.c-paint` → `terrain.d-foliage-brush` → `terrain.e-foliage-types`, the last two registered by a different crate (`renzora_foliage_editor`) but part of the same palette, and therefore carrying the same `terrain.` prefix. The modeling groups do the same across two crates: `modeling.a-draw` comes from `renzora_mesh_draw`, `modeling.b-select` onward from `renzora_mesh_edit`.

### Viewport toolbar groups

A tool's *settings* — as opposed to the tool button itself — can be mounted as a group in the toolbar with `renzora_ember::toolbar::register_viewport_tool_group(key, builder)`. The `key` is a stable identifier: the group is draggable like every other group on the bar, and its position is saved under that key, so changing it resets users' toolbars.

```rust
renzora_ember::toolbar::register_viewport_tool_group("mytool-settings", |commands, fonts| {
    let group = commands.spawn(/* … */).id();
    // Hide the group when it isn't relevant — an always-visible group holds
    // its width in every other context for nothing.
    bind_display(commands, group, |w| /* my tool is active */ true);
    group
});
```

This exists because `renzora_viewport` can't depend on the crates that want to mount things in it. Two narrower registries sit beside it in the same module: `register_viewport_tool_trailing` (widgets pinned to the strip's right-hand end) and `register_viewport_top_strip` (full-width bars under the strip).

See **Script API Bindings** for exposing functions to scripts, **[Post-Processing Effects](./post-processing.md)** for camera effects (which are [standalone plugins](./standalone-plugins.md) now, not distribution plugins), and **Custom Blueprint Nodes** / **Custom Material Nodes** for those subsystems — each has its own registration path layered on the same `add!` model described here.

## Loose single-file plugins (Phase 3)

A loose plugin is a single `.rs` file placed at `<plugin-root>/<name>.rs`
— the root of the same `plugins/` directory the directory-based C-ABI
plugins already use. The author writes the same source as a
directory-based plugin (using `renzora_plugin`'s ergonomic API) and
declares an explicit scope; the editor compiles, stages and reloads
the result through the Phase 2 cached compiler service and a
transactional activation gate.

**Authoring contract.** A loose plugin source file contains exactly
one `renzora_plugin::add!(PluginType, Scope)` declaration, with
`Scope` set to `Runtime` or `Editor`. Unscoped, conflicting, or
`Both`-scope declarations are rejected by the contract parser and
recorded as `WrongScope` / `MalformedContract` in the inventory — no
BuildService submission is made for a malformed file. The author
writes:

```rust
use renzora_plugin::prelude::*;

#[derive(Component)]
struct Spinner { speed: f32 }

fn spin(mut q: Query<&mut Spinner>, _time: Res<Time>) {
    for mut s in &mut q { s.speed += 1.0; }
}

struct SpinnerPlugin;
impl Plugin for SpinnerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, spin);
    }
}

renzora_plugin::add!(SpinnerPlugin, Runtime);
```

The generated wrapper depends only on the small `renzora_plugin` SDK
— no Bevy, no `renzora`, no editor crate — and emits a cdylib that
exports `renzora_plugin_init` and `renzora_plugin_scope`.

**Discovery and trust.** The `LoosePluginHost` Bevy plugin watches the
plugin root with `notify-debouncer-full` (300 ms debounce,
non-recursive). On a `.rs` change, the host reads the source, parses
the contract, and updates the authoritative
`LoosePluginInventory`. A loose plugin whose source was placed in the
root before the user has granted trust consent is recorded as
`AwaitingTrustConsent` and **no BuildService submission is made**
until consent is recorded in `LoosePluginTrust`. A disabled loose
plugin is recorded as `Disabled` and likewise not submitted.

**Compilation.** Once consented and enabled, the source is submitted
to the shared `renzora_compiler_cache::BuildService` as a
`BuildRequest { artifact_kind: Tier1Plugin, source_snapshot, … }`.
The BuildService runs `cargo build` against a wrapper crate generated
under the cache root, partitions the artifact with the existing
Phase 2 fingerprint inputs, and reports `Published` or `CacheHit` on
the per-revision receiver. Older receivers for the same identity
receive `Superseded`.

**Staging.** A successful publish is staged by `StableStaging` at a
collision-proof flat path under `<plugins-dir>/.loose-staged/`. The
atomic replace uses `rename(2)` on POSIX and `MoveFileExW` with
`MOVEFILE_REPLACE_EXISTING` on Windows, so a compile failure never
touches the previously staged file.

**Transactional activation.** The staged path feeds
`renzora_plugin::host::loader::load_one_transactional`, which
performs:

- image open, init-symbol lookup, scope-symbol lookup;
- explicit scope validation against `is_editor`;
- ABI version + `INTERFACE_PREFIX_HASHES` negotiation;
- component/resource layout compatibility check via
  `verify_same_layout`, called inside `register_component` /
  `register_resource` for any name that already exists. A mismatch
  in size, field count, per-field offset, or per-field kind
  discriminant captures a concrete reason on `HostCtx::
  layout_conflict_reason` and causes `init_plugin_gen` to return
  `InitOutcome::LayoutConflict(reason)`. The transactional loader
  surfaces this as `ActivationFailure::LayoutConflict(reason)` and
  the loose inventory transitions to
  `LayoutChangeRequiresRestart`;
- plugin `build` initialization through `init_plugin_gen` with the
  candidate's `at = proposed_generation`, so candidate systems are
  inert (`GenGate::stale`) until commit;
- snapshot of the prior resource bytes via
  `host::read_resource_bytes_safe` BEFORE init, so a candidate
  that overwrites a resource can be rolled back to the exact prior
  bytes;
- journal of every non-system registry mutation, filtered to the
  candidate's `(slot, proposed_generation)` so the prior
  generation's entries are never in the journal; snapshot/diff via
  `RegistrySnapshot` / `diff_registrations`;
- on success: bump the slot's counter, retire ONLY the prior
  generation's slot-owned registrations via
  `retire_slot(world, slot, prior_loaded_at)`, refresh byte-
  compatible schemas via `refresh_compatible_schemas`, commit the
  candidate registrations, then atomically publish the new
  generation counter;
- on failure: `apply_journal_rollback(world, &mut journal, slot,
  proposed_generation)` undoes every entry in reverse (including
  resource-byte restoration via
  `host::write_resource_bytes_unsafe`), the slot's `loaded_at` and
  counter are restored to their prior values, and the candidate's
  systems remain permanently inert (`at != counter`).

**Same-ID ownership.** `PluginComponentOwners` maps every
`ComponentId` to a `Vec<(slot, generation)>` of ownership claims,
not a single `(slot, generation)` tuple. A same-identity reload
reuses the stable `ComponentId`, so the prior-generation claim and
the candidate-generation claim coexist on the same id during the
transaction. `retire_slot` removes the prior's claims on commit;
`apply_journal_rollback` removes only the candidate's claims on
rollback. Bevy `ComponentId`s remain allocated across reloads; the
existing entity storage, the schema entry, and the live value
stay intact — a same-slot reload sees the same component, with the
candidate's fresh schema having replaced the prior's for byte-
compatible fields.

**Stable asset rollback.** Mesh / material / image entries
created during a candidate's init are journaled by stable
`Handle::id().index()` rather than vector index. Rollback
finds the candidate's row by `(owner, generation, handle_id)` and
removes only that row, leaving the post-rollback vector order
identical to the pre-init snapshot. Index-based rollback was
rejected because removing whatever currently occupies a recorded
index would shift every later entry and break the rollback
invariant that the post-rollback registries match the pre-init
registries exactly.

**Never-unload invariant.** Every `Library` opened by
`Library::new` is wrapped in `ManuallyDrop` and pushed onto one of
two pools in the `PluginSlot`: `_libraries` for committed loads
and `failed_libraries` for rolled-back loads (open succeeded but
symbol / scope / ABI / init / layout refused). Neither pool is
ever drained; dropping a `Library` calls `FreeLibrary`, which has
deadlocked on this platform, and any function pointer the
candidate registered would point at freed memory.

**Last-good behavior.** On every failure mode — open fail, missing
symbol, scope reject, ABI mismatch, prefix mismatch, layout change,
init fail — the slot's prior generation stays active. The candidate's
systems are registered with `at = proposed` but counter remains at
`loaded_at`, so `GenGate::stale()` returns true forever. A failed
candidate never runs and never disturbs the running build. The
stable staged file is unchanged.

**Durable host identity.** Every plugin component and resource the
host registers is recorded under a durable, scene-persisted name
that is stable across recompilation, editor restart, and
editor/runtime builds:

```
renzora.plugin/v1/<blake3_64hex>/<local>
```

- `v1` is the format version; bumping it is the breaking-change
  path. A reader that does not recognise the prefix refuses the
  scene rather than guessing.
- `<blake3_64hex>` is the full-strength (256-bit) BLAKE3 of the
  canonical identity's `scheme://path` form. Two distinct canonical
  identities have a cryptographic collision likelihood.
- `<local>` is whatever the plugin supplied — the literal
  descriptor name in a hand-written `ComponentDesc` (the C ABI),
  or the `module_path!() + "::" + type_name` path produced by
  `#[derive(Component)]`.

The host constructs the durable name at the registration
boundary. The plugin author never has to know or write it; both
hand-written descriptors and derive-generated components go
through the same construction. A single central helper
`renzora_plugin::host::durable_type_path(identity, local)` is the
only place this string is built. `PluginComponents` and
`PluginComponentSchemas` store the durable name; Bevy's
`ComponentDescriptor.name` and the scene's
`RawTypeTable::by_path` mirror it, so a save and reload in a
different session resolves back to a `ComponentId` in that
session's registry.

Loose plugins carry their own `CanonicalId` through the
activation queue. Directory-loaded C-ABI plugins derive a stable
identity from the discovery-root-relative logical filename:
the parent directories are preserved verbatim and the final
filename is normalised cross-platform so the same plugin
produces the same identity on Linux (`libfoo.so`), macOS
(`libfoo.dylib`) and Windows (`foo.dll`). The single entry
point is
`renzora_plugin::host::loader::canonical_id_for_path(path, discovery_root)`,
which returns `engine://<parent_dirs>/<logical_name>` — for
example, `engine://effects/weather` for
`<root>/effects/libweather.so` (Linux/macOS) or
`<root>/effects/weather.dll` (Windows).

The cross-platform `lib`-strip rule is extension-aware: for
`.so` and `.dylib` exactly one leading `lib` is stripped (the
Unix load-prefix convention); for `.dll` no prefix is stripped
(Windows does not add one). The single helper
`renzora_plugin::host::loader::logical_plugin_name_for_filename`
is the only place the rule lives, and it returns `Err` rather
than guessing on an unsupported extension, an empty stem, or
a `lib.so` whose strip would yield an empty name. Examples:

| on-disk filename       | logical name           |
| ---                    | ---                    |
| `foo.dll`              | `foo`                  |
| `libfoo.so`            | `foo`                  |
| `libfoo.dylib`         | `foo`                  |
| `libfoo.dll`           | `libfoo`               |
| `liblibfoo.so`         | `libfoo`               |
| `liblibfoo.dylib`      | `libfoo`               |
| `library.dll`          | `library`              |
| `effects/libfoo.dll`   | `effects/libfoo`       |
| `effects/liblibfoo.so` | `effects/libfoo`       |

The watcher reuses the initial identity verbatim via
`PluginSlot::directory_identity` rather than re-deriving from
the file stem, so a Linux rebuild that lands as `libfoo.so` and
a Windows rebuild that lands as `foo.dll` share the same slot
and reuse the same durable `ComponentId`s. Both feed the same
`HostCtx.identity`, so the durable-name format is consistent
across plugin kinds.

**Settings, inventory, export.** The host mirrors every loose-plugin
row into `renzora::PluginInventory` with `PluginKind::LooseTier1` so
the Settings panel lists loose plugins alongside directory and C-ABI
plugins. The inventory key is the full canonical identity (e.g.
`engine://spin.rs`), not the bare leaf — two plugins with the same
leaf remain distinct. Disable state agrees between `DisabledPlugins`
and the loose inventory; the Settings toggle's `plugin_toggle_click`
system writes through the canonical id to
`LoosePluginInventory::set_enabled` so the host's watcher and
reload paths do not submit a build for a disabled plugin. The
Settings panel also exposes a Grant / Revoke trust button and a
Reload button on every loose-plugin card; the trust button mutates
`LoosePluginTrust`, `LoosePluginInventory`, and enqueues a reload
request on grant, while the reload button enqueues one directly.
`LoosePluginReloadRequests` is drained by the loose host's
`process_reload_requests` in `PreUpdate`. Active Runtime loose
plugins are picked up by `renzora_export::build::stage_loose_plugins_from`,
which copies each selected plugin's staged cdylib into the export
tree at `<output>/plugins/<safe-name>/build/` — the safe-name
encoder replaces `:` and `/` in the canonical id with `_`, so
`engine://spin.rs` lands at `<output>/plugins/engine__spin_rs/build/`.
Editor-scoped loose plugins are excluded from runtime exports.

**Acceptance evidence.** The `crates/renzora_loose_plugins/tests/acceptance.rs`
suite builds real cdylibs with `rustc --crate-type=cdylib --extern
renzora_plugin=…`, dlopens them through
`loader::load_one_transactional`, and observes every registry
mutation the candidate makes. Same-canonical-identity v1 → v2
reload (acceptance_3) commits at generation 1 and again at
generation 2 from the SAME staged path; the test asserts the
`Behavior` `ComponentId` is reused across reloads — Bevy
allocates ids permanently and the prior's claim is removed by
`retire_slot` while the candidate's claim is preserved. The
resource-bytes round-trip test (acceptance_14) uses
`read_resource_bytes_safe` and `write_resource_bytes_unsafe` — the
exact helpers `activate_with_transaction` calls — to snapshot a
sentinel, overwrite it, then restore it through the same host
path. Layout-conflict detection (acceptance_6) is exercised by a
real 4-byte → 16-byte `LayoutProbe` re-registration at the SAME
staged path (both builds share the crate name `lp_probe` so the
derive macro emits the same TYPE_PATH); the activation is refused
as `LoadOutcome::Failed("…layout conflict…")`. A→B→C rapid edits
(acceptance_8) submit three real `BuildRequest`s through
`BuildService::submit` (with distinct fingerprint inputs so each
is a distinct build) and assert A and B observe
`BuildOutcome::Superseded` while C observes a non-Superseded
outcome. Same-slot loader/init failure (acceptance_5) overwrites
the staged bytes with garbage and observes the prior generation
stays committed; restoring good bytes yields a generation 2
commit. Multiple-asset rollback (acceptance_15) populates
`PluginAssets` with distinct Bevy `Assets<Mesh>` /
`Assets<Image>` handles and asserts the handle-id-based rollback
restores the exact pre-init state. The export-side test in
`crates/renzora_export/tests/loose_plugin_export.rs` walks the
export tree and asserts no directory name contains `:` or `/`,
and `stage_loose_plugins_from` produces two distinct safe-name
directories for two canonical ids sharing a leaf.

See [native-plugins.md](native-plugins.md) for the directory-based
flow this complements.
