//! Build the engine-side `PluginScriptBackend` that dispatches every
//! `.rs` call through the host-owned [`CompiledScriptBackend`]
//! registry.
//!
//! ## Architecture (F4-1 + S4-1)
//!
//! A compiled script cdylib exports, via the
//! `renzora_plugin::rust_script!` macro, a per-cdylib
//! `unsafe extern "C" fn renzora_plugin_tier1_script_call`. That
//! trampoline lives inside the cdylib's text segment; the host never
//! has a reachable function pointer without holding the cdylib's
//! `Library` open. The host registry maps canonical identity to an
//! `Arc<ScriptGeneration>` whose `ScriptEntry` is the per-cdylib
//! trampoline and whose `Library` keeps the image mapped.
//!
//! The `.rs` `PluginScriptBackend`'s own `entry` is the function
//! below — `rust_script_host_dispatch`. When a call arrives on the
//! per-entity `ScriptComponent`, `PluginScriptBackend::call` reads
//! `ScriptCall::path`, builds the canonical id, and calls the
//! backend's own `entry` with the `ScriptCall`. Our dispatcher:
//!
//! 1. Validates the `ScriptCall` pointer.
//! 2. Reads the canonical id from `call.path`.
//! 3. Resolves the canonical id against the project's root through
//!    the process-global [`RESOLVER`] slot.
//! 4. Looks the id up in the `CompiledScriptBackend` registry.
//! 5. Clones the `Arc<ScriptGeneration>` (this releases the
//!    registry lock and keeps the `Library` alive for the
//!    dispatch).
//! 6. Invokes the per-cdylib entry through the cloned `Arc`.
//! 7. Returns the resulting `ScriptStatus`. A missing identity
//!    produces `UnknownOp` (deterministic, never crashes).
//!
//! The dispatch never calls itself recursively: the per-cdylib
//! entry does not name this function. It does not pass a Rust-ABI
//! function pointer across the dynamic-library boundary.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use renzora_plugin::script::compiled::{CompiledScriptBackend, ScriptGeneration};
use renzora_plugin::sys::{self, ScriptCall, ScriptEntry, ScriptStatus};
use renzora_scripting::plugin_backend::PluginScriptBackend;

use crate::compiled_runtime::resolve_canonical_id_for;
use crate::compiled_runtime::PathResolver;
use crate::CompiledScriptSlot;

/// The descriptor symbol a compiled script cdylib exports, written
/// by the `rust_script!` macro.
pub const SCRIPT_DESCRIPTOR_SYMBOL: &[u8] = b"renzora_plugin_tier1_script_desc\0";

/// The per-cdylib entry symbol a compiled script cdylib exports.
pub const SCRIPT_ENTRY_SYMBOL: &[u8] = b"renzora_plugin_tier1_script_call\0";

/// The host's compiled-script dispatcher.
///
/// # Safety
///
/// `call` must be a valid `*const ScriptCall` for the
/// duration of the call. It is exactly the pointer
/// `PluginScriptBackend::call` constructs and dispatches; the
/// surrounding protocol guarantees lifetime.
pub unsafe extern "C" fn rust_script_host_dispatch(call: *const ScriptCall) -> ScriptStatus {
    if call.is_null() {
        return ScriptStatus::Error;
    }
    let call: &ScriptCall = &*call;

    let path_str = match read_str(call.path) {
        Ok(s) => s,
        Err(_) => return ScriptStatus::UnknownOp,
    };

    let slot = installed_slot();
    let Some(slot) = slot else {
        return ScriptStatus::UnknownOp;
    };
    let resolver = installed_resolver();
    let canonical = match resolve_canonical_id_for(path_str, resolver.as_deref()) {
        Some(c) => c,
        None => return ScriptStatus::UnknownOp,
    };

    let generation: Arc<ScriptGeneration> = match slot.lookup(&canonical) {
        Some(g) => g,
        None => return ScriptStatus::UnknownOp,
    };

    let entry: ScriptEntry = generation.entry;
    let status = (entry)(call);

    // Hold the Arc until the call returns so the Library stays
    // mapped throughout the dispatch.
    drop(generation);

    status
}

/// Read a `StrRef` into a UTF-8 `&str` with the lifetime of the
/// borrowed bytes. Returns `Err(())` if the pointer is null/empty or
/// the bytes are not valid UTF-8.
fn read_str<'a>(s: sys::StrRef) -> Result<&'a str, ()> {
    if s.ptr.is_null() || s.len == 0 {
        return Err(());
    }
    let bytes = unsafe { core::slice::from_raw_parts(s.ptr, s.len) };
    core::str::from_utf8(bytes).map_err(|_| ())
}

/// Process-global slot handle. The host plugin installs this on
/// `build`; the host dispatcher reads it from any thread.
///
/// `OnceLock` is the right tool: the slot is installed once per
/// process, and reads after installation are wait-free.
static INSTALLED_SLOT: OnceLock<Mutex<Option<Arc<CompiledScriptBackend>>>> = OnceLock::new();

fn installed_slot() -> Option<CompiledScriptSlot> {
    let lock = INSTALLED_SLOT.get_or_init(|| Mutex::new(None));
    let guard = lock.lock().ok()?;
    guard.as_ref().map(|arc| CompiledScriptSlot(arc.clone()))
}

/// Borrow the raw `Arc<CompiledScriptBackend>` the dispatcher
/// reads from. Used by `CompiledScriptSlot::resolve_canonical` so
/// it can match the path against the registered ids without
/// requiring a separate copy.
pub fn installed_arc() -> Option<Arc<CompiledScriptBackend>> {
    let lock = INSTALLED_SLOT.get_or_init(|| Mutex::new(None));
    let guard = lock.lock().ok()?;
    guard.as_ref().cloned()
}

/// Install the slot the dispatcher reads from. Idempotent; a second
/// call replaces the previous install.
pub fn install_global_slot(slot: CompiledScriptSlot) {
    let lock = INSTALLED_SLOT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().expect("global slot mutex poisoned");
    *guard = Some(slot.0);
}

/// Test-only: install a slot bypassing the production
/// OnceLock-installed process-global. Tests need this when they
/// substitute their own backend and want the dispatcher to see it.
#[cfg(test)]
pub fn _test_install_global_slot(slot: CompiledScriptSlot) {
    let lock = INSTALLED_SLOT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().expect("global slot mutex poisoned");
    *guard = Some(slot.0);
}

/// Process-global [`PathResolver`] handle. Production callers
/// install one through [`install_global_resolver`] from the
/// lifecycle's `watch` system. Without a resolver, the dispatcher
/// can still resolve already-canonical `project://<rel>` strings
/// but rejects every other path. Tests can install their own
/// resolver through the same hook.
static INSTALLED_RESOLVER: OnceLock<Mutex<Option<Arc<PathResolver>>>> = OnceLock::new();

fn installed_resolver() -> Option<Arc<PathResolver>> {
    let lock = INSTALLED_RESOLVER.get_or_init(|| Mutex::new(None));
    let guard = lock.lock().ok()?;
    guard.as_ref().cloned()
}

/// Install the resolver the dispatcher reads from. Idempotent; a
/// second call replaces the previous install.
pub fn install_global_resolver(resolver: PathResolver) {
    let lock = INSTALLED_RESOLVER.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().expect("global resolver mutex poisoned");
    *guard = Some(Arc::new(resolver));
}

/// Resolve a `path` string the host supplies through
/// `ScriptCall::path` into a canonical id the registry can look
/// up. The host dispatcher uses this; tests may call it directly.
pub fn resolve_canonical_id(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('\\', "/");
    resolve_canonical_id_for(&s, installed_resolver().as_deref())
}

/// Build the engine-side `PluginScriptBackend` the host installs for
/// `.rs`. The backend's `entry` is the production host dispatcher
/// `rust_script_host_dispatch`; per-cdylib entries are reached by
/// looking up `call.path`'s canonical id in the
/// `CompiledScriptBackend` registry the dispatch reads from.
pub fn compile_rust_script_backend() -> Box<dyn renzora_scripting::ScriptBackend> {
    Box::new(PluginScriptBackend::new(
        "Rust (compiled, Tier 1)".to_string(),
        vec!["rs".to_string()],
        rust_script_host_dispatch,
    ))
}

/// Install the resolver the lifecycle owns into the process-global
/// slot. Called by `lifecycle::watch` whenever the active project
/// changes.
pub fn sync_global_resolver(resolver: &PathResolver) {
    install_global_resolver(resolver.clone());
}

// `World` import kept available so other modules can use the
// `sync_global_resolver` helper without importing the type.
#[allow(unused_imports)]
use bevy::prelude::World as _World;
