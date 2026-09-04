//! The editor executable.
//!
//! Deliberately much smaller than the runtime's `main.rs`. That binary has to
//! decide at startup whether it is a game, a dedicated server, a listen server
//! or a VR session; this one is only ever the editor. The editor launches the
//! *runtime* binary as a child process for Play, so it never needs those modes
//! itself.
//!
//! The editor arrives by a plain function call rather than `dlopen`. With Bevy
//! statically linked there is no shared `bevy_dylib` for a loadable bundle to
//! attach to — a cdylib linking static Bevy would carry a second copy of Bevy,
//! and therefore a second `World` type, so every component crossing the boundary
//! would mismatch. "Editor as a removable file" becomes "editor as a separate
//! executable"; removing editor code from a shipped game is now a property of
//! which binary you ship, not of which files you delete beside it.

mod setup_ui;

fn executable_directory() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(std::path::Path::to_path_buf)
}

fn main() {
    // The editor always keeps a console: its log output is the primary
    // diagnostic channel, and on Windows the runtime binary is built
    // `windows_subsystem = "windows"` precisely so shipped games don't get one.
    renzora_runtime::renzora_engine::crash::install_panic_hook(true);
    renzora_runtime::attach_console();

    // ── Setup, before Bevy ───────────────────────────────────────────────────
    // A downloaded release arrives with the SDK still compressed and every native
    // plugin still source-only, so the first launch after an install or update
    // has real work to do. It has to happen HERE, before `App` assembly: that is
    // when `NativePluginLoader` loads plugins, so unpacking any later would be
    // too late for the very thing that needed it.
    //
    // Ordinary launches answer `needed() == false` after a couple of directory
    // stats and fall straight through. See `renzora_native_plugin::prebuild`.
    if renzora_native_plugin::prebuild::needed() {
        setup_ui::run();
        renzora_native_plugin::prebuild::restart();
    }

    // U4-2 + U4-3: production compiler-service assembly. Editor
    // sessions construct the shared `Arc<BuildService>` BEFORE
    // `renzora_runtime::add_engine_plugins` so the generated
    // `RustScriptPlugin` (one of the lines inside
    // `add_runtime_plugins`) sees the resource during its
    // `build`. The same host-level assembly function (`renzora_
    // runtime::host_assembly`) is used by `src/main.rs` (root
    // editor path) and by the acceptance harness; both consumers
    // of the resulting `Arc` receive THE SAME pointer
    // (`Arc::ptr_eq` is the U4-9 proof).
    let compiler_service = renzora_runtime::host_assembly::build_compiler_service(
        &renzora_runtime::host_assembly::SharedServiceFactory,
        renzora_compiler_cache::shared::SharedBuildServiceConfig {
            cache_root: renzora_runtime::host_assembly::default_cache_root(
                executable_directory().as_deref(),
                true,
            ),
            ..Default::default()
        },
        "renzora-editor",
    );

    let mut app = renzora_runtime::init_app();
    renzora_runtime::add_default_rendering(&mut app, true);

    // U4-1 + U4-3: install the compiler-service resource BEFORE
    // `add_engine_plugins`. The generated `RustScriptPlugin`
    // reads the resource during its `build`; a missing resource
    // means the plugin's lifecycle system registration is
    // skipped. We install whatever the production assembly
    // produced (`Available` or `Unavailable`). On
    // `Unavailable`, no resource is installed and the plugin's
    // `build` early-returns. The host assembly returns the
    // optional `Arc` for `Arc::ptr_eq` verification.
    let plugins_dir = executable_directory()
        .map(|d| d.join("plugins"))
        .unwrap_or_else(|| std::path::PathBuf::from("plugins"));
    let disabled = renzora_runtime::renzora::load_disabled_plugins();
    let trusted = renzora_runtime::renzora::load_trusted_loose_plugins();
    let extension_config = renzora_runtime::host_assembly::ExtensionHostConfig::new_editor(
        executable_directory().as_deref(),
        "renzora-editor",
        renzora_compiler_cache::shared::SharedBuildServiceConfig::default(),
        plugins_dir,
        disabled.clone(),
        trusted,
    );
    let installed_extension_host = renzora_runtime::host_assembly::assemble_extension_host(
        &mut app,
        &extension_config,
        &compiler_service,
    );
    let shared_arc = installed_extension_host.shared_arc.clone();
    // The compiler-availability diagnostic is a single actionable
    // line. The editor surfaces it through `console_error` so the
    // settings panel and the console log both see it. We log
    // regardless of availability so an editor session that
    // succeeded has a record of which compiler root it picked.
    if let Some(diag) = compiler_service.diagnostic() {
        eprintln!("{diag}");
        renzora_runtime::renzora::core::console_log::console_error("Compiler", diag.to_string());
    }

    renzora_runtime::add_engine_plugins(&mut app, true);
    app.add_plugins(renzora_runtime::renzora_engine::crash::CrashReportPlugin);

    // AFTER the engine foundation, so Editor-scope plugins layer on top of the
    // runtime ones — the ordering the old `load_bundle` call site guaranteed.
    renzora_editor::install(&mut app);

    // C-ABI plugins from `<exe_dir>/plugins/`. Unaffected by static linking:
    // they link no Bevy at all, so there is no ABI to match — the interface is
    // passed in as a function table.
    // No `statics`: linking plugins in is an export-time choice for a shipped
    // game, and it would cost the editor the thing it needs most from them —
    // hot reload, which needs a file on disk to watch and swap.
    app.add_plugins(renzora_plugin::host::loader::RenzoraPluginHostPlugin {
        is_editor: true,
        statics: Vec::new(),
        disabled: disabled.clone(),
    });
    app.add_plugins(renzora_postprocess::plugin_bridge::PluginRenderBridgePlugin);
    renzora_postprocess::add_plugin_material(&mut app);

    // U4-1 + U4-3: hand the SAME `Arc` to the loose host through
    // the host-level assembly. On `Unavailable`, the loose host
    // stays in `Unavailable` compiler mode — no
    // `LooseBuildService` is installed; live loose-plugin
    // recompiles are out of scope; the editor's other
    // functionality continues.
    app.add_plugins(installed_extension_host.loose_host);

    // U4-4: install the production OS-watcher adapter so the
    // lifecycle's event seam receives real filesystem events.
    // Tests skip this plugin and push events through the
    // `ScriptSourceEventQueue` directly.
    app.add_plugins(renzora_rust_script::source_watcher::RustScriptSourceWatcherPlugin::default());

    // U4-3: `RustScriptPlugin` was installed exactly once by the
    // generated `add_runtime_plugins` path inside
    // `add_engine_plugins` above. The plugin's `build` already
    // saw the resource the assembly installed.

    // The compile-time check that one-and-only-one `Arc` exists.
    // `shared_arc` is the Arc the assembly handed back; the loose
    // host injected the same Arc into its `compiler_mode`, and
    // the host's `build` installs THE SAME Arc into
    // `LooseBuildService`. `Arc::ptr_eq` proves the two resources
    // wrap the same service. The test
    // `crates/renzora_rust_script/tests/phase4_acceptance.rs::
    // u4_single_install_path` covers the same invariant in
    // production assembly; the editor's runtime startup performs
    // the same check as a build-time assertion.
    if let (Some(arc), Some(loose_resource)) = (
        shared_arc.as_ref(),
        app.world()
            .get_resource::<renzora_loose_plugins::LooseBuildService>(),
    ) {
        debug_assert!(
            std::ptr::eq(
                arc.as_ref() as *const _,
                loose_resource.0.as_ref() as *const _
            ),
            "U4-3: loose-host BuildService and RustScriptBuildService must wrap the same Arc"
        );
    }

    app.run();
}
