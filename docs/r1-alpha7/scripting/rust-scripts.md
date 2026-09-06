# Rust Scripts

A `.rs` file anywhere under your project is compiled on the machine that opens it and run once per frame for each entity carrying it — through the same `ScriptEngine` Lua uses, via the versioned Tier 1 C-ABI in `renzora_plugin::script`. A Rust script does **not** link Bevy.

```rust
// <project>/scripts/spin.rs
use renzora_plugin::script::*;

fn update(ctx: &Ctx, _reply: &mut ScriptReply) -> Result<(), String> {
    let _ = ctx.entity();
    Ok(())
}

renzora_plugin::rust_script!(update);
```

Attach it exactly like a Lua script: drop it into the entity's **Scripts** component. Routing is by file extension, so `.rs`, `.lua` and `.blueprint` scripts coexist on the same entity.

## What you get

The same hook vocabulary Lua uses, plus the small Tier 1 `Ctx` API on top of it. Update-only scripts — the common case — declare a single `update` function and the dispatcher invokes it for `OnUpdate`; every other hook returns `NoHook` so the engine treats the script as a no-op rather than logging a missing hook every frame. A script that wants `OnReady`, `OnRpc`, etc. extends the dispatcher in `renzora_plugin::script::compiled` — keeping the contract identical to the other languages.

The full hook vocabulary:

| Hook | Runs |
|---|---|
| `on_ready` | once, when the entity's scripts start |
| `on_update` | every frame; the most common hook |
| `on_rpc(name, from, args)` | an RPC targeted at this script |
| `on_ui(name, entity, args)` | a UI event targeted at this script |
| `on_draw(w, h)` | the script's draw callback |
| `on_animation_event(name, entity)` | an animation event |
| `on_http(callback, status, body)` | an HTTP response |
| `on_player_event(id, joined)` | a player joined or left |
| `on_scene_event(path, error)` | a scene finished loading, or failed |
| `on_event(name, args)` | a broadcast game event |

The `Ctx` exposes the entity the script is attached to, the frame
context (time, input, named entities, gamepads, action axes), and a
small set of host calls for reading other components and entities.
Mutations happen through `ScriptReply`: the hook returns `Result<(), String>`
and the reply carries commands the host applies after the hook returns —
the same model Lua uses.

## The `Ctx` API

| | |
|---|---|
| `ctx.frame.time.delta` | seconds since last frame |
| `ctx.frame.time.elapsed` | seconds since startup |
| `ctx.frame.input_movement` / `mouse_position` / `mouse_delta` | last frame's input |
| `ctx.frame.keys_pressed` / `keys_just_pressed` / `keys_just_released` | sparse key tables |
| `ctx.frame.gamepads` | connected gamepads and their state |
| `ctx.frame.action_*` / `action_axis_*` | input action bindings |
| `ctx.frame.named_entities` | `&str -> Entity` table |
| `ctx.entity.entity_id` / `name` / `position` / `rotation` / `scale` | this entity |
| `ctx.entity.parent_entity` / `children` | the hierarchy around it |
| `ctx.entity.collisions_entered` / `collisions_exited` / `active_collisions` | collision events for this entity |
| `ctx.entity.health` / `max_health` / `is_invincible` | gameplay state |
| `ctx.entity.light_intensity` / `light_color` / `material_color` | visuals |
| `ctx.host.get(entity, component, field)` | read one reflected field |
| `ctx.host.get_component(entity, component)` | read every field of a component |
| `ctx.host.get_components(entity)` | list the components on an entity |
| `ctx.host.asset_progress()` | asset-loading progress for a loading-screen script |
| `ctx.host.scene_load_state()` | the currently-loading scene |
| `ctx.host.translate(key)` | localize a key |

## Returning commands

A hook fills a `ScriptReply`:

```rust
fn update(ctx: &Ctx, reply: &mut ScriptReply) -> Result<(), String> {
    // Move the entity by its input axis.
    let dx = ctx.frame.input_movement[0] * 5.0 * ctx.frame.time.delta;
    let dy = ctx.frame.input_movement[1] * 5.0 * ctx.frame.time.delta;
    reply.commands.push(ScriptCommand::Translate { dx, dy, dz: 0.0 });
    Ok(())
}
```

A reply can hold commands, prop writes (`vars: Vec<(String, ScriptValue)>`),
draw commands (`draws: Vec<DrawCmd>`), and an optional text payload
(`text: Option<String>` for the REPL). The host applies commands after
the hook returns; var writes are folded back into the script's
`ScriptVariables`.

## When it runs

Exactly when a Lua script does: in play mode, in Simulate, or when that
script's **play button** in the inspector is on. Nothing runs while you
are arranging the scene in edit mode — a script that spawns or
despawns would otherwise start doing so the moment you dropped it on
an entity.

## Recompiling

Saving a script rebuilds it through the shared `renzora_compiler_cache::BuildService`,
the same one loose plugins use. The compile runs off the main thread; only the
descriptor load and pointer swap happen on the main thread. Compile errors,
panics and a missing SDK all appear in the **Console** panel — with diagnostics
pointing at the user's source path, not at the staged copy.

A script that fails to compile is not retried until you edit it again, so one
error does not become a scrolling wall. Multiple file events received before
submission are combined into one build. When a newer edit is submitted while
an older build is running, the older request is marked superseded and only the
newest successful version can become active.

The watcher reconciles a debouncer batch against the canonical identity
index: a remove-then-create on the same path is a rename; a bare remove is a
deletion; a bare create is a new compile. The watcher can attach after the
editor has started. Switching projects retires the previous project's scripts,
moves the watcher to the new project, and cancels old in-flight builds without
waiting for them. Closing the project detaches its watcher.

## The compiled-script boundary

A `.rs` script compiles to a small `cdylib`. The host first asks the library for
the descriptor size, validates it, allocates host-owned storage, and then asks
the library to copy its descriptor into that storage. This prevents the host
from reading a foreign structure before its size is known. The copied
C-compatible descriptor has this shape:

```text
#[repr(C)]
pub struct CompiledScriptDesc {
    pub abi_version: u32,                   // == COMPILED_SCRIPT_ABI (currently 1)
    pub descriptor_size: u32,               // mem::size_of::<CompiledScriptDesc>()
    pub required_capabilities: u32,         // bitmask of capability bits the script needs
    pub _pad: u32,                          // reserved
    pub call_prefix_hashes: *const u64,    // append-stable hash chain over ScriptCall's fields
    pub call_prefix_count: usize,
    pub host_prefix_hashes: *const u64,    // append-stable hash chain over ScriptHostCalls's fields
    pub host_prefix_count: usize,
    pub entry: ScriptEntry,                 // unsafe extern "C" fn(*const ScriptCall) -> ScriptStatus
}
```

The loader validates all of `abi_version`, `descriptor_size`, the prefix-hash
chains, and the capability mask through [`CompiledScriptDesc::check_compat`](../../api/scripting.md)
before publishing the descriptor. A malformed or version-skewed
artifact is refused with a clear diagnostic; the previous good
generation stays active.

The descriptor's `entry` is the per-cdylib trampoline — also
`unsafe extern "C" fn(*const ScriptCall) -> ScriptStatus` — generated
inline by the `rust_script!` macro inside the cdylib itself. The
trampoline decodes the call, statically invokes the author's typed
`update(ctx, reply) -> Result<(), String>` by name (compiled into
the same cdylib), and writes the reply through the call's `out`
sink. Calling the typed function does **not** cross the C ABI,
**does not** carry a `Result`, `String`, `Vec`, or trait object
across the dynamic-library boundary, and **does not** require the
host to know about Rust-ABI types. The compiler never has to be
the same `rustc` as the host's.

The author-facing macro emits the descriptor, the trampoline, and
the typed entry. A `pub fn __tier1_entry` is also emitted so the
lean exporter's aggregator can collect the typed entry without
linking Bevy or the editor.

## Requirements

- The shared compiler cache — see [Build Cache](../editor-dev/build-cache.md).

A Rust script is a small `cdylib` that depends only on `renzora_plugin`. The
crate compiles in a second or two on a warm cache and in tens of seconds on
a cold one. There is no separate plugin SDK to install; the cache shares
its compiler with every loose plugin.

The `BuildService` is shared between loose plugins and Rust scripts: the
editor constructs one `Arc<BuildService>` and hands it to both consumers.
The cache key differs (`cdylib-plugin` vs `cdylib-script`), so a plugin
edit does not invalidate a script cache entry.

Build artifacts land under the shared cache root
(`<project>/.loose-cache/` or whichever root the editor points the
`BuildService` at). They are derived — nothing there needs to be looked at
or committed.

## In an exported game

Scripts run in exports. How they get there depends on the packaging mode,
and neither route asks anything of the player — no SDK, no Rust toolchain.

| Packaging | How the script gets in | Compiled by |
|---|---|---|
| Copy-based | shipped as a library beside the game, with a `scripts.index` manifest | the editor, at export |
| Lean | compiled into the executable via a generated static-script table | the export build |

**Copy-based** exports carry the latest validated `cdylib` and a
canonical-id-keyed manifest. A script that has never compiled is reported
rather than silently omitted; a failed build never ships.

**Lean** exports link nothing extra — each project's scripts are emitted
by the lean exporter's generated `renzora_static_scripts` aggregator
crate, which collects one `pub fn __tier1_entry` per script module and
publishes them as a `Vec<(CanonicalId, TypedScriptFn)>` table. The
dispatcher trampoline (`renzora_plugin::script::dispatcher_trampoline`)
is the same in the editor and in an export. A script behaves the same
in the editor and in an export, or an export cannot be tested by
playing it.

Scripts may live anywhere in the project, not only in `scripts/`. Where two
folders hold the same file name, each is reachable by its full
project-relative path. A bare file-name lookup (no separators) is
permitted as a compatibility alias **only when it uniquely resolves**;
when two scripts share the same leaf name, the editor surfaces the
ambiguity at lookup time and the user must specify the full path.

## Limits

- **No `&mut World`.** A compiled Rust script targets the small Tier 1
  C-ABI; unrestricted Bevy access belongs to the restart-required
  engine-plugin tier (see [Native Plugins](../extending/native-plugins.md)).
- **No props-from-source attributes.** A script's tunables are ordinary
  components on the entity, which the inspector already edits.
- **No REPL.** Evaluating a Rust expression would mean invoking the
  compiler per expression.
- **One file per script.** A script is a single `.rs`; a plugin is the
  answer when you need modules.

## The legacy `renzora::script!` form

A source file that still uses `renzora::script!(update)` with a
`fn update(_: &mut renzora::ScriptCtx) {}` shape is detected by
`crates/renzora_identity::Recogniser` as `Declaration::LegacyRecognised`.
The discovery code surfaces a clear migration diagnostic — the file
is **never** compiled through the unsafe old ABI:

```
[rust-script] <path> still uses `renzora::script!`; the Phase 4 Tier 1
form is `renzora_plugin::rust_script!`. Compile-time Bevy access moved
to the restart-required engine-plugin tier.
```

Migrate the script in two steps: replace the macro name and the
function signature:

```rust
// Before (legacy, no longer loaded)
use renzora::ScriptCtx;
fn update(_ctx: &mut ScriptCtx) {}
renzora::script!(update);

// After (Phase 4 Tier 1)
use renzora_plugin::script::*;
fn update(ctx: &Ctx, _reply: &mut ScriptReply) -> Result<(), String> { Ok(()) }
renzora_plugin::rust_script!(update);
```

The macro emits the compiled-script descriptor and its `extern "C"`
entry automatically. A `pub fn __tier1_entry` is also emitted so the
lean exporter's aggregator can collect the script without linking
Bevy or the editor.

## Rust scripts and loose plugins

A loose `<plugin-root>/<name>.rs` plugin and an editor Rust script are
both Tier 1 `cdylib`s against `renzora_plugin`, but they target different
entry points. A loose plugin registers components, systems, render
passes, and bindings through `renzora::add!`; a Rust script registers
a compiled-script descriptor through `renzora_plugin::rust_script!`.
Both go through the shared `BuildService`; the cache key differs
(`cdylib-plugin` vs `cdylib-script`) so a plugin edit does not invalidate
a script cache entry.

## New-file templates

New Rust scripts created from the hierarchy use the current small SDK in both
boilerplate settings. The minimal template has an empty update function; the
illustrated template rotates its attached entity around the vertical axis.
Neither template requires Bevy imports or the retired `renzora::script!` macro.
