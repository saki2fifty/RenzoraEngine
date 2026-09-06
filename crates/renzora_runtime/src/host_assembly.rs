//! Host-level extension-host assembly — the editor's single point of
//! truth for "is the compiler available, and which `Arc<BuildService>`
//! do Rust scripts and loose plugins submit to?".
//!
//! Two features share one `BuildService`:
//!
//! - `renzora_loose_plugins` (Phase 3) submits `ArtifactKind::Tier1Plugin`.
//! - `renzora_rust_script` (Phase 4) submits `ArtifactKind::Tier1Script`.
//!
//! Both MUST submit to the same instance. Two disjoint services would
//! duplicate cache partitions, miss cross-artifact supersession, and
//! double the worker pool. The host assembly lives here — and only
//! here — so the two feature crates never depend on each other.
//!
//! ## U4-1: explicit compiler-unavailable mode
//!
//! `build_compiler_service` returns either `Available { service }`
//! or `Unavailable { diagnostic }`. The host assembly installs the
//! `Arc` into both consumers' resources when available, and skips
//! both installations when unavailable. There is no permissive
//! fallback: the same diagnostic appears in BOTH resources' absence
//! and there is no "second service" — neither consumer fabricates one
//! when the shared one fails to start. The forced-`Unavailable`
//! test (U4-10) and the tests that drive unavailable behavior
//! depend on this contract.
//!
//! ## U4-2: gate compiler construction to source-modding modes
//!
//! The editor entry point constructs the service; an ordinary
//! shipped runtime does NOT. The headless harness used by tests
//! can construct one explicitly. The `compiler_modding_policy`
//! helper is the single source of truth for the modes that
//! initialize a compiler.
//!
//! ## U4-3: this module owns cross-feature orchestration
//!
//! The Rust-script feature crate (`renzora_rust_script`) used to
//! own this assembly and depend on `renzora_loose_plugins` to
//! provide the loose-host configuration surface. Moving the
//! orchestration here eliminates that cross-feature dependency
//! while preserving the same call chain both editor entry points
//! already used (T4-1).
//!
//! ## U4-5: production assembly used by tests too
//!
//! The same function the editor's `main.rs` calls is what tests
//! call. The harness flavor — `install_extension_host_headless` —
//! skips Bevy plugins the headless `MinimalPlugins` stack cannot
//! support, but the compiler and plugin resources it installs are
//! identical to the editor's. Script-installing headless helpers require the
//! runtime's `scripting` Cargo feature; compiler/loose-host assembly does not.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::prelude::*;

use renzora_compiler_cache::shared::{build_shared_service, SharedBuildServiceConfig};
use renzora_compiler_cache::BuildService;

pub use renzora_loose_plugins::host_plugin::CompilerMode;

/// What kind of session is starting up? Drives whether the host
/// assembly constructs a `BuildService` at all (U4-2).
///
/// Source-modding sessions — `Editor`, and a future
/// `SourceModdingRuntime` for shipped games that allow user mods —
/// construct the shared service. Every other mode
/// (`Runtime`, `Server`, `HostListen`) does not.
///
/// The flag is intentionally split so the host assembly can reject
/// the wrong combination with a single match rather than relying on
/// a runtime check inside `BuildService::new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Editor: full source-modding infrastructure. Constructs the
    /// shared `BuildService` (if the host has an SDK + cache root).
    Editor,
    /// A future game shipped with explicit source-modding support.
    /// Constructs the shared service just like the editor does.
    SourceModdingRuntime,
    /// Ordinary shipped game, listen server, or dedicated server.
    /// Does NOT construct a `BuildService`; loose-plugin source
    /// compilation is disabled; Rust-script source compilation is
    /// disabled. Prebuilt C-ABI cdylibs can still load.
    Runtime,
}

/// Decide whether the given session kind should construct a
/// shared `BuildService`. U4-2 helper used by both editor entry
/// points and the acceptance harness.
pub fn compiler_modding_policy(kind: SessionKind) -> bool {
    match kind {
        SessionKind::Editor | SessionKind::SourceModdingRuntime => true,
        SessionKind::Runtime => false,
    }
}

/// The host-level result of compiler-service construction. U4-1
/// makes this explicit: `Available` carries the production `Arc`
/// both consumers share, `Unavailable` carries the single
/// actionable diagnostic. There is no fallback; there is no
/// "self-owned" path the host accidentally exercises.
#[derive(Clone)]
pub enum CompilerService {
    /// A shared `Arc<BuildService>` ready to install into both
    /// consumers. The same `Arc` is handed to
    /// `RustScriptBuildService` (Bevy resource) and `LooseBuildService`
    /// (Bevy resource); `Arc::ptr_eq` is the proof in U4-9.
    Available { service: Arc<BuildService> },
    /// No shared compiler available. The diagnostic names the
    /// failure (cache root missing, SDK stamp invalid, worker count
    /// invalid, factory-injected failure). Both consumers stay
    /// uninstallable: `RustScriptBuildService` is absent (the
    /// script plugin's `build` skips lifecycle registration), and
    /// `LooseBuildService` is absent (the loose host's drain
    /// systems gate submissions on its presence).
    Unavailable { diagnostic: String },
}

impl CompilerService {
    /// Borrow the production `Arc` when available.
    pub fn as_arc(&self) -> Option<Arc<BuildService>> {
        match self {
            CompilerService::Available { service } => Some(service.clone()),
            CompilerService::Unavailable { .. } => None,
        }
    }

    /// Diagnostic for the unavailable case. Always `None` when
    /// available.
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            CompilerService::Available { .. } => None,
            CompilerService::Unavailable { diagnostic } => Some(diagnostic.as_str()),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, CompilerService::Available { .. })
    }
}

/// A pluggable service factory the host assembly uses to construct
/// the shared `BuildService`. The default implementation calls
/// `renzora_compiler_cache::shared::build_shared_service`; tests
/// substitute `ForcedUnavailableFactory` (U4-10) to assert the
/// unavailable mode end-to-end.
pub trait ServiceFactory: Send {
    fn build(
        &self,
        config: &SharedBuildServiceConfig,
        session_tag: &str,
    ) -> Result<Arc<BuildService>, String>;
}

/// The production factory. Calls the shared module's
/// `build_shared_service`.
pub struct SharedServiceFactory;

impl ServiceFactory for SharedServiceFactory {
    fn build(
        &self,
        config: &SharedBuildServiceConfig,
        session_tag: &str,
    ) -> Result<Arc<BuildService>, String> {
        let tag = session_tag.to_string();
        if !config.sdk_path.is_absolute() || !config.sdk_path.join("Cargo.toml").is_file() {
            return Err(format!(
                "[{tag}] Rust source SDK is missing at {}. Repair the editor installation; the legacy metadata SDK cannot compile live scripts.",
                config.sdk_path.display()
            ));
        }
        build_shared_service(config.clone())
            .map_err(|e| format!("[{tag}] shared BuildService unavailable: {e}"))
    }
}

/// A factory that ALWAYS returns `Unavailable` with a deterministic
/// diagnostic. U4-10: tests inject this so the assembly function's
/// failure path is the only observable outcome.
pub struct ForcedUnavailableFactory {
    pub diagnostic: String,
}

impl ServiceFactory for ForcedUnavailableFactory {
    fn build(
        &self,
        _config: &SharedBuildServiceConfig,
        session_tag: &str,
    ) -> Result<Arc<BuildService>, String> {
        Err(format!(
            "[{}] forced unavailable: {}",
            session_tag, self.diagnostic
        ))
    }
}

/// Build the shared compiler service via `factory`. Returns
/// `CompilerService::Available { service }` on success, otherwise
/// `CompilerService::Unavailable { diagnostic }` with the
/// factory's error string as the actionable diagnostic.
pub fn build_compiler_service<F: ServiceFactory>(
    factory: &F,
    config: SharedBuildServiceConfig,
    session_tag: &str,
) -> CompilerService {
    match factory.build(&config, session_tag) {
        Ok(service) => CompilerService::Available { service },
        Err(diagnostic) => CompilerService::Unavailable { diagnostic },
    }
}

/// Install the shared compiler service into the Bevy app BEFORE
/// `renzora_runtime::add_engine_plugins`. Returns the same `Arc`
/// the loose host should install, so `Arc::ptr_eq` between the
/// two resources holds.
///
/// On `Unavailable`, neither resource is installed; the function
/// returns `None`. Caller logs the diagnostic.
pub fn install_compiler_service(
    app: &mut App,
    service: &CompilerService,
) -> Option<Arc<BuildService>> {
    match service {
        CompilerService::Available { service } => {
            // U4-3: install BOTH resources so the shared `Arc` is the
            // single submission target. The script plugin's `build`
            // promotes `RustScriptSharedService` →
            // `RustScriptBuildService`; the loose plugin host's
            // `build` installs the same `Arc` into
            // `LooseBuildService`. The two consumers therefore wrap
            // the same `Arc` (`Arc::ptr_eq`).
            app.insert_resource(renzora_rust_script::RustScriptSharedService(
                service.clone(),
            ));
            app.insert_resource(renzora_rust_script::RustScriptBuildService(service.clone()));
            Some(service.clone())
        }
        CompilerService::Unavailable { diagnostic: _ } => {
            // No resource is installed. The script plugin's `build`
            // reads the absent `RustScriptBuildService` and skips
            // lifecycle registration; the loose host installs
            // without `LooseBuildService`. Caller logs the
            // diagnostic.
            None
        }
    }
}

/// Configure the loose plugin host to use the same `Arc` (or
/// `Unavailable` mode) the editor assembly produced. The caller
/// MUST hand this off to the loose host before `app.add_plugins`.
pub fn install_compiler_service_into_loose_host(
    loose_host: &mut renzora_loose_plugins::LoosePluginHost,
    service: &CompilerService,
) {
    match service {
        CompilerService::Available { service } => {
            loose_host.inject_shared_build_service(service.clone());
        }
        CompilerService::Unavailable { diagnostic } => {
            // U4-1: explicit unavailable mode. Loose-plugin source
            // compilation is disabled for the session; prebuilt
            // C-ABI cdylibs can still load.
            loose_host.mark_compiler_unavailable(diagnostic.clone());
        }
    }
}

/// Resolve the installed source SDK independently of the process working directory.
pub fn installed_compiler_config(exe_dir: &Path, is_editor: bool) -> SharedBuildServiceConfig {
    SharedBuildServiceConfig {
        cache_root: default_cache_root(Some(exe_dir), is_editor),
        sdk_path: exe_dir.join("rust-sdk/crates/renzora_plugin"),
        ..SharedBuildServiceConfig::default()
    }
}

#[cfg(test)]
mod installation_tests {
    use super::*;

    #[test]
    fn source_sdk_follows_the_executable_not_the_working_directory() {
        let executable = std::env::current_exe().expect("test executable");
        let root = executable.parent().expect("executable directory");
        let config = installed_compiler_config(root, true);
        assert!(config.sdk_path.is_absolute());
        assert_eq!(config.sdk_path, root.join("rust-sdk/crates/renzora_plugin"));
        assert_eq!(config.cache_root, default_cache_root(Some(root), true));
        assert!(!config.cache_root.starts_with(root));
        let moved = installed_compiler_config(&root.join("relocated"), true);
        assert_ne!(config.sdk_path, moved.sdk_path);
        assert_ne!(config.cache_root, moved.cache_root);
    }

    #[test]
    fn missing_source_sdk_reports_unavailable_before_starting_workers() {
        let config = SharedBuildServiceConfig::default();
        let result = SharedServiceFactory.build(&config, "missing SDK test");
        assert!(matches!(result, Err(message) if message.contains("Rust source SDK is missing")));
    }
}

/// Per-user compiler storage; never write into an installed executable directory.
pub fn default_cache_root(exe_dir: Option<&Path>, _is_editor: bool) -> PathBuf {
    let executable = std::env::current_exe().ok();
    let installation = exe_dir.or_else(|| executable.as_deref().and_then(Path::parent));
    let identity = blake3::hash(
        installation
            .unwrap_or_else(|| Path::new("unknown-installation"))
            .as_os_str()
            .as_encoded_bytes(),
    )
    .to_hex()
    .to_string();
    // On unusual hosts without a user cache location, the OS temporary
    // directory is preferable to mutating (or failing in) Program Files.
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("renzora/compiler")
        .join(identity)
}

/// Configuration bundle for an editor / runtime initialization.
///
/// The editor entry points build this from CLI flags + project
/// config; tests build it from explicit arguments. The factory is
/// `SharedServiceFactory` by default; U4-10 tests substitute
/// `ForcedUnavailableFactory`.
#[derive(Clone)]
pub struct ExtensionHostConfig {
    pub session: SessionKind,
    pub session_tag: String,
    pub compiler_config: SharedBuildServiceConfig,
    pub plugins_dir: PathBuf,
    pub disabled_plugin_ids: Vec<String>,
    pub trusted_plugin_ids: Vec<String>,
}

impl ExtensionHostConfig {
    /// Production `ExtensionHostConfig` for a Linux desktop editor
    /// invocation; the cache root, SDK path, and disabled / trusted
    /// plugin lists are filled in by the caller. The factory is
    /// `SharedServiceFactory`.
    pub fn new_editor(
        exe_dir: Option<&Path>,
        session_tag: impl Into<String>,
        compiler_config: SharedBuildServiceConfig,
        plugins_dir: PathBuf,
        disabled_plugin_ids: Vec<String>,
        trusted_plugin_ids: Vec<String>,
    ) -> Self {
        let mut compiler_config = compiler_config;
        compiler_config.cache_root = default_cache_root(exe_dir, true);
        Self {
            session: SessionKind::Editor,
            session_tag: session_tag.into(),
            compiler_config,
            plugins_dir,
            disabled_plugin_ids,
            trusted_plugin_ids,
        }
    }

    /// Production `ExtensionHostConfig` for a runtime ship that
    /// forbids source compilation. `compiler_config` is preserved
    /// for layout but ignored because `compiler_modding_policy` is
    /// `false` for `SessionKind::Runtime`.
    pub fn new_runtime(exe_dir: Option<&Path>, plugins_dir: PathBuf) -> Self {
        let config = SharedBuildServiceConfig {
            cache_root: default_cache_root(exe_dir, false),
            ..SharedBuildServiceConfig::default()
        };
        Self {
            session: SessionKind::Runtime,
            session_tag: "runtime".to_string(),
            compiler_config: config,
            plugins_dir,
            disabled_plugin_ids: Vec::new(),
            trusted_plugin_ids: Vec::new(),
        }
    }
}

/// Construct the loose plugin host for the given configuration.
/// The caller is responsible for `app.add_plugins(loose_host)`; this
/// helper only constructs the value.
pub fn build_loose_host(
    config: &ExtensionHostConfig,
    compiler_mode: CompilerMode,
) -> renzora_loose_plugins::LoosePluginHost {
    let mut host = renzora_loose_plugins::LoosePluginHost::editor_with_trust(
        &config.plugins_dir,
        config.disabled_plugin_ids.clone(),
        config.trusted_plugin_ids.clone(),
    );
    host.compiler_mode = compiler_mode;
    host
}

/// Result of [`install_extension_host`]: the loose-host value the
/// caller adds with `app.add_plugins`, plus the optional `Arc`
/// for downstream `Arc::ptr_eq` assertions.
pub struct InstalledExtensionHost {
    pub loose_host: renzora_loose_plugins::LoosePluginHost,
    pub shared_arc: Option<Arc<BuildService>>,
}

/// Top-level helper that runs U4-3 + U4-5 orchestration for an
/// editor session: build the compiler service, install it into the
/// Bevy app, and hand a configured loose host back to the caller.
///
/// U4-5: the same function the editor entry points call is what
/// the acceptance harness calls. Both paths therefore see the
/// same `Arc` and the same loose-host configuration.
pub fn install_extension_host<F: ServiceFactory>(
    app: &mut App,
    config: &ExtensionHostConfig,
    factory: &F,
) -> InstalledExtensionHost {
    let compiler_service =
        build_compiler_service(factory, config.compiler_config.clone(), &config.session_tag);

    assemble_extension_host(app, config, &compiler_service)
}

/// Install a previously resolved compiler service and construct the
/// corresponding loose host. Production entry points use this after
/// deciding whether their session permits source compilation; tests
/// inject a deterministic service through the same boundary.
pub fn assemble_extension_host(
    app: &mut App,
    config: &ExtensionHostConfig,
    compiler_service: &CompilerService,
) -> InstalledExtensionHost {
    let shared_arc = install_compiler_service(app, compiler_service);
    let compiler_mode = match compiler_service {
        CompilerService::Available { service } => CompilerMode::Shared(service.clone()),
        CompilerService::Unavailable { diagnostic } => CompilerMode::Unavailable {
            diagnostic: diagnostic.clone(),
        },
    };
    InstalledExtensionHost {
        loose_host: build_loose_host(config, compiler_mode),
        shared_arc,
    }
}

/// Headless variant the acceptance harness uses. Skips Bevy plugins
/// the headless `MinimalPlugins` stack cannot support but
/// installs the same compiler resources and the same loose
/// plugin host. U4-5: the harness does NOT add `RuntimePlugin`
/// (which requires editor-only resources); it does add
/// `ScriptingPlugin` and `RustScriptPlugin` so the registered
/// schedules can flow.
///
/// Available with the `scripting` feature (enabled by default).
#[cfg(feature = "scripting")]
pub fn install_extension_host_headless<F: ServiceFactory>(
    app: &mut App,
    config: &ExtensionHostConfig,
    factory: &F,
    scripts_folder: PathBuf,
) -> InstalledExtensionHost {
    let installed = install_extension_host(app, config, factory);
    // Headless scripts folder for `ScriptingPlugin`. Production
    // editor sessions wire this through the editor's project root;
    // the test harness uses the tempdir it set up.
    app.add_plugins(renzora_scripting::ScriptingPlugin::new().with_scripts_folder(scripts_folder));
    // U4-5: install `RustScriptPlugin` through the production
    // pathway. Tests cannot run the generated
    // `add_runtime_plugins` list because `MinimalPlugins` doesn't
    // expose the editor-only resources; this is the documented
    // headless path for headless tooling/tests.
    app.add_plugins(renzora_rust_script::RustScriptPlugin);
    installed
}

/// Headless assembly using an already-created service. This is the
/// deterministic test entry point for the same host boundary used by
/// production; it differs only in the Bevy plugins needed by a
/// `MinimalPlugins` application.
///
/// Available with the `scripting` feature (enabled by default).
#[cfg(feature = "scripting")]
pub fn assemble_extension_host_headless(
    app: &mut App,
    config: &ExtensionHostConfig,
    compiler_service: &CompilerService,
    scripts_folder: PathBuf,
) -> InstalledExtensionHost {
    let installed = assemble_extension_host(app, config, compiler_service);
    app.add_plugins(renzora_scripting::ScriptingPlugin::new().with_scripts_folder(scripts_folder));
    app.add_plugins(renzora_rust_script::RustScriptPlugin);
    installed
}
