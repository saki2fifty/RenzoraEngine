//! Integration harness for the Phase 3 acceptance suite.
//!
//! The harness compiles real cdylibs, drives the production
//! `load_one_transactional` path, runs Bevy schedules via
//! `app.update()`, observes raw component/resource bytes, and
//! installs the loose-host Bevy plugin / Settings so the integration
//! tests exercise the real systems.
//!
//! Tests never construct a journal entry by hand and never queue a
//! reload request directly: any action that has a production
//! counterpart uses the production function. The shared
//! `apply_loose_plugin_toggle` command in `host_plugin` is what the
//! Settings UI's `plugin_toggle_click` system AND the harness's
//! `Harness::apply_toggle` both call.

use bevy::prelude::*;
use crossbeam_channel::Receiver;
use renzora_compiler_cache::types::{
    default_target_triple, ArtifactKind, BuildOutcome, BuildProfile, BuildRequest,
    FingerprintInputs, PanicStrategy, Revision,
};
use renzora_compiler_cache::{BuildService, BuildServiceConfig};
use renzora_identity::CanonicalId;
use renzora_loose_plugins::contract::LoosePluginScope;
use renzora_loose_plugins::host_plugin::{apply_loose_plugin_toggle, PendingBuild};
use renzora_loose_plugins::{LoosePendingBuilds, LoosePluginInventory, LoosePluginReloadRequests};
use renzora_plugin::host::loader::{self, LoadOutcome, TransactionalActivationOutcome};
use renzora_plugin::host::{
    apply_journal_rollback, diff_registrations, register_custom_material_for_test,
    CustomMaterialApplier, PendingMaterials, PendingPostProcesses, PendingRenderPasses,
    PluginAssets, PluginAudioBackend, PluginComponentSchemas, PluginComponents, PluginHttpInbox,
    PluginNetBackend, PluginPanels, PluginResources, PluginScriptBackends, PluginServiceReplies,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Heavyweight BuildService tests (real Cargo-backed integration
/// tests that spawn child cargo processes and write wrapper workspaces)
/// contend for the partition workspace lock inside the
/// `BuildServiceDriver`. Two such tests run in parallel under the
/// default Cargo test runner will each spawn their own BuildService
/// against overlapping temp dirs and serialise on internal worker
/// acquisition, frequently pushing the staged-file wait past the
/// 120-second budget. Y3-4/Y3-7: a single shared `Mutex` keeps these
/// tests deterministic without forcing the whole suite to one
/// thread. Other tests (synthetic cdylib, harness compile-to-rustc)
/// do not touch BuildService and run freely in parallel.
static HEAVY_BUILD_SERVICE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn artifact_directories(test_exe: &Path) -> (PathBuf, PathBuf) {
    let deps = test_exe
        .parent()
        .expect("acceptance executable has a dependency directory");
    let fingerprints = deps
        .parent()
        .expect("dependency directory has a profile parent")
        .join(".fingerprint");
    (deps.to_path_buf(), fingerprints)
}

#[test]
fn artifact_directories_follow_the_test_executable() {
    for profile in [
        "target/dist",
        "target/llvm-cov-target/dist",
        "custom-output/x86_64-unknown-linux-gnu/release",
    ] {
        let profile = Path::new(profile);
        let (deps, fingerprints) = artifact_directories(&profile.join("deps/acceptance-hash"));
        assert_eq!(deps, profile.join("deps"));
        assert_eq!(fingerprints, profile.join(".fingerprint"));
    }
}

#[cfg(unix)]
#[test]
fn relocated_acceptance_executable_compiles_and_runs_a_plugin() {
    let executable = std::env::current_exe().expect("test executable");
    let (deps, fingerprints) = artifact_directories(&executable);
    let relocated = tempfile::tempdir().expect("relocated test directory");
    let profile = relocated.path().join("custom-target/dist");
    let relocated_deps = profile.join("deps");
    std::fs::create_dir_all(&relocated_deps).expect("relocated dependency directory");
    // Link dependencies rather than duplicating the large Bevy artifact tree.
    // The executable itself must not be a symlink: current_exe resolves it.
    for entry in std::fs::read_dir(&deps).expect("dependency artifacts") {
        let path = entry.expect("dependency entry").path();
        if matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("rlib" | "rmeta" | "so" | "dylib")
        ) {
            std::os::unix::fs::symlink(&path, relocated_deps.join(path.file_name().unwrap()))
                .expect("link dependency artifact");
        }
    }
    std::os::unix::fs::symlink(fingerprints, profile.join(".fingerprint"))
        .expect("link fingerprints");
    let child = relocated_deps.join("acceptance-relocated");
    std::fs::copy(executable, &child).expect("copy acceptance executable");
    let result = std::process::Command::new(child)
        .args([
            "--exact",
            "runtime_execution_plugin_system_runs_and_writes_observed_value",
            "--nocapture",
        ])
        .output()
        .expect("run relocated acceptance test");
    assert!(
        result.status.success(),
        "relocated test failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

pub fn heavy_build_service_lock() -> MutexGuard<'static, ()> {
    HEAVY_BUILD_SERVICE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A compiled cdylib plus a handle on the `tempfile::TempDir` it
/// was produced in. Dropping the cdylib directory removes the
/// backing file, so the harness returns both halves together.
pub struct CompiledCdylib {
    pub path: PathBuf,
    _workdir: tempfile::TempDir,
}

impl std::ops::Deref for CompiledCdylib {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for CompiledCdylib {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Per-process env shared by every test. Holds workdir, staging
/// directory, the BuildService, and a per-test prefs path so F3-10
/// does not mutate `HOME`.
pub struct TestEnv {
    /// Unique workdir per test instance. Drop cleans it up.
    pub workdir: tempfile::TempDir,
    /// Stable staging directory for the cdylib artifacts the tests
    /// use. `tempfile::TempDir` cleans up the directory on drop.
    pub staging_dir: PathBuf,
    /// BuildService config — separate per test so cache roots do not
    /// collide and stale artifacts cannot reach a subsequent test.
    pub cache_root: PathBuf,
    /// Explicit preferences root the persistence tests use. Production
    /// functions consult `~/.renzora/editor.toml`; tests use a
    /// `RenzoraTestEnv::prefs_path` instead. The path lives under the
    /// workdir so it is unique per test and survives parallel runs.
    pub prefs_root: PathBuf,
}

impl TestEnv {
    pub fn new() -> Self {
        let workdir = tempfile::tempdir().unwrap();
        let staging_dir = workdir.path().join("staged");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let cache_root = workdir.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();
        let prefs_root = workdir.path().join("prefs");
        std::fs::create_dir_all(&prefs_root).unwrap();
        Self {
            workdir,
            staging_dir,
            cache_root,
            prefs_root,
        }
    }

    /// Path to a per-test editor preferences TOML. Tests pass this to
    /// `renzora::save_trusted_loose_plugins_at` etc.
    pub fn prefs_path(&self) -> PathBuf {
        self.prefs_root.join("editor.toml")
    }
}

/// BuildService driver. Builds a real `BuildService` per test against
/// the per-test `cache_root` and `staging_dir`. Distinct from the
/// Production `LooseBuildService` resource in that this one is owned
/// by the test so receivers can be probed directly. F3-9: we still
/// feed the receivers into the production `LoosePendingBuilds` and
/// drive the production drain systems.
pub struct BuildServiceDriver {
    pub service: Arc<BuildService>,
}

/// Wait for `rx` to deliver an outcome (or timeout).
pub fn recv_until(rx: &Receiver<BuildOutcome>, deadline: Instant) -> Option<BuildOutcome> {
    loop {
        match rx.try_recv() {
            Ok(o) => return Some(o),
            Err(crossbeam_channel::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => return None,
        }
    }
}

/// The harness. Holds the Bevy `App`, the `TestEnv` it ran in, and
/// the cdylib compile cache.
pub struct Harness {
    pub env: TestEnv,
    pub app: App,
}

impl Harness {
    pub fn with_minimal_plugins(env: &TestEnv) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        // The host resources the transactional path touches.
        app.init_resource::<PluginPanels>();
        app.init_resource::<PluginResources>();
        app.init_resource::<PluginComponentSchemas>();
        app.init_resource::<PluginComponents>();
        // Z3-1: the scene-side mirror of plugin-owned schemas. The
        // production bridge (`renzora_engine::plugin_scene_bridge`)
        // inserts this; the harness ships an empty one so the
        // production-equivalent identity test can assert two
        // plugins' full paths simultaneously coexist in
        // `RawTypeTable::by_path` after `refresh_raw_component_registry`
        // is called by the test.
        app.init_resource::<renzora_bsn::RawComponentRegistry>();
        app.init_resource::<AppTypeRegistry>();
        app.init_resource::<PendingRenderPasses>();
        app.init_resource::<PendingPostProcesses>();
        app.init_resource::<PendingMaterials>();
        app.init_resource::<PluginAssets>();
        app.init_resource::<PluginScriptBackends>();
        app.init_resource::<PluginAudioBackend>();
        app.init_resource::<PluginNetBackend>();
        // CustomMaterialApplier is a function pointer — no Default.
        // A no-op applier is fine for tests because we never spawn a
        // mesh with a custom-material slot; rollback tests only
        // mutate the registries.
        let noop_applier: CustomMaterialApplier = CustomMaterialApplier(|_, _, _| {});
        app.world_mut().insert_resource(noop_applier);
        app.init_resource::<PluginHttpInbox>();
        app.init_resource::<PluginServiceReplies>();
        if !app.world().contains_resource::<Assets<Shader>>() {
            app.world_mut().insert_resource(Assets::<Shader>::default());
        }
        Self {
            env: TestEnv {
                workdir: _fresh_workdir(),
                staging_dir: env.staging_dir.clone(),
                cache_root: env.cache_root.clone(),
                prefs_root: env.prefs_root.clone(),
            },
            app,
        }
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn update(&mut self) {
        // Set up Time so plugin systems that take `Res<Time>` can
        // observe a non-zero delta.
        if !self.app.world().contains_resource::<Time>() {
            self.app.world_mut().init_resource::<Time>();
        }
        {
            let mut time = self.app.world_mut().resource_mut::<Time>();
            time.advance_by(std::time::Duration::from_micros(16_667));
        }
        // The loose-host plugin's drain systems run in `PreUpdate`
        // and the plugin systems run in `Update`. `MinimalPlugins`
        // does not initialise `MainScheduleOrder` for `app.update()`
        // to drive, so we walk the explicit pipeline.
        let _ = self
            .app
            .world_mut()
            .try_run_schedule(bevy::prelude::PreUpdate);
        let _ = self.app.world_mut().try_run_schedule(bevy::prelude::Update);
        let _ = self
            .app
            .world_mut()
            .try_run_schedule(bevy::prelude::PostUpdate);
    }

    /// Try-compile variant: returns `Some(CompCdylib)` on success, `None`
    /// when rustc exited non-zero. Used when a test exercises a
    /// source that intentionally reaches a build path the host
    /// doesn't expose (e.g., a source that calls a host symbol not in
    /// the ifaces this build compiled with).
    pub fn try_compile_source_to_cdylib(
        &self,
        source: &str,
        crate_name: &str,
    ) -> Option<CompiledCdylib> {
        let workdir = tempfile::tempdir().unwrap();
        let src_path = workdir.path().join(format!("{crate_name}.rs"));
        // The source is written verbatim. The harness does NOT wrap it
        // in a generated module — that would change `crate::` semantics
        // and is explicitly prohibited by the ninth-correction review.
        // Durable identity is constructed at the host boundary from
        // the canonical identity the caller supplies; the harness's
        // direct-rustc path finds the same durable name as the
        // BuildService path because both paths consult
        // `renzora_plugin::host::durable_type_path`.
        std::fs::write(&src_path, source).expect("write source");
        // Cargo places this test executable beside its dependency artifacts.
        // Resolve from the running binary, not a guessed target/profile path:
        // coverage, --target-dir, and cross-target builds all relocate it.
        let test_exe = std::env::current_exe().expect("locate acceptance executable");
        let (deps_dir, fingerprint_dir) = artifact_directories(&test_exe);
        let mut renzora_rlibs: Vec<PathBuf> = std::fs::read_dir(&deps_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        n.starts_with("librenzora_plugin-") && n.ends_with(".rlib")
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The acceptance suite's renzora_plugin dependency is declared
        // with `features = ["host"]`. Pick the rlib whose fingerprint
        // features also list `host` AND whose hash directory carries
        // the matching fingerprint JSON. Falls back to the most
        // recent mtime if no exact match is found, but the precise
        // selection avoids pulling in a rlib built against a
        // different feature set (e.g. one that also enables
        // audio/net and bakes different symbol resolutions in),
        // which would cause rustc to reject the produced cdylib as
        // a duplicate symbol or fail to link at all.
        let mut chosen: Option<(PathBuf, std::time::SystemTime)> = None;
        eprintln!("DEBUG: harness filter pass, candidates:");
        for rlib in &renzora_rlibs {
            // rlib filename: librenzora_plugin-<hash>.rlib
            if let Some(hash) = rlib
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.strip_prefix("librenzora_plugin-"))
                .and_then(|s| s.strip_suffix(".rlib"))
            {
                let fp = fingerprint_dir.join(format!("renzora_plugin-{hash}"));
                let fp_json = fp.join("lib-renzora_plugin.json");
                if let Ok(text) = std::fs::read_to_string(&fp_json) {
                    // The fingerprint JSON is a single line of escaped
                    // JSON, so the literal characters in the file are
                    // `"features":"[..\"host\"..]"`. Match on the
                    // actual escape patterns present in the file.
                    let matches =
                        text.contains(r#""\""host\"",\""#) || text.contains(r#""\""host\"" ]"#);
                    if matches {
                        if let Ok(meta) = std::fs::metadata(rlib) {
                            if let Ok(modified) = meta.modified() {
                                if let Some((_, prev)) = chosen {
                                    if modified > prev {
                                        chosen = Some((rlib.clone(), modified));
                                    }
                                } else {
                                    chosen = Some((rlib.clone(), modified));
                                }
                            }
                        }
                    }
                }
            }
        }
        let renzora_rlib = chosen
            .map(|(p, _)| p)
            .or_else(|| {
                // Fallback: most-recent mtime. Many tests still
                // happen to work with the latest version because the
                // host rlib is largely binary-compatible across
                // feature sets, but the precise selection above is
                // safer.
                renzora_rlibs.sort_by_key(|p| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                });
                renzora_rlibs.last().cloned()
            })
            .unwrap_or_else(|| panic!("renzora_plugin rlib not found in {}", deps_dir.display()));
        let ext = std::env::consts::DLL_EXTENSION;
        let out = workdir.path().join(format!("lib{crate_name}.{ext}"));
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
        let output = std::process::Command::new(&rustc)
            .current_dir(workdir.path())
            .arg("--edition=2021")
            .arg("--crate-type=cdylib")
            .arg(format!("--crate-name={crate_name}"))
            .arg("-L")
            .arg(format!("dependency={}", deps_dir.display()))
            .arg("--extern")
            .arg(format!("renzora_plugin={}", renzora_rlib.display()))
            .arg("-C")
            .arg("panic=abort")
            .arg(&src_path)
            .arg("-o")
            .arg(&out)
            .output()
            .expect("spawn rustc");
        if output.status.success() {
            // Persist the workdir path under an env var so we can
            // inspect the produced cdylib if the test fails.
            std::env::set_var(
                format!("HARNESS_CDYLIB_{crate_name}"),
                out.display().to_string(),
            );
            Some(CompiledCdylib {
                path: out,
                _workdir: workdir,
            })
        } else {
            eprintln!(
                "rustc failed for crate {crate_name}\nstdout: {}\nstderr: {}\nworkdir: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                workdir.path().display()
            );
            None
        }
    }

    /// Wrapper around [`Self::try_compile_source_to_cdylib`] that
    /// unwraps. Returns the [`CompiledCdylib`] which keeps the
    /// underlying workdir alive for the produced `.so` lifetime.
    pub fn compile_source_to_cdylib(&self, source: &str, crate_name: &str) -> CompiledCdylib {
        self.try_compile_source_to_cdylib(source, crate_name)
            .expect("cdylib compile failed")
    }

    pub fn load_plugin_source(
        &mut self,
        source: &str,
        crate_name: &str,
        identity: &CanonicalId,
    ) -> Result<TransactionalActivationOutcome, LoadOutcome> {
        let path = self.compile_source_to_cdylib(source, crate_name);
        loader::load_one_transactional(
            self.app.world_mut(),
            &path,
            true,
            &[],
            &[],
            identity.clone(),
        )
    }

    pub fn load_one_transactional_at(
        &mut self,
        staged: &Path,
        identity: &CanonicalId,
    ) -> Result<TransactionalActivationOutcome, LoadOutcome> {
        loader::load_one_transactional(
            self.app.world_mut(),
            staged,
            true,
            &[],
            &[],
            identity.clone(),
        )
    }

    pub fn load_dir(&mut self, dir: &Path) -> Vec<(PathBuf, LoadOutcome)> {
        loader::load_dir(self.app.world_mut(), dir, dir, true, &[], &[])
    }

    /// Load with an explicit discovery root. The directory plugins
    /// store their canonical identity derived from the root-relative
    /// path; tests that exercise identity reuse across
    /// restart / reload call this rather than `load_dir`.
    pub fn load_dir_with_root(
        &mut self,
        dir: &Path,
        discovery_root: &Path,
    ) -> Vec<(PathBuf, LoadOutcome)> {
        loader::load_dir(self.app.world_mut(), dir, discovery_root, true, &[], &[])
    }

    /// Install the production `RenzoraPluginHostPlugin`. After this,
    /// `request_reload_at` followed by `run_plugin_reload_schedule`
    /// exercises the SAME reload path the file watcher drives in the
    /// editor (the watcher calls `request_reload`; the `PluginReload`
    /// schedule runs `apply_reload_requests` on the next frame).
    pub fn install_plugin_host(&mut self, plugin_dir: &Path) {
        self.app
            .add_plugins(renzora_plugin::host::loader::RenzoraPluginHostPlugin {
                is_editor: true,
                statics: Vec::new(),
                disabled: Vec::new(),
            });
        // The plugin chooses its own `dir` from `current_exe`; in
        // tests we want the watcher to observe a specific directory
        // — overwrite the resource after install so the watcher
        // polls the right place. `for_tests` primes the watcher's
        // `seen` map from a directory stat so the watcher does not
        // re-fire every existing plugin as "changed".
        self.app.world_mut().insert_resource(
            renzora_plugin::host::loader::PluginWatcher::for_tests(plugin_dir.to_path_buf()),
        );
    }

    /// Push `path` onto the reload queue and run the `PluginReload`
    /// schedule once. Tests use this to simulate the watcher firing.
    pub fn drive_reload(&mut self, path: &Path) {
        renzora_plugin::host::loader::request_reload(self.app.world_mut(), path.to_path_buf());
        let _ = self
            .app
            .world_mut()
            .try_run_schedule(renzora_plugin::host::loader::PluginReload);
    }

    pub fn slot_loaded_at(&self, staged: &Path) -> u32 {
        let Some(loaded) = self
            .app
            .world()
            .get_resource::<renzora_plugin::host::loader::LoadedPlugins>()
        else {
            return 0;
        };
        loaded
            .0
            .iter()
            .find(|s| s.path.as_path() == staged)
            .map(|s| s.loaded_at)
            .unwrap_or(0)
    }

    /// Read a plugin-registered `ComponentId` whose `type_path` matches
    /// `needle`. Tests compare against the Bevy-emitted
    /// `<module_path!>::<name>` for a struct.
    pub fn component_id_by_type_path(&self, needle: &str) -> bevy::ecs::component::ComponentId {
        let schemas = self.app.world().resource::<PluginComponentSchemas>();
        schemas
            .0
            .iter()
            .find(|s| s.type_path.contains(needle))
            .map(|s| s.id)
            .unwrap_or_else(|| panic!("no schema matches `{needle}`"))
    }

    /// Spawn an entity with a raw-bytes component. Production code
    /// (the inspector, the scene loader) goes through this same
    /// `insert_by_id` path.
    pub fn spawn_entity_with_raw_component<T: Copy>(
        &mut self,
        id: bevy::ecs::component::ComponentId,
        value: &T,
    ) -> Entity {
        let entity = self.app.world_mut().spawn_empty().id();
        unsafe {
            let mut bytes = std::slice::from_raw_parts(
                (value as *const T).cast::<u8>(),
                std::mem::size_of::<T>(),
            )
            .to_vec();
            let ptr = bevy::ptr::OwningPtr::new(std::ptr::NonNull::new_unchecked(
                bytes.as_mut_ptr().cast(),
            ));
            self.app
                .world_mut()
                .entity_mut(entity)
                .insert_by_id(id, ptr);
            std::mem::forget(bytes);
        }
        entity
    }

    pub fn read_raw_component_u32(
        &self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
    ) -> u32 {
        self.read_raw_component::<u32>(entity, id)
    }

    pub fn read_raw_component_i32(
        &self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
    ) -> i32 {
        self.read_raw_component::<i32>(entity, id)
    }

    pub fn read_raw_component<T: Copy + Default>(
        &self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
    ) -> T {
        let world = self.app.world();
        let ent = world.entity(entity);
        let ptr = ent.get_by_id(id).expect("component missing on entity");
        // SAFETY: the ptr was inserted with this layout. We copy out
        // a single instance.
        unsafe { std::ptr::read_unaligned(ptr.as_ptr().cast::<T>()) }
    }

    pub fn write_raw_component_u32(
        &mut self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
        v: u32,
    ) {
        self.write_raw_component(entity, id, &v);
    }

    pub fn write_raw_component_i32(
        &mut self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
        v: i32,
    ) {
        self.write_raw_component(entity, id, &v);
    }

    pub fn write_raw_component<T: Copy>(
        &mut self,
        entity: Entity,
        id: bevy::ecs::component::ComponentId,
        v: &T,
    ) {
        unsafe {
            let mut bytes =
                std::slice::from_raw_parts((v as *const T).cast::<u8>(), std::mem::size_of::<T>())
                    .to_vec();
            let ptr = bevy::ptr::OwningPtr::new(std::ptr::NonNull::new_unchecked(
                bytes.as_mut_ptr().cast(),
            ));
            self.app
                .world_mut()
                .entity_mut(entity)
                .insert_by_id(id, ptr);
            std::mem::forget(bytes);
        }
    }

    /// Observe a single resource in `Read` mode.
    pub fn observe<R: Resource, F: FnOnce(&R) -> bool>(&self, f: F) -> bool {
        f(self.app.world().resource::<R>())
    }

    /// Inject the loose-host Bevy plugin into the harness App. After
    /// calling this, `update()` runs the production drain systems.
    pub fn install_loose_host(&mut self, env: &TestEnv) {
        self.install_loose_host_with_build_service(env, None);
    }

    /// Same as [`Self::install_loose_host`], but if `shared_service` is
    /// `Some`, the loose host installs THAT BuildService as its
    /// `LooseBuildService` resource. Used by the supersession test to
    /// share one BuildService between the harness's submission path and
    /// the host's drain systems.
    pub fn install_loose_host_with_build_service(
        &mut self,
        env: &TestEnv,
        shared_service: Option<std::sync::Arc<renzora_compiler_cache::BuildService>>,
    ) {
        let loose = if let Some(svc) = shared_service {
            renzora_loose_plugins::host_plugin::LoosePluginHost::editor_with_shared_build_service(
                &env.staging_dir,
                Vec::new(),
                Vec::new(),
                svc,
            )
        } else {
            renzora_loose_plugins::host_plugin::LoosePluginHost::editor_with_trust(
                &env.staging_dir,
                Vec::new(),
                Vec::new(),
            )
        };
        self.app.add_plugins(loose);
    }

    pub fn upsert_discovered_plugin(&mut self, id: &CanonicalId) {
        let world = self.app.world_mut();
        let mut inv = std::mem::take(&mut *world.resource_mut::<LoosePluginInventory>());
        inv.upsert_discovered(
            id.clone(),
            LoosePluginScope::Runtime,
            self.env.staging_dir.join(id.to_string()),
            true,
            false,
        );
        *world.resource_mut::<LoosePluginInventory>() = inv;
    }

    /// Forward the shared Settings toggle command. Tests verify the
    /// production wiring by going through this path — they never
    /// mutate `LoosePluginInventory`, `LoosePendingBuilds`, or
    /// `LoosePluginReloadRequests` directly.
    pub fn apply_toggle(&mut self, id: &CanonicalId, enable: bool) {
        let world = self.app.world_mut();
        let mut inv = std::mem::take(&mut *world.resource_mut::<LoosePluginInventory>());
        let mut pending = std::mem::take(&mut *world.resource_mut::<LoosePendingBuilds>());
        let mut reloads = std::mem::take(&mut *world.resource_mut::<LoosePluginReloadRequests>());
        apply_loose_plugin_toggle(&mut inv, &mut pending, &mut reloads, id, enable);
        *world.resource_mut::<LoosePluginInventory>() = inv;
        *world.resource_mut::<LoosePendingBuilds>() = pending;
        *world.resource_mut::<LoosePluginReloadRequests>() = reloads;
    }

    pub fn attach_pending_build(&mut self, id: &CanonicalId, rx: Receiver<BuildOutcome>) {
        let world = self.app.world_mut();
        let mut pending = std::mem::take(&mut *world.resource_mut::<LoosePendingBuilds>());
        pending.pending.insert(
            id.clone(),
            PendingBuild {
                receiver: rx,
                revision: Revision(0),
            },
        );
        *world.resource_mut::<LoosePendingBuilds>() = pending;
    }

    pub fn observe_inventory<F: FnOnce(&LoosePluginInventory) -> bool>(
        &self,
        id: &CanonicalId,
        f: F,
    ) -> bool {
        let _ = id;
        f(self.app.world().resource::<LoosePluginInventory>())
    }

    /// Same as `observe_inventory` but returns a value from `f`
    /// instead of a bool.
    pub fn observe_inventory_or<T, F: FnOnce(&LoosePluginInventory) -> T>(
        &self,
        id: &CanonicalId,
        f: F,
    ) -> T {
        let _ = id;
        f(self.app.world().resource::<LoosePluginInventory>())
    }

    pub fn observe_pending<F: FnOnce(&LoosePendingBuilds) -> bool>(&self, f: F) -> bool {
        f(self.app.world().resource::<LoosePendingBuilds>())
    }

    /// Test-only helper that simulates `add_material_shader` minus the
    /// renderer dependency. Goes through the production
    /// `next_custom_material_id` counter so a rollback that uses the
    /// stable `material_id` actually drops the candidate's rows.
    /// Used by the asset-rollback acceptance test to seed prior +
    /// candidate `PendingMaterial` rows in interleaved order.
    pub fn register_test_material(&mut self, slot: usize, gen: u32, id: &str) {
        register_custom_material_for_test(self.app.world_mut(), slot, gen, id);
    }

    /// Diff the world against `before`, then run the production
    /// rollback arm. Use the same `before` snapshot the
    /// transactional loader would have captured before the
    /// candidate's init ran.
    pub fn diff_then_rollback_with_snapshot(
        &mut self,
        before: &renzora_plugin::host::RegistrySnapshot,
        slot: usize,
        proposed_generation: u32,
    ) {
        let mut journal = diff_registrations(self.app.world(), before, slot, proposed_generation);
        apply_journal_rollback(
            self.app.world_mut(),
            &mut journal,
            slot,
            proposed_generation,
        );
    }
}

// Compatibility helper: hand back a fresh `TempDir` for the harness's
// `TestEnv` clone. The original `env` already owns one; the harness
// needs its own so `Harness::drop` doesn't double-free the path.
fn _fresh_workdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

impl BuildServiceDriver {
    pub fn start(env: &TestEnv) -> Self {
        // The SDK the BuildService compiles against is the real
        // `renzora_plugin` crate from the workspace. Pointing the
        // BuildService at the workspace path directly is what lets
        // `cargo generate-lockfile` resolve `renzora_plugin = { path
        // = <sdk>, ... }` cleanly and what lets `cargo build` actually
        // produce a working cdylib for a well-formed source.
        let sdk_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("renzora_plugin");
        let cfg = BuildServiceConfig {
            cache_root: env.cache_root.clone(),
            profile: BuildProfile::Dist,
            sdk_path,
            toolchain_stamp: "test".into(),
            compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
            n_workers: Some(1),
            n_children: Some(1),
            shutdown_deadline: std::time::Duration::from_secs(30),
            required_symbols_by_kind: std::collections::HashMap::from([(
                ArtifactKind::Tier1Plugin,
                vec![b"renzora_plugin_init\0".to_vec()],
            )]),
        };
        let service = BuildService::new(cfg);
        match service {
            Ok(s) => Self { service: s },
            Err(e) => panic!("BuildService::new failed: {e}"),
        }
    }

    pub fn submit_source(
        &self,
        source: Vec<u8>,
        abi: u32,
        stamp: u32,
    ) -> Result<Receiver<BuildOutcome>, renzora_compiler_cache::service::BuildServiceError> {
        self.submit_source_with_identity(
            CanonicalId::parse("engine://test.rs").unwrap(),
            source,
            abi,
            stamp,
        )
    }

    pub fn submit_source_with_identity(
        &self,
        identity: CanonicalId,
        source: Vec<u8>,
        abi: u32,
        stamp: u32,
    ) -> Result<Receiver<BuildOutcome>, renzora_compiler_cache::service::BuildServiceError> {
        let req = BuildRequest {
            identity,
            source_snapshot: Arc::new(source),
            fingerprint_inputs: FingerprintInputs {
                target_triple: default_target_triple(),
                toolchain_stamp: stamp.to_string(),
                sdk_content_hash: [0u8; 32],
                abi_version: abi,
                interface_prefix_hashes: vec![0, 1, 2],
                wrapper_schema: 1,
                manifest_schema: 1,
                lock_resolution: [0u8; 32],
                capabilities: std::collections::BTreeSet::new(),
                profile: BuildProfile::Dist,
                rustflags: Vec::new(),
                panic: PanicStrategy::Abort,
                compiler_service_schema: renzora_compiler_cache::types::COMPILER_SERVICE_SCHEMA,
            },
            target: default_target_triple(),
            artifact_kind: ArtifactKind::Tier1Plugin,
        };
        self.service.submit(req)
    }

    pub fn drain_outcomes(&self) -> Vec<BuildOutcome> {
        // No public API to drain everything; the test framework's
        // `recv_until` is what production code uses.
        Vec::new()
    }

    pub fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs(20)
    }
}
