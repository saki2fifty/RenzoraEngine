# Build cache & retention

`crates/renzora_compiler_cache` provides the shared compiler and cache used by loose single-file plugins and by editor-authored Rust scripts. Both flows submit through the same `BuildService`, distinguished by `ArtifactKind::Tier1Plugin` and `ArtifactKind::Tier1Script`. The cache key is partitioned by crate-type tag, so a plugin edit does not invalidate a script cache entry and vice versa.

> **Validation status (rev-7-final).** The Linux paths (cargo invocation, fingerprint verification, `rename`-based atomic pointer replacement, A→B→A reactivation, real `dlopen`/symbol resolution, partition locking, bounded shutdown, identity-specific cancellation, parallel cache roots, two-distinct-partition concurrent compilation, active-child shutdown kills descendants, resistant-child shutdown honours the absolute deadline, descendant-with-retained-pipe-writer bounded shutdown, complete reader-thread diagnostics, complete build-input / fingerprint agreement, one-time authoritative toolchain + SDK discovery captured once per service generation, real `--locked` drift rejection without regenerating the lockfile, single canonical renderer for manifest bytes + wrapper hash, render emission of EXACTLY ONE `resolver = "2"` plus `renzora_plugin = { workspace = true, default-features = false, features = [...] }` inline in the dep table, direct dependency-reuse proof via cargo `--message-format=json-render-diagnostics` `compiler-artifact` events, A→B→C rapid-edit completion routing with exact `request_id` + immediate `Superseded` on submit) are validated by the acceptance tests under `crates/renzora_compiler_cache/tests/acceptance.rs` running under `--profile dist` in parallel (no `--test-threads=1` requirement). **R7-1 (rev-7)** — the build transaction is genuinely atomic: `render_workspace_and_package` is completely pure (no `create_dir_all`, no source writes, no manifest writes); one stable wrapper package per partition (no workspace-member growth when a new script identity appears); one partition-lock acquisition owns the entire cache-miss transaction (write manifests + selected source, bootstrap or read the lockfile, hash it, drift-check, cache lookup, cargo with `--locked`, stage artifact, release the lock). **R7-2 (rev-7)** — every rapid-edit receiver resolves exactly once within bounded time: A and B receive `BuildOutcome::Superseded { superseded_revision: <A|B>, by_revision: <B|C> }` deterministically at submit time; C receives `Published`/`CacheHit` for revision 3; the pending-request map is empty after all three resolve. **R7-3 (rev-7)** — the rapid-edit and same-partition tests use strict `recv_timeout(...).expect(...)` with no permissive branches (no `terminal >= 1`, no `< 2` lifecycle skip); the acceptance suite includes `prod_non_empty_capability_real_cargo_build` which runs a real Cargo build with the `static_plugins` capability and asserts the on-disk wrapper `Cargo.toml` encodes the capability INSIDE the `renzora_plugin = { workspace = true, default-features = false, features = ["static_plugins"] }` dep (no standalone `[dependencies]` table entry). **R7-final (rev-7-final)** — the first-build bypass is removed: every cargo build — including the very first build of a fresh partition — uses `--locked` with the authoritative `effective` whose `lock_resolution` and `lockfile_path` were derived from the lockfile bytes `ensure_lockfile` just bootstrapped. `prod_first_build_uses_locked_after_lockfile_bootstrap` starts from an empty partition, asserts `Cargo.lock` was bootstrapped, asserts the first cargo build was invoked with `--locked` (via a PATH-shadowing wrapper that records argv), asserts the build succeeds, asserts the post-build Cargo.lock bytes and SHA-256 equal the bytes that `cargo generate-lockfile` produces on a parallel mirror (cargo did not modify the lockfile), and asserts the published fingerprint's `lock_resolution` equals that exact hash. 49 acceptance tests + 13 lib tests pass. Phase 1 tests remain 36/36 (the editor Rust-script path is unchanged). Strict Clippy (`-D warnings -A clippy::too_many_arguments -A clippy::type_complexity`) is clean. **The Windows-only paths** (`CreatePipe` + inheritable handles + `CreateProcessW` with `CREATE_SUSPENDED` + Job Object + `ResumeThread`; `SetHandleInformation` return codes; `GetExitCodeProcess` for non-blocking exit; environment block built with case-insensitive sort; command-line quoted per `CommandLineToArgvW`; `TerminateJobObject` for shutdown) compile cleanly under `cargo check --target x86_64-pc-windows-msvc` (validated in this session via the `renzora-50f2cb55-windows` Docker container). **No Windows binary was built and no Windows host executed any test in this session.** A Windows validation pass is required before claiming the Windows contract is met on a Windows host.

## Cache root

`<cache_root>` is the directory the editor passes to `BuildServiceConfig`. On desktop it lives under the user-data directory; on CI / headless it is whatever the build script chose.

```
<cache_root>/
    _cargo_target/                   # shared Cargo target; one dir per (target, toolchain, abi, profile, capabilities) partition
    <id>/                             # one directory per canonical id
        active.bin                    # ActivePointer — atomically replaced (§ Active pointer)
        gen-<N>/                      # immutable published generation N
            lib<id>.<ext>             # the actual dylib / cdylib artifact
            fingerprint.bin           # length-delimited BuildFingerprint
            status.bin                # { kind, last_accessed_unix_seconds, abi_v }
        index.bin                     # per-id BTreeMap<Blake3(BuildFingerprint), PublishedGeneration> for inactive generations
        stage-<uuid>/                 # present only during staging; removed on success or moved to abandoned/ on startup recovery
        abandoned/<uuid>/             # left over from a crashed build; removed on the next startup sweep
```

`<id>` is the sanitized form of the canonical id (e.g. `project://enemy/spin.rs` → `project_c__s__enemy_s_spin.rs`). The encoding is reversible: `:` becomes `_c_`, `/` becomes `_s_`, `_` becomes `__`, `%` becomes `_p_`. Every dangerous character gets a unique encoding so two different canonical ids never share a directory.

## BuildFingerprint

The cache key is `Blake3(serialized_fingerprint)`, 32 bytes = 64 hex characters = 256 bits. There is no truncation. The fingerprint covers:

| Field | Source |
|---|---|
| `schema_version` | bumped when the field table itself changes |
| `canonical_identity` | the canonical id |
| `source_content` | exact bytes of the `.rs` file at the scheduled revision |
| `target_triple` | e.g. `x86_64-pc-windows-msvc` |
| `toolchain_stamp` | full `rustc -Vv` output captured at SDK build time |
| `sdk_content_hash` | Blake3 of the Tier 1 SDK package |
| `abi` | `{ version, interface_prefix_hashes: Vec<u32> }` — the `INTERFACE_PREFIX_HASHES` of the C-ABI surface |
| `wrapper_schema` | version of the generated `Cargo.toml` / `lib.rs` wrapper template |
| `manifest_schema` | version of the on-disk `active.bin` / `fingerprint.bin` / `status.bin` schemas |
| `lock_resolution` | SHA-256 of the canonical dep lock emitted by the cache |
| `capabilities` | `BTreeSet<String>` — `runtime`, `static_plugins`, `static_scripts`, … |
| `profile` | `dist` or `dist-lean` |
| `rustflags` | `Vec<String>` — Cargo `--config` overrides |
| `panic` | `abort` or `unwind` |
| `crate_type` | `cdylib` / `staticlib` / `dylib` |
| `compiler_service_schema` | version of `crates/renzora_compiler_cache` |

The full fingerprint is stored in `gen-<N>/fingerprint.bin` alongside the artifact. A cache hit is **not** accepted on hash equality alone: the loader re-reads `fingerprint.bin`, re-serializes the candidate fingerprint from the current `BuildRequest`, and compares byte-for-byte. This is the centralized fingerprint verifier (`staging::verify_generation_fingerprint`) and is used by **both** the active-pointer path and the inactive-reactivation path so the rules cannot diverge. This catches the (negligible-at-256-bit) hash-collision case and the case where a wrapper/schema change is supposed to invalidate the cache but inputs at the wrapper boundary were misreported.

## Active pointer (`active.bin`)

`active.bin` stores an `ActivePointer { generation, fingerprint_hash, compiler_service_schema }`. It is replaced via `replace_active_pointer`, which dispatches by platform:

| Platform | Operation |
|---|---|
| POSIX (Linux, macOS) | `rename(2)` of `active.bin.tmp.<uuid>` over the existing `active.bin` — atomic; readers see the old inode or the new inode, never partial. **Validated by `t2_27_active_pointer_replacement_is_atomic`.** |
| Windows | `ReplaceFileW(target, source, NULL, REPLACEFILE_WRITE_THROUGH \| REPLACEFILE_IGNORE_MERGE_ERRORS \| REPLACEFILE_IGNORE_ACL_ERRORS, NULL, NULL)` — the documented atomic-replace primitive on NTFS. **Code-reviewed only; not executed on a real Windows host in this change.** |

`FlushFileBuffers` / `fsync` is called on the temp file (and best-effort on the parent directory on POSIX) before the swap so a process crash between the swap and a kernel flush cannot expose an uninitialized pointer. Sharing-violation retries (50 ms initial, jittered bounded backoff, cap 1 s, max 20 attempts) cover real-world AV / indexer / OneDrive interference. The previous generation's directory is **not** removed if it is mapped, pinned, or the active generation of any id.

## Inactive-generation index (`index.bin`)

Every successfully published generation is indexed by its full fingerprint in a per-id sidecar:

```
<cache_root>/<id>/index.bin   # BTreeMap<Blake3(BuildFingerprint), PublishedGeneration>
```

The index is rebuilt from on-disk `gen-<N>/fingerprint.bin` records at startup (`rebuild_index_from_disk`) so it has no separate persistence guarantees. Lookup consults the active pointer first, then the inactive index. On an inactive match, the active pointer is updated to that generation via `replace_active_pointer` (no Cargo invocation, no library loaded twice). **Validated by `prod_a_b_a_reactivates_a_without_cargo`.**

## Shared Cargo target

A single `_cargo_target/` is partitioned only by genuinely incompatible inputs:

```
_cargo_target/
    <target_triple>/
        <toolchain_stamp>/
            <abi.version>+<hash>/
                <profile>/
                    <capabilities_canonical>/
                        <compiler_service_schema>/
                            generated/
                                script_<id_hash>/
                                    Cargo.toml
                                    src/lib.rs
```

Two unrelated scripts sharing (target, toolchain, abi, profile, capabilities) compile in the same partition and reuse the same `target/release/deps/*.rlib` artefacts. Concurrent cargo invocations inside one partition are serialized by a `PartitionLock` mutex (`cargo_target::PartitionLock` holds a real `parking_lot::Mutex<()>`; the guard is retained for the duration of the cargo invocation); different partitions run in parallel. **Validated by `prod_partition_eviction_uses_real_lock`.**

**Cross-platform artifact selection.** After cargo exits successfully, the cache extracts the compiled artefact from the `compiler-artifact.filenames` JSON stream. Selection is platform-aware (`compiler::locate_artifact`) so the same code path handles every supported target:

| target triple       | dynamic library | static library  |
| ---                 | ---             | ---             |
| `*-linux-*`         | `lib<name>.so`  | `lib<name>.a`   |
| `*-apple-darwin`    | `lib<name>.dylib`| `lib<name>.a`  |
| `*-windows-msvc`    | `<name>.dll`    | `<name>.lib`    |
| `*-windows-gnu`     | `<name>.dll`    | `lib<name>.a`   |

The selector ignores the SDK's own `renzora_plugin` artefacts (both `librenzora_plugin.so` and `renzora_plugin.dll`), Windows MSVC `<name>.dll.lib` import-library sidecars, `<name>.exp` and `<name>.pdb` debug artefacts, rustc intermediates (`*.d`, `*.rlib`, `*.rmeta`), and any other identity's cdylib. A miss produces a `LocateArtifactMiss` diagnostic that names the target triple, the platform-expected filename, and the full Cargo-emitted list. **Validated by 11 unit tests in `crates/renzora_compiler_cache/tests/acceptance.rs::unit_locate_artifact_*` plus a Windows cross-compile of `renzora_compiler_cache --tests --target x86_64-pc-windows-msvc` in the `renzora-50f2cb55-windows` container.**

## Retention budgets

| Budget | Default | Applies to | Reset on editor restart |
|---|---|---|---|
| Cargo dep cache | 8 GiB | `<cache_root>/_cargo_target/` only | yes — cache contents survive |
| Published artifacts | 4 GiB | `<cache_root>/<id>/gen-*/` only — does **not** cover `_cargo_target/` | yes |

`MappedSet` (currently `dlopen` / `LoadLibraryW`'d artifacts) and `PinnedSet` (user-pinned generations, persisted in `<cache_root>/pins.json`) are **never evicted**. The active generation for every id is also never evicted. Cargo dep-cache partitions are evicted least-recently-touched-first; the sweep acquires the partition's `PartitionLock` (deferring on contention) and skips any partition reported as in flight by `CargoSupervisor::in_flight_partitions()`. Windows `ERROR_SHARING_VIOLATION` deletions are deferred to the next sweep.

## Lifecycle integration

The existing Rust-script lifecycle (`LifecycleAction::Idle/OpenFirst/Keep/Switch/Close/RetryAttach`, `compile_for_new_project`, and `ScriptWatcher::building`) remains separate. The cache service does not change `LoadedScripts::insert/resolve/remove`; integration with that lifecycle belongs to the later Rust-script migration.

## Edge cases

- **Edit-during-build.** If the user edits the source while a Cargo build is running, the in-flight attempt is **not** cancelled. The attempt completes its scheduled revision; on completion the scheduler compares `latest_revision` to `completed_revision` and enqueues at most one follow-up attempt for the latest revision. Intermediate revisions are coalesced.
- **Transient infrastructure failure.** `cargo` exits non-zero but the error is `I/O`, `could not lock`, or similar — the worker treats it as transient and re-enqueues with jittered bounded backoff (initial 500 ms, cap 30 s, max 5 attempts then `CompileError`). The re-enqueueing background thread re-checks `transient_eligible_now()` after the delay and skips the enqueue if the revision has since advanced or the project closed.
- **Editor crash.** POSIX: each cargo child is its own process-group leader (`setpgid(0,0)`); the supervisor retains no PID list and the OS terminates the entire group only via `kill(-pgid, …)` from a deliberate shutdown path. On an unclean exit, orphan children inherit init. **Windows**: each cargo child is created suspended (`CREATE_SUSPENDED`), assigned to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumed. The supervisor retains the job handle; when the supervisor dies, the OS closes the handle and terminates the entire descendant tree. The supervisor never enumerates processes by name on startup.

## Offline build contract

The Tier-1 build path uses Cargo, which has its own network access
requirement on the very first build of each partition. The contract
is:

1. **First compile of a partition requires network access.** The
   worker invokes `cargo generate-lockfile` exactly once per partition
   to bootstrap `<generated_root>/Cargo.lock` from the rendered
   manifest + SDK. Cargo reaches `index.crates.io` (or a configured
   mirror) to resolve `renzora_plugin`'s dependencies (`proc-macro2`,
   `quote`, `syn`, `renzora_plugin_derive`). This step runs at most
   once per unique combination of `(target_triple, toolchain_stamp,
   capabilities, profile, abi_version, compiler_service_schema)`;
   subsequent submits targeting the same partition reuse the on-disk
   lockfile.

2. **Subsequent compiles are fully offline.** Once `Cargo.lock` is
   on disk, every compile in that partition passes `--locked` to
   `cargo build`. Cargo refuses to run if the resolved graph has
   drifted from the lockfile — that refusal is the `lockfile drift`
   error path, not a network error.

3. **Cache reuse is also offline.** A `CacheHit` reads the published
   artifact from the cache without invoking cargo at all. Only the
   first cache miss for a unique `(canonical_identity, source)`
   combination invokes cargo; a `CacheHit` does not.

4. **The network gate is one-time and one-partition.** An editor
   installed on a fully offline machine, with the SDK on disk and
   no prior `Cargo.lock` in any partition, will fail to compile any
   first-ever plugin of a new capability set until at least one
   successful network-resolved `cargo generate-lockfile` has run.
   After that, every compile, hot-reload, and cache hit is offline.

The harness's `make_fake_sdk` test fixture declares a `std` feature
on the fake `renzora_plugin` package because the production renderer
emits `features = ["std", ...]` on the per-package `renzora_plugin`
dependency (Y3-5) — a test that omits `std` from its fake SDK will
see `cargo generate-lockfile` fail with `package renzora_plugin does
not have that feature`. The acceptance test `prod_real_source_compiles_publishes_loads`
proves the first-compile network gate works end-to-end with the
real `BuildService` and a real SDK path.

## Loose-plugin integration

> The loose hot-plugin path uses the same
> `renzora_compiler_cache` service. A loose `plugins/<name>.rs` file is
> discovered by the editor-only `LoosePluginHost` watcher (root-level
> `notify-debouncer-full`, 300 ms debounce, non-recursive), parsed for
> its `renzora_plugin::add!(PluginType, Runtime|Editor)` declaration,
> and submitted as an `ArtifactKind::Tier1Plugin` request to the
> host-owned `BuildService` (one `Arc<BuildService>` per process). The
> service publishes immutable cache generations; the loose-plugin host
> stages each successful generation to a flat loader-visible path
> under `<plugins-dir>/.loose-staged/` and loads it through the new
> `renzora_plugin::host::loader::load_one_transactional`.

**Artifact-path handoff.** `BuildOutcome::{Published,CacheHit}`
carry the exact `immutable_artifact_path: PathBuf` the cache
publishes. The loose host consumes it directly — no fallback
directory scan, no process-relative cache guess. The path the
worker threads report is `cache.artifact_path(id, generation,
lib_ext)`, the same path the cache's `read_active` /
`lookup_inactive` lookups return.

**Stable staged path:** `<plugins-dir>/.loose-staged/<safe-id>.<ext>`,
where `<safe-id>` is the canonical identity's `to_scheme_path()` with
`:` and `/` replaced by `_`. The directory is FLAT — collision-proof
names per identity — so the slot identity keys on a single stable
file that survives every cache generation. `StableStaging::place`
copies the immutable generation to a `swap-<uuid>.<ext>` temp file
and atomically renames it onto the stable path (`rename(2)` on POSIX,
`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows).

**Compile failure does not touch the stable staged file.** The cache
publishes only on success, and the swap-rename only happens after the
cache reports `Published | CacheHit`. A failed submission leaves the
previous stable file in place and the row's status becomes
`CompileFailed`. The previous active generation is preserved.

**Transactional activation with owner-aware rollback.**
`load_one_transactional` audits every non-system host registration
surface reachable through `renzora_plugin::sys::Interface`. Every
slot-owned entry now carries an `(owner: usize, owner_generation:
u32)` pair so the candidate's mutations are distinguishable from the
prior generation's. `activate_with_transaction` snapshots:

- `RegistrySnapshot` of every registry, including the per-entry
  generation;
- the prior bytes of every Bevy resource via
  `host::read_resource_bytes_safe`, captured BEFORE
  `init_plugin_gen` so a candidate that overwrites a resource
  cannot also overwrite the snapshot;
- a `PluginComponentSchemas` clone for layout-conflict detection.

It then runs `init_plugin_gen` with `at = proposed_generation`
(candidate systems inert until commit), diffs the registries into
a `Journal` filtered by `(slot, proposed_generation)`, and either:

- commits: bump the slot's counter, retire ONLY the prior
  generation's slot-owned registrations via
  `retire_slot(world, slot, prior_loaded_at)`, refresh
  byte-compatible schemas via `refresh_compatible_schemas`, and
  atomically publish the new generation counter. The candidate's
  entries (at `proposed_generation`) survive untouched; or
- rolls back: `apply_journal_rollback(world, &mut journal, slot,
  proposed_generation)` undoes every entry in reverse — including
  restoring resource bytes via
  `host::write_resource_bytes_unsafe` from the captured
  `prior_bytes`. The slot's `loaded_at` and counter are restored
  to their prior values, and the candidate's systems stay
  permanently inert (`at != counter`).

Layout conflict detection is real: `detect_layout_conflict`
compares the candidate's `PluginComponentInfo` against its pre-init
counterpart by `size`, field count, per-field offset, and per-field
kind discriminant. A mismatch refuses the candidate with
`ActivationFailure::LayoutConflict(why)` and rolls back; the prior
generation stays active with all of its registrations intact.

The journal types live in `renzora_plugin::host::{JournalEntry,
RegistrySnapshot, TransactionJournal}` (the contract crate); the
loose-plugin crate re-exports them. `PluginComponentOwners` is the
companion map that records `(slot, owner_generation)` for every
component / resource id so a same-slot reload can retire only the
prior generation's ids.

**Never-unload invariant.** Every `Library` opened by
`Library::new` in `load_one_transactional` is wrapped in
`ManuallyDrop` and pushed onto one of two pools in the
`PluginSlot`:

- `PluginSlot::_libraries`: committed loads.
- `PluginSlot::failed_libraries`: rolled-back loads (open
  succeeded but symbol / scope / ABI / init / layout refused).

Dropping a `Library` would call `FreeLibrary`, which has deadlocked
on this platform; any function pointer the candidate registered
would point at freed memory. Neither pool is ever drained.

**Windows shadow copy.** `load_one_transactional` calls the existing
`shadow_copy(stable_path, generation)` helper before mapping the
image, so a mapped DLL never blocks the staged-file replace. The
editor-only shadow copy directory is `stable_path.parent()/.reload/`,
named `<stem>-<generation>.<ext>`. A shipped game maps the stable
file directly without a shadow copy.

**Editor Rust scripts remain on `renzora_rust_script`.** They are not
yet migrated to the cache service. The
`renzora_loose_plugins` crate does not depend on `renzora_rust_script`.

**Settings, trust, and reload.** Loose plugin cards on the Settings
→ Editor → Plugins panel carry a **Grant / Revoke trust** button and
a **Reload** button. They mutate the authoritative
`LoosePluginInventory` and `LoosePluginTrust` resources and enqueue
a manual reload via `LoosePluginReloadRequests`, which the loose
host drains in `PreUpdate`. The full canonical identity is the
durable key in `renzora::PluginInventory` (not the bare leaf), so
two plugins with the same filename remain distinct.

**Export.** Active Runtime loose plugins are copied into the
export tree by `renzora_export::build::stage_loose_plugins_from`.
The export overlay reads `LoosePluginInventory::export_candidates`
while it holds `&mut World`, snapshots the list, and passes it to
the background export worker. Editor-scoped loose plugins are
filtered upstream; disabled plugins are filtered at staging time.
