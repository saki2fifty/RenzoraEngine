# Profiling with Tracy

Renzora ships a **Tracy profiler bridge** (`plugins/tracy`) — a standalone C-ABI
plugin that streams live engine telemetry to a running
[Tracy](https://github.com/wolfpld/tracy) profiler over its native protocol:

- a **frame mark** per app frame, and
- every Bevy diagnostic as a named Tracy plot — frame time, FPS, entity count,
  per-render-pass GPU/CPU span times, and system CPU/memory where the platform
  supports it.

> **This plugin gives you plots, never a flame graph.** Per-system CPU zones and
> GPU-pass zones are `#[cfg(feature = "trace")]` inside `bevy_ecs` and
> `#[cfg(feature = "tracing-tracy")]` inside `bevy_render` — instrumentation that
> was compiled out does not exist to be switched on, so no plugin loaded at run
> time can produce it. With only this bridge running, Tracy's **Flame Graph and
> zone Statistics windows stay empty** and `tracy-csvexport -e`/`-g` return
> nothing; all the signal is in the plot rows. For the flame graph, use the
> **profiling build** below, which compiles the instrumentation in.

## The profiling build — full flame graph + per-system Statistics

The bridge's *plots* tell you *how much* (a pass cost 2 ms, FPS is 52) but not
*what ran when*. For the per-system timeline across all ~200 plugins, and the
GPU-pass zones, build with the `profiling` feature:

```
cargo renzora profile        # native: build + stage + launch, trace_tracy compiled in
```

This is just `cargo renzora run` with `--features profiling`, which turns on:

- **`bevy/trace`** — the per-system and render-node CPU spans. Bevy gates system
  zones behind this feature, so *without it the 200-plugin breakdown is invisible*
  even with Tracy connected.
- **`bevy/trace_tracy`** — installs Tracy's tracing layer (CPU zones + one frame
  mark per frame) and the GPU-timestamp zones in `bevy_render`.
- **`renzora_runtime/profiling`** — adds `RenderDiagnosticsPlugin`, which is what
  actually allocates the Tracy GPU context so the GPU-pass zones record. (GPU
  zones work on Dx12/Vulkan; macOS/Metal is excluded upstream.)

Start a Tracy **server first**, then `cargo renzora profile` — the on-demand
client only buffers once a profiler connects, so launch order doesn't lose data.

> **Leave the in-app "Tracy Profiler" toggle OFF in a profiling build.** Bevy
> already emits one frame mark per frame; the `plugins/tracy` bridge marking too
> would double-count every frame and halve the reported frame time.

**`cargo renzora` and `cargo renzora profile` both launch with `RENZORA_NO_XR=1`.**
Editing in a headset is `cargo renzora xr`.

This used to apply to the profiling lane only, which produced a memorable
symptom: the *instrumented* build ran faster than the normal one.

If an OpenXR runtime is installed and set as the system default — not connected,
not in use, merely present — the editor otherwise takes the XR-capable boot,
which disables `PipelinedRenderingPlugin`. The render sub-app then runs inline on
the main thread instead of in parallel with the sim. That showed up as
`sub app{name=RenderApp}` nested under `update` and costing ~11.6 ms of a 27 ms
frame: a serialization that swamps whatever you were actually trying to measure,
and which every ordinary editor session was silently paying.

Add `--xr` when the headset path *is* the subject:

```
cargo renzora profile --xr    # profile the XR-capable (non-pipelined) boot
```

A `RENZORA_NO_XR` already set in your environment always wins; the flag is
consumed by the xtask and never forwarded to the binary.

> **It moves the plugin ABI.** Compiling `trace_tracy` recompiles `bevy_dylib`
> (CLAUDE.md §3), so prebuilt community plugins in `plugins/` won't load against a
> profiling binary. Everything built from source in the same invocation — the
> editor bundle and every workspace plugin — still matches, so the editor and its
> built-in features are unaffected. It's a disposable build; don't ship it.

## Reading a capture

- **Frame time graph** (top ruler): each frame is one mark. A tall frame is a
  hitch; click it to zoom the timeline to that frame.
- **Flame graph** (the main timeline): nested CPU zones per frame — the call/scope
  tree of which system ran when, and for how long. A wide bar = an expensive
  system. There's a separate **GPU** track below the CPU threads with the render
  passes (`main_opaque_pass_3d`, shadow passes, `ssao`, `atmosphere_luts`,
  `bloom`, …).
- **Statistics window** (top bar → *Statistics*): every zone aggregated, sortable
  by total or self time. **This is the "which of my systems is the bottleneck"
  view** — sort by total time and the worst offenders rise to the top. Works for
  GPU zones too (switch the source), giving a per-pass GPU cost ranking.
- **Find Zone** (top bar → *Find Zone*): a per-zone histogram + call-site — use it
  once Statistics has named a suspect, to see its distribution and where it's
  invoked.
- **`prepare_windows` eating most of a frame on the CPU side is the tell for
  GPU-bound** — the CPU is blocked waiting on the GPU to present. In that case the
  answer is in the GPU track / GPU Statistics, not the CPU systems.

### Exporting for offline analysis

With the profiling build, zones exist, so the Tracy **CLI** tools can dump them to
CSV (grab the tools from the *same* Tracy release as your GUI):

```
tracy-capture -o out.tracy -s 8 -f          # capture 8s from the running editor
tracy-csvexport -e out.tracy > cpu.csv      # CPU zones: name,…,total_ns,self,counts,mean…
tracy-csvexport -g out.tracy > gpu.csv      # GPU zones: name,…,GPU execution time
```

Aggregate `cpu.csv` by `name` for total self-time per system, and `gpu.csv` for
ms/frame per pass (divide summed GPU time by frame count).

## Enabling it

One switch: **Settings → Plugins → Tracy Profiler → Enable Tracy**. It takes
effect immediately — no restart. The opt-in persists in
`~/.config/renzora/tracy.json` (`%APPDATA%\renzora\tracy.json` on Windows).

> Earlier versions also required Dev Mode and a restart. Both were consequences
> of the bridge living inside the editor binary: it had to decide at startup
> whether to install its diagnostic sources, so the switch could only be read
> once. As a plugin it installs nothing — it reads measurements the host already
> publishes — so enabling is just "start feeding".

**Turning it off** stops the feeding immediately: no plots, no frame marks. It
does **not** close the Tracy listener socket, because `tracy-client` has no
shutdown outside its `manual-lifetime` feature. That costs an idle socket and
nothing more — the client is built with `ondemand`, so it buffers no trace data
until a profiler actually connects. To get the socket back too, restart.

**To remove Tracy entirely**, delete `plugins/tracy.dll` from beside the
executable. There is no profiler code in the engine itself.

### Choosing what to plot

Under the master switch is a **Plots** list — one toggle per group, applied on
the next frame:

| Group | Covers | Default |
|---|---|---|
| Frame | `fps`, `frame_time`, `frame_count` | on |
| Entity count | `entity_count` | on |
| CPU & memory | `system/*`, `process/*` | on |
| Render passes — GPU time | `render/*/elapsed_gpu` | on |
| Render passes — CPU time | `render/*/elapsed_cpu` | on |
| Shader & pipeline counters | `*_invocations`, `*_primitives_out` | **off** |
| Other diagnostics | anything else, incl. `ui/*` reactivity | on |

Grouped rather than one toggle per plot because the render paths are per-pass and
open-ended — a heavier scene has more of them, and a checklist that grows as you
load a level is not a settings panel.

Invocation counts default off because they are the one group that is usually
noise: raw counters in the millions, which Tracy autoscales, crowding the
millisecond timings that a frame-budget question actually turns on. Turn them on
when the question is "how much geometry is this pass actually touching".

*Other diagnostics* is the catch-all and defaults on deliberately. The host's
diagnostic set is open — any engine crate or plugin can register a path — and
silently dropping unrecognised ones would look exactly like the engine having
stopped measuring.

#### Reading the `ui/*` rows

The reactive UI publishes seven diagnostics, and together they answer "why did
the editor get slower when I selected something":

Eleven `ui/*` diagnostics stream, and between them they answer "why did the
editor get slower when I selected something".

**The bevy_ui pipeline** — where the cost usually is:

| Path | Meaning |
|---|---|
| `ui/content_ms` | `Prepare` → `Content`: propagation + text measurement |
| `ui/layout_ms` | `Content` → `Layout`: taffy solving the tree |
| `ui/nodes_total` | Live `Node` count |
| `ui/text_nodes` | Live `Node` + `Text` count — what measurement scales with |

**The reactive layer** — where it usually *isn't*:

| Path | Meaning |
|---|---|
| `ui/bindings_total` | Bindings walked this frame (excludes parked) |
| `ui/bindings_parked` | Bindings behind a collapsed section — free |
| `ui/bindings_skipped` | Skipped by the dependency gate without running |
| `ui/bindings_changed` | Produced a new value, i.e. a UI write happened |
| `ui/reactions_us` | Binding recompute time, µs |
| `ui/lists_us` | Keyed-list snapshot + diff time, µs |
| `ui/rows_rebuilt` | List rows built or rebuilt |

**Check the reactive rows first, to rule them out.** Opening inspector sections
looks exactly like a reactivity problem and has twice been diagnosed as one; both
times it was not. The tell is `bindings_changed` sitting near zero while the
frame time climbs — nothing is recomputing, the rows simply exist. Note the units
differ deliberately: the reactive figures are **µs** and the pipeline ones are
**ms**, because that is the ratio between them.

Then read `content_ms` against `layout_ms`, because the two point to opposite
fixes. **Content-bound** means text measurement dominates — fewer or cheaper
labels per row. **Taffy-bound** means the tree does — fewer nodes per row.
`nodes_total` is the number both scale with, and watching it jump on selection
tells you what a row really costs: an inspector that adds ~1,000 nodes for ~120
bindings is paying for the nodes, not the bindings.

#### What "off" does to a row already on screen

Turning a group off stops it feeding on the next frame. **A row already on
Tracy's timeline stays there, showing a frozen line**, and that is a limit of
Tracy rather than a shortcut here: its protocol carries `PlotDataInt`,
`PlotDataFloat`, `PlotDataDouble`, `PlotConfig` and `PlotName`, and nothing that
removes a plot. The data model is append-only — once a name has been emitted in a
capture, the server keeps its row for the rest of that capture.

Two ways to get the clean feed:

- **Set the toggles before connecting.** A group that is off when the profiler
  attaches is never emitted, so no row is ever created. The plugin checks the
  toggle *before* it creates the plot name, which is the only moment the decision
  can still be made.
- **Reconnect.** The on-demand client discards plot data while nothing is
  attached, and replays only GPU contexts, lock names and thread names on
  connect — never plots. So the next connection shows exactly the groups that are
  enabled at that moment.

## Capturing

Enable the toggle, then start a Tracy server (the desktop `Tracy.exe`, or the
headless `tracy-capture` CLI). The editor connects and the timeline fills with
frame marks and plots. Because the plugin is Editor-scoped, it profiles the
editor — including gameplay running in the viewport's play mode.

## How it's wired (for plugin authors)

`plugins/tracy` is a standalone C-ABI plugin — it does not link Bevy, and its
only dependencies are `renzora_plugin` and `tracy-client`. It

- reads the host's measurements through the `Diagnostics` system param
  (`SystemCall::diagnostics`, ABI MINOR 4.8), which hands a system this frame's
  `DiagnosticsStore` as `(path, value, smoothed)` triples,
- registers its *Tracy Profiler* section with `App::add_settings_section`, whose
  `EmberToggle` reports through `PanelActionId`,
- persists its own opt-in, and
- creates nothing at all while off — no client, no socket, no plot names.

This is the reference example for reading diagnostics from a plugin: an FPS
overlay, a perf HUD or a telemetry uploader all want the same param.

```rust
use renzora_plugin::diagnostics::Diagnostics;

fn report(diags: Diagnostics) {
    if let Some(fps) = diags.get("fps") {
        info(&format!("{:.0} fps", fps.smoothed));
    }
}
```

Two things the host does not promise. **Which measurements exist** — an editor
carries all of them, a shipped game usually carries none, and a backend without
GPU timestamp queries has `render/*/elapsed_cpu` but not `elapsed_gpu`; `get`
returns `Option` for that reason. **That a present measurement has a value** — a
diagnostic registers before its first sample, so check `Diagnostic::is_valid()`
rather than plotting a `NaN`.

Nothing about Tracy is hardcoded into the editor or the contract.

## The UI Layout panel — bevy_ui cost without a Tracy build

Tracy answers "which system", but standing up a profiling build to ask one
question about the editor's own UI is a slow loop. The **UI Layout** panel
(Debug → UI Layout) answers the specific question that kept coming up, live and
with no rebuild: *where is bevy_ui's per-frame cost going?*

It brackets the UI pipeline with three timestamps around the public system sets:

```text
  A ── UiSystems::Prepare ‥ Propagate ‥ Content ── B ── Layout ── C
       └──────── content (text measurement) ──────┘   └─ taffy ─┘
```

and reports the two halves separately, plus a node census (total / hidden / text
/ visible text). The split is the actionable part, because the two halves have
opposite fixes: **content-bound** means fewer or cheaper labels, **taffy-bound**
means fewer nodes. The census refreshes only while the tab is open, and then only
every 30 frames — a panel about frame time should not cost frame time.

Read it against **UI Reactivity**'s `ms/frame recompute`. Whichever is larger is
the one worth optimising, and the answer is usually not the one you expect: the
measured split was **0.23 ms of reactivity against 5.48 ms of UI layout**.

> The stats resource is written *without* `bypass_change_detection`, deliberately
> — see [Reactivity](reactivity.md#bypass_change_detection-is-now-a-staleness-bug).

## Standing findings — don't undo these

These results are easy to reverse by accident, because in each case the
*expensive* option looks like the harmless default.

**Watch the filesystem; never poll it from a system.** `host::dev` used to
re-walk every plugin crate's source tree with a recursive `read_dir` every
0.25 s, diffing a map of `(mtime, len)` stamps. Measured on a splash screen with
no project open:

| | polling | `notify` watcher |
|---|---|---|
| `poll_plugin_sources` | 1278.0 µs/frame | **1.8 µs/frame** |
| its max | 31.33 ms | **0.10 ms** |
| frames in the 20.5-24 ms lump | 4.3% | **0.12%** |

351 walks at ~19 ms each across a 96 s capture, none of which found anything,
in every editor session — the install is gated on `is_editor`, not Dev Mode.
`notify` and `notify-debouncer-full` are already in the tree via bevy's
`file_watcher`, so this costs no new dependency; in `renzora_plugin` the dep is
gated behind the `host` feature so a plugin author still resolves to zero
dependencies.

Two details that are easy to get wrong and were both hit here. **Watch `src/`
and `Cargo.toml`, not the crate directory** — each plugin declares its own
`[workspace]` and therefore has its own `target/`, so a recursive watch makes
every rebuild flood the queue with its own build output. Filtering those paths
after delivery is not enough; an overflow drops real events alongside the noise.
And **an overflow must not trigger a rebuild-everything fallback** — a `git
checkout` would then rebuild all 63 plugins, which is the worst possible response
to the moment you least want one.

The remaining poll, `loader::poll_plugin_dir`, is deliberate: it stats one flat
directory of built libraries rather than a tree, and measures 15.6 µs/frame.

**Editor chrome is excluded with a query filter, never a per-entity check.** The
editor's own `bevy_ui` nodes live in the same `World` as the scene — roughly 1500
of them, ~950 of those named, on a completely empty project. Any system that means
"the scene" must therefore say so, and it must say so as a `With`/`Without` filter
so Bevy resolves it **once per archetype**. The rule is: an entity is scene content
unless it is a `bevy_ui` node, and a `bevy_ui` node is scene content only if it is
authored game UI (`UiCanvas`/`UiWidget`). `renzora_hierarchy::state::HierarchyCandidate`
is the canonical spelling.

Two places got this wrong and are worth understanding, because the mistake is
natural:

- `build_entity_tree` looped every archetype, then called `world.get::<Name>(entity)`
  on **every entity in the world** to find the named ones, then made three or four
  more random-access lookups per named entity to discard the chrome. All of that
  information is in the archetype; it was being re-derived per entity, up to 10x/sec,
  in an exclusive system.
- `ScriptComponent` was auto-inserted on every named entity, which meant every named
  UI node. Each insert is a deferred **archetype move** — the entity's whole component
  set is copied to a new table — and chrome respawns in bursts, so one panel rebuild
  became hundreds of archetype moves in a single frame. It also doubled the archetype
  count for UI, since each UI component-set then existed both with and without it.

**Behaviour contract this changes:** *nothing* receives a `ScriptComponent`
automatically any more. The auto-insert observer was first narrowed to skip `bevy_ui`
nodes and has since been removed outright — an empty component on every named entity
still cost an archetype move each and left the executor's `&ScriptComponent` query
walking entities with no scripts on them. Every path that needs the component now
creates it on demand: the inspector's **Scripts** entry, dropping a script or
blueprint file onto an entity, the hierarchy's New Asset menu, saving a blueprint
graph, and `renzora_ember::game_ui`, which still inserts one on `UiWidget`/`UiCanvas`
so `<input bind="Entity.var">` resolves. If you spawn an entity outside those paths
and need script variables on it, insert the component yourself.

**The inspector's Scripts section is nonetheless shown on every entity, and that is
not a regression of the above.** Its `has_fn` is unconditionally `true`, so the
section is inherent to the entity the way 2D Lighting is inherent to a `Camera2d` —
but the section is drawn over an *absent* component, and the add-bar inserts one only
when a script actually lands (removing the last script removes it again). Attaching a
script is among the most common things anyone does in the editor, and routing it
through Add Component → search → drop was pure ceremony in front of the action; the
fix for that is UI-side, and materialising an empty component on every entity to get
it would give back every cost listed above **plus** a `scripts: []` entry serialised
into every saved scene, since the component is registered for reflection. If you ever
want the component genuinely universal, the prerequisite is a marker component that
the executor, the hot-reload pass and the hierarchy's badge change-detection filter
on, so the empty ones sit in archetypes those queries never visit.

**Two costs that scaled with `ScriptComponent` count are also gone.**
`check_script_hot_reload` iterated `&mut ScriptComponent` every 0.5 s, and
`Mut::deref_mut` sets the change tick whether or not the write changes anything — so
every component in the scene was marked `Changed` twice a second. `renzora_hierarchy`'s
`AssetBadgeChanges` watches `Changed<ScriptComponent>` for its script badge, so that
storm set `HierarchyDirty` and forced `build_entity_tree` — a full-world scan on an
exclusive system — to run at 2 Hz forever with nothing changed. It now reads through
the immutable `Deref` and bails before touching `DerefMut` unless something is
genuinely stale. Separately, `scripts_should_run` scanned every `ScriptComponent` to
answer "is any script previewing?", and Bevy evaluates a run condition per system
rather than sharing the result — four systems gate on it, so that was four scans a
frame. The answer now lands in a `ScriptsActive` resource computed once in
`ScriptingSet::PreScript`, and the in-play case returns from `PlayModeState` without
iterating anything.

**`ghost_nodes` is deliberately off.** It's a `bevy_ui` feature that swaps
`UiChildren` for a much slower implementation on the editor's hottest path.
`update_children_recursively` calls `is_changed()` once per UI node per frame, and
it short-circuits only when a node's children actually changed — so in steady state
every node falls through to `iter_ghost_nodes()`, which returns a
`Box<dyn Iterator>`: **one heap allocation per UI node per frame**. With ~5k UI
entities that is the dominant term in `ui_layout_system`. `GhostNode` is used
exactly zero times in the workspace, so we pay it for nothing. Re-enabling it is an
[ABI bump](../extending/plugins.md) *and* a performance regression — only do it if
something genuinely needs `GhostNode`, and measure afterwards.

**`bevy_ui` has no hidden-subtree skip.** It walks the full UI tree three times a
frame unconditionally; `Display::None` does not prune it, and
`compute_hidden_layout` clears the cache and recurses, so hidden subtrees are never
even cached. This is why the dock despawns backgrounded panels rather than hiding
them (see [Panels](panels.md)) — hiding a panel does not make it free, and nothing
in the layout stage will make it free later.

**The inspector culls its off-screen sections, and must keep doing so.** Because
`bevy_ui` never prunes hidden subtrees (above), an open component section that has
scrolled out of the panel still charges a full tree walk every frame. The
inspector therefore throws a section's rows away once its body leaves the viewport
by more than half a screen, and rebuilds them when it scrolls back — the same
fill/unfill machinery collapsing a section already used, applied on a second axis.
Measured on one entity with its components open:

| | before | after |
|---|---|---|
| ms/frame UI layout | 5.48 | **3.36** |
| Layout (taffy) | 4.05 | **2.34** |
| Content (text measure) | 1.43 | **1.01** |
| Nodes total | 2814 | 2433 |

Two invariants hold it together, and both are "don't" rules that fail silently:
a section is **never culled before its height has been measured** (the reserved
height is what stops the list collapsing and the scroll range shifting under the
user), and a body is **never measured while it is not holding its rows** (that
records its padding as the section's height and reserves that forever). Both are
covered by `cull_tests` in `renzora_inspector::native`.

Note that this is *not* built on [`virtual_scroll`](widgets.md), which every other
editor list uses. That windows a `keyed_list` by measuring one row stride and
assuming every item shares it — exact for the asset grid and the hierarchy, and
wrong for the inspector, where a collapsed section is one header and an open one
with a native drawer is hundreds of px. Measuring each section's own height
sidesteps the assumption instead of fighting it.

A note on where to spend effort: across this pass, removing *discrete work*
(a system, a rebuild, a subtree) predicted its measured win 5 times out of 5, while
shaving *per-unit constants* on work that still ran predicted it 0 times out of 3.
If a change doesn't remove something from the frame entirely, be sceptical of the
estimate until Tracy confirms it.

## Runtime UI scale

Runtime UI scale still checks the current canvas and display size each frame,
but only writes the global scale when the calculated value differs. This avoids
unchanged-scale change notifications without relying on cached resize events.
The regression workload measures notifications across 1,000 stable frames and
window/canvas changes; it is not an FPS benchmark.

## Collision snapshots

Collision snapshots expose only the first contact that entered and the first
that exited during a frame. Both physics backends now stop each difference
iterator at that first contact, avoiding temporary lists of unused contacts.
The contact sets are still checked every frame; this is not an event-driven
physics rewrite or a measured FPS improvement. Contact ordering remains
unspecified, as before.

Contact-name strings retain capacity across enter/exit notifications. A
two-entity regression checks 1,000 alternating contact frames without growing
the warmed name buffers. Longer names can still grow those buffers; their
capacity is retained until the component is dropped.

Both backends compare live contacts against the prior snapshot before rebuilding
the contact set. Unchanged contacts keep that set, while one-frame flags and
names still clear. A 1,000-frame stable-contact regression performs zero rebuilds.
Contact traversal still happens each frame, and a transition can require a
second traversal to collect the replacement set. This is not an event-driven
update or a measured FPS improvement.

Velocity mirrors still read the current backend every frame, but only mark
`PhysicsReadState` changed when the velocity or speed bits differ. Missing
velocities still reset the reading to zero, and switching between 2D and 3D
still selects the correct backend. Grounded state is untouched. A headless
regression observes zero extra change notifications across 1,000 stable frames
per backend; this does not measure FPS or eliminate the velocity calculation.

## Plugin material settings

Plugin materials cache the entity groups that contain their settings instead
of searching every entity for each material. New groups are included as the
scene changes. The collector preserves the previous first-source ordering,
including disabled sources, and keeps the last uniform when no source exists.
It reuses its working lists and copies directly into uniforms only when bytes
change; unused materials no longer walk unrelated entities. Removed materials
release their unused cached queries.

The headless regression compares a scan of over 10,000 unrelated entities with
zero matching groups in the cached path, and checks 100 stable updates. This
measures avoided search work, not editor FPS or GPU timing.

## Script execution storage

Script execution temporarily owns only the entry list; it leaves the component
and its ID allocator attached to the entity. Runtime state returns to the same
list storage after hooks finish. Script commands remain deferred. A 1,000-update
regression observes zero component removals and preserves initialization and
disabled entries. The reflection read handlers share one immutable name lookup
per pass rather than cloning that map for each handler.

The built-in plugin-backed languages also share one immutable collection of
keyboard/action maps, gamepads, named entities and finished timers per pass.
They no longer clone those collections into every script context. The existing
per-backend frame encoding cache remains in place. A 1,000-pass regression runs
two scripts per pass with the same snapshot address and no owned name-map copies;
wire-format tests compare all shared fields and encoded bytes against owned data.
Custom engine-side backends retain isolated owned fields unless they explicitly
opt into `ScriptBackend::supports_shared_frame_inputs` and use
`ScriptContext::frame_inputs()`. Their ability to mutate their own copies remains
unchanged. Scene-name tables now retain immutable storage across passes. A
cached query compares ordered entity IDs and Name change ticks; it rebuilds the
tables on changes, including removals, disabling and duplicate-name order changes.
No name strings are cloned or hashed on a settled pass. The token scan remains,
and entity-specific child data and other input construction are separate costs.
The index is released when no eligible script entities remain, even if script
execution has stopped. Existing retained snapshots stay immutable.

## Hidden System Profiler

The System Profiler refreshes its view data only while its tab is active in a
main, fixed or floating dock. Its existing refresh interval remains in effect.
Underlying diagnostic capture stays enabled; only the view's sorting, formatting
and entity-count scans are gated. The headless gate regression checks 1,000
hidden updates and resumption in the main and fixed docks.

## Tile maintenance

Tile-object baking queries changed, unbaked or explicitly waiting objects.
Successfully baked objects leave the working set until edited. The waiting
marker preserves retries when a replacement atlas loads after the object's
change tick expires. A headless workload checks zero baker visits across 1,000
settled frames and successful re-baking after an edit.

Tileset sampler repair considers changed handles and image-added/modified/loaded
messages. Settled frames with no image messages visit only changed handles;
late loads and another consumer changing the sampler still trigger repair.

## Water materials

Water shading builds a complete candidate uniform off-asset and assigns it only
when values differ. Sun and recreated simulation texture handles still update;
the simulation itself is not paused. A headless 1,000-frame regression observes
zero stable material-change events, then verifies shading, sun and texture edits.

## Lumen geometry samples

Geometry extraction retains its sample vector and compares the exact resulting
bytes. Unchanged samples do not trigger another GPU upload; newly allocated GPU
buffers always receive the data. Camera culling, world transforms, sample order
and the 200,000-sample cap remain unchanged. Disabling injection clears the active
count but retains storage for reuse. CPU transformation still runs on active
frames; this is not per-mesh dirty-region caching or a measured FPS improvement.

## World-space UI meshes

Mesh-mode canvases share retained traversal, rectangle and text-entry scratch
buffers. Label strings are borrowed, and font sources are cloned only when a
mesh rebuild is needed. Font choice and text alpha participate in the content
hash. Component and relationship changes now route to affected mesh canvases;
settled canvases skip their layout walk and content hash. Typed change queries
still check component ticks, but unrelated UI changes do not traverse a canvas.
Headless tests retain all five rectangle buffers
through 1,000 fills and keep the actual canvas mesh handle across 1,000 stable
updates before checking a layout edit.

Text content is hashed as a complete string instead of one hasher call per
byte. A 4,352-byte regression reduces those calls to two while preserving edit
invalidation. The full text is still read; this is not constant-time hashing.

Removing the last visible background, making it fully transparent, or reducing
its layout to zero size now clears the old panel background mesh and material.
Restoring a background rebuilds it normally. The canvas is not hidden: text
uses separate child meshes, and the dark surface for a canvas without a template
remains owned by the panel synchronizer. Lifecycle tests cover all three empty
background transitions, repeated empty frames, restoration and fallback
preservation. Font asset revisions also invalidate text-bearing canvases. On
same-ID font-byte replacement, the shared text helper refreshes Bevy's font
collection through its public loader before the next mesh build; a hash change
alone would otherwise keep using stale glyph data. Background-only canvases do
not rebuild for font changes. This refresh is asset-event driven, not a per-frame
font-byte comparison. The regression replaces real font data under one handle
and checks that actual glyph geometry changes and then settles again.

The routing includes layout, transform, text/style, child-list and standard
Disabled changes/removals. New roots, changed canvas settings and pending font
builds still run. Membership tables only change when tree membership changes.
Unknown custom disabling filters retain the conservative eligibility walk.

A headless 1,024-node workload over 1,000 settled frames went from 1,025,000
node visits to zero. Observed test times were 314 ms before and 127–166 ms after;
these include scheduling/change-query overhead and are not graphical FPS results.
A 10,000-name, 100-pass workload went from 365 ms rebuilding tables to about 7 ms
reusing them. Exact timings depend on host load; retained allocations and work
counts are the regression guarantees.

## Audio timeline bookkeeping

The playback scheduler borrows the duration cache, removes stale voices in place,
and resolves source paths only for clips that need starting. It no longer clones
the full duration map or builds a temporary stale-clip list each active frame.
Scheduling still walks the timeline; seek tolerance, clip windows and backend
requests are unchanged. Headless coverage checks 1,000 stable playback updates,
muted/missing/future clips, finite-duration trimming, seeking and stopping.

## Wind-driven water

World-wind synchronization compares the current cascades directly with their
baseline, reuses baseline arrays on edits, and only writes changed wind values.
The existing quantization and authored-pattern scaling remain in place. A
1,000-frame stable regression retains baseline storage and emits no additional
water-component notifications; wind changes, authored edits and cascade removal
still update. The system still checks surfaces each frame.

## Sprite-derived state

Atlas-region cropping runs for changed regions or sprites. Y-sort runs for
changed settings, local transforms or propagated global transforms. Including
the output components preserves repair after another system edits those outputs.
A headless test sees no additional eligible entries across 1,000 settled frames
and verifies edits/movement. Bevy still evaluates change filters; this is not an
event-only index.

Sprite-sheet cropping now uses changed sheet/sprite inputs, a pending-image
marker, and image-added/modified/loaded messages. Missing images keep their old
crop and retry; settled sheets leave the ordinary crop query. Image-event frames
inspect handles to find affected sprites. A 1,000-frame headless regression
checks zero further eligible crop entries after settling, late image availability,
dimension changes, handle replacement, frame edits, output repair, reactivation
after expired events, 1×1 reset and removal of the pending marker.

Atlas regions, Y-sort, tile-object baking and tileset sampling also refresh on
reactivation. A shared removal reader marks the authored component changed when
the last query-disabling marker is removed, including custom markers registered
before schedule initialization. This preserves edits made while disabled after
their original change ticks or asset events have expired, without scanning all
enabled inputs again on settled frames.

Tile collider rebuilds also use Bevy's maintained child lists to find each
owner's tiles and generated shapes. They no longer scan the full tile/shape
queries once per owner. A reusable affected-owner set now gathers changed tile
values, parent/child relationships, palette changes, sheet/tile removals and
standard disable/reactivation signals. Palette edits include their paint layers;
the per-owner content hash still avoids unnecessary collider replacement.
Regression coverage checks two 64-tile layers, 1,000 settled frames with no owner
work, a one-owner tile edit, moves, removals and inherited palette changes.
Changing generated collider children can cause one extra hash check while the
relationships settle; this does not repeatedly rebuild unchanged colliders.

## September 6 performance batch validation

The batch passed 484 tests across 17 affected libraries, with four existing
ignored tests, plus 16 Rust-script unit tests and all 19 compiled-script
acceptance cases. Combined strict affected-library lint and the native editor
compile check passed. The acceptance cases exercise real compilation, activation,
failed reload preservation, supersession, project/identity changes, panic
containment and library retirement.

The measurements below count work avoided and storage retained; they are not FPS
benchmarks. No Windows build or graphical/hardware performance test was run for
this batch. Larger follow-ons remain: shared shader clock ownership,
vendor dependency migration and fully incremental
render/input/navigation/physics processing. Completing this batch does not mean
every audit finding or all repeated traversal has been eliminated.

## Hierarchy invalidation scope

The hierarchy retains its scene candidates and their ancestor dependencies when
building a tree. Changes to unrelated editor chrome no longer dirty that cache;
new candidates and changes to unnamed ancestors still do. Eligibility marker
and standard Disabled transitions are included. The existing 100 ms rebuild
debounce remains for genuine scene churn.

Custom filter membership is checked through retained entity archetypes and
resolved filter component IDs. Dynamic icon results are compared each frame,
including callbacks that read unrelated world state. Changes invalidate the
cached tree; unchanged results do not rebuild it. These lightweight dependency
checks still run, rather than claiming automatic discovery of callback reads.
The regression covers custom components on unnamed ancestors, entity-local and
global icon changes, and 1,000 unchanged frames without a tree rebuild.

## Empty hierarchy caching

An empty hierarchy is a valid cached result. The initial dirty flag requests
the first build; after that only invalidation requests another. The existing
100 ms debounce also applies to empty scenes. A headless lifecycle regression
checks 1,000 unchanged empty updates without rebuilding, then entity addition,
removal and another stable empty interval. Filtering editor-chrome invalidation
remains separate work.

## Navigation target cleanup

The navigation target cache consumes agent/path/global-transform removals before
checking for an available navigation mesh. Scene teardown therefore releases
obsolete entries even after the mesh is gone, without adding a full-world scan.
A headless test removes 1,000 agents while retaining a live agent's target, and
checks individual component removal. Repath and movement polling remain unchanged.

The script-readable navigation mirror now recalculates only after its agent,
path or world transform changes (or the mirror is first added), and publishes
only different values. A 1,000-frame test observes no stable publications;
non-observable speed edits remain quiet and target clearing still updates readers.

## Follow-up buffer reuse

Script execution retains its four timing-sample vectors after flushing them.
The production execution regression keeps the update buffer across 1,000 passes
and verifies all 1,000 timing samples still reach the statistics. Per-sample
script paths and other frame snapshots are still owned copies.

Reliable UDP packets are encoded once and their bytes retained until acknowledged.
The loopback regression sends 1,000 retries from the same allocation and checks
identical received bytes, acknowledgement cleanup and duplicate suppression.
The subsequent bounded-network change retains the packet format but limits the
send window to 1,024 sequence positions beyond the oldest unacknowledged event.
A fixed 128-byte replay bitmap and sequence floor replace lifetime history.
Accepted packets remain retryable; refused submissions report backpressure,
oversized data or connection/sequence exhaustion instead of growing indefinitely.
Server admission honors `max_clients`, and receive polls have a packet budget.
Delivery remains unordered and unauthenticated; these are memory/work bounds,
not internet-security guarantees. See [Multiplayer Overview](../multiplayer/overview.md).

## Gamepad input snapshots

Script input retains the three inner axis/button tables for each connected pad,
clearing and refilling them instead of allocating replacements. Disconnected
slots are removed; reconnecting into a free slot starts with fresh values. The
1,000-frame regression checks retained value storage, held/edge buttons, axis
values, disconnect and reuse. Input still updates every frame, including while
scripts are inactive; timer and preview behavior are unchanged.

## Load-progress path storage

Asset tracking and the asset/scene scripting bridges reuse unchanged filename
storage rather than cloning it each update. A headless 1,000-update regression
checks all three path buffers, path replacement/removal and an advancing clock.
Entity counting, state transitions and snapshot publication still run each frame.
`done` remains a persistent state and elapsed time continues after completion;
this change does not turn completion into a one-frame event or freeze the clock.

Script execution captures each loading bridge once per pass and shares its
immutable snapshot between entries. Installing the per-script handlers no longer
copies path strings; public reads still return independent owned snapshots.
Handlers are cleared after execution, and custom overrides cannot leak into the
next entry. A 1,000-pass regression covers both shared-input and legacy backends.

## UI sibling ordering

Z-index synchronization retains a parent work set populated from changed child
lists, added UI markers, changed parent/Z-index components, and removed UI or
Z-index components. Settled sibling groups are not traversed. Root-canvas global
ordering still receives the existing compare-first check. A 1,000-frame test
finds no additional parent work after settling; reordering, reparenting, marker
changes, output repair, despawn and root sort-order edits remain covered.
Standard `Disabled` additions/removals invalidate the affected groups too. When
custom query-disabling components are registered, ordering retains its original
eligibility scan as a compatibility fallback; the zero-parent-work measurement
applies to the standard configuration, not that fallback.

## Audio frame updates

Commands, live-player edits and spatial positions now stage into one reusable
request. The Cleanup-stage `audio_update` adds the listener, makes the frame's
Update call and consumes finished-voice/meter replies. Earlier Update calls no
longer discard those replies. Play/stop/load operations remain separate.
The regression drives all three production producers and the consumer: one
Update call per frame, ordered gain changes, finished-voice removal, live meters
and retained position-buffer storage across 1,000 updates. Failed one-frame
commands are cleared as before; positions are regenerated the next frame.

Spatial updates now compare each live voice's position against its last
successful send. Unchanged positions produce no movement payload. Failed sends
retry, and adopting another backend invalidates the cache even at the same
address. Ended voices are pruned and releasing the backend clears the cache.
Disabled emitters are skipped; their latest position is checked on reactivation.
The system still visits live voices each frame; this is a payload/allocation
reduction, not elimination of that scan or the listener/meter update call.
This change does not yet suppress unchanged position entries.

## Mixer bus comparison

The audio mixer compares borrowed bus keys and backend-facing values before
building an owned bus list. Stable settings no longer allocate a vector and
copy every bus key just for equality testing. Actual edits still build/send the
board, and a failed send still retries. The successful-board cache also records
the AudioLink adoption/release generation, so a newly adopted backend receives
the full settings even when their values are unchanged. Meter values and display names remain
outside that wire comparison. A 1,000-comparison regression with changing meters
requires zero new boards; edits, custom-bus order/count, NaN and signed-zero
semantics are checked against the previous owned-board equality.
An actual link-boundary test checks one send across 1,000 settled updates,
same-address re-adoption, failed-send retries and release/re-adoption.

## Startup GPU capability probe

Ray-tracing availability and the integrated-GPU hint share one cached temporary
adapter probe, including a cached failure. Backend mapping is shared with renderer
settings. This removes the second preliminary request; Bevy still creates its
own renderer adapter/device. The probe has no surface, so its answers remain
startup hints rather than guaranteed identification of the final adapter.
Headless tests inject the probe and count one request across 1,000 reads of both
answers, for success and failure. They also check backend/device policy. This is
not a measured startup-time or FPS claim.
