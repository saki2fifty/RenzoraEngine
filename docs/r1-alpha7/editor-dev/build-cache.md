# Build cache & retention

> **Phase 2 scope.** `crates/renzora_compiler_cache` provides an internal Tier-1 compiler/cache foundation. It is **not yet used by current editor-authored Rust scripts** — those continue to run on the unchanged Phase 1 build path (`crates/renzora_rust_script`). Phase 4 (after the Tier-1 script ABI exists) will migrate the script path to this cache. Today, this page documents the foundation and its production-path validation; the editor integration is deferred.

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

## Retention budgets

| Budget | Default | Applies to | Reset on editor restart |
|---|---|---|---|
| Cargo dep cache | 8 GiB | `<cache_root>/_cargo_target/` only | yes — cache contents survive |
| Published artifacts | 4 GiB | `<cache_root>/<id>/gen-*/` only — does **not** cover `_cargo_target/` | yes |

`MappedSet` (currently `dlopen` / `LoadLibraryW`'d artifacts) and `PinnedSet` (user-pinned generations, persisted in `<cache_root>/pins.json`) are **never evicted**. The active generation for every id is also never evicted. Cargo dep-cache partitions are evicted least-recently-touched-first; the sweep acquires the partition's `PartitionLock` (deferring on contention) and skips any partition reported as in flight by `CargoSupervisor::in_flight_partitions()`. Windows `ERROR_SHARING_VIOLATION` deletions are deferred to the next sweep.

## Lifecycle integration

The Phase 1 lifecycle (`LifecycleAction::Idle/OpenFirst/Keep/Switch/Close/RetryAttach`, `compile_for_new_project`, `ScriptWatcher::building`) drives the compiler service through a thin Bevy adapter (`CompileServiceResource` in `crates/renzora_rust_script/src/compile_service.rs`). Phase 2 does **not** redesign that lifecycle and does **not** change `LoadedScripts::insert/resolve/remove`. The new cache layer is consumed by the adapter; the lifecycle is unchanged. The Phase 1 acceptance tests (`watch::tests::scheduler_seven_frames`, `watch::tests::scheduler_with_external_pre_script_provider`) continue to pass with the adapter in place.

## Edge cases

- **Edit-during-build.** If the user edits the source while a Cargo build is running, the in-flight attempt is **not** cancelled. The attempt completes its scheduled revision; on completion the scheduler compares `latest_revision` to `completed_revision` and enqueues at most one follow-up attempt for the latest revision. Intermediate revisions are coalesced.
- **Transient infrastructure failure.** `cargo` exits non-zero but the error is `I/O`, `could not lock`, or similar — the worker treats it as transient and re-enqueues with jittered bounded backoff (initial 500 ms, cap 30 s, max 5 attempts then `CompileError`). The re-enqueueing background thread re-checks `transient_eligible_now()` after the delay and skips the enqueue if the revision has since advanced or the project closed.
- **Editor crash.** POSIX: each cargo child is its own process-group leader (`setpgid(0,0)`); the supervisor retains no PID list and the OS terminates the entire group only via `kill(-pgid, …)` from a deliberate shutdown path. On an unclean exit, orphan children inherit init. **Windows**: each cargo child is created suspended (`CREATE_SUSPENDED`), assigned to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumed. The supervisor retains the job handle; when the supervisor dies, the OS closes the handle and terminates the entire descendant tree. The supervisor never enumerates processes by name on startup.
