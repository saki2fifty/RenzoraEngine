mod backend;
mod command;
mod component;
mod context;
pub mod engine;
pub mod extension;
pub mod get_handler;
pub mod http;
pub mod plugin_backend;
pub mod plugin_bridge;
mod input;
mod plugin;

pub mod api;
pub mod perf;
pub mod resources;
pub mod systems;

#[cfg(test)]
pub(crate) mod test_util;

pub use backend::*;
pub use command::*;
pub use component::*;
pub use context::*;
pub use engine::*;
pub use extension::*;
pub use get_handler::{
    AssetProgressBridge, AssetProgressSnapshot, SceneLoadBridge, SceneLoadSnapshot,
};
pub use input::*;
pub use plugin::*;

/// Starter contents for a new `.lua` script.
///
/// Lives here, beside the hook vocabulary it demonstrates, so the two places
/// that create scripts — the Assets panel's New menu and the hierarchy's
/// right-click Attach — write the same file. The same reason
/// `renzora_blueprint::starter_blueprint_json` lives in the blueprint crate.
///
/// `boilerplate` off gives a bare comment: Lua needs no skeleton to be a valid
/// script, so "minimal" really is almost empty here — unlike Rust, which needs
/// its entry-point macro either way.
pub fn starter_lua(boilerplate: bool) -> String {
    if !boilerplate {
        return "-- New Lua script\n".to_string();
    }
    // Both hooks, and the one thing about transforms that catches everyone: the
    // `position_*` globals are read-only inputs, so moving an entity means
    // calling a function.
    concat!(
        "-- Attached to an entity. The engine calls these hooks; delete the\n",
        "-- ones you don't need.\n",
        "\n",
        "function on_ready()\n",
        "    -- Once, when the entity's scripts start.\n",
        "end\n",
        "\n",
        "function on_update()\n",
        "    -- Every frame. `delta` is seconds since the last one.\n",
        "    --\n",
        "    -- `position_x`, `rotation_y`, … are read-only inputs refreshed each\n",
        "    -- frame — assigning to them does nothing. Move an entity by calling\n",
        "    -- translate() / set_position(), or through physics.\n",
        "    local speed = 5.0\n",
        "    translate(input_x * speed * delta, 0, input_y * speed * delta)\n",
        "end\n",
    )
    .to_string()
}

/// Starter contents for a new C-ABI Rust script.
///
/// Both settings emit a complete declaration; illustrative code is optional.
pub fn starter_rust(boilerplate: bool) -> String {
    let body = if boilerplate {
        "    // Rotate this entity around its vertical axis (command angles are degrees).\n    reply.commands.push(ScriptCommand::Rotate { x: 0.0, y: ctx.frame.time.delta.to_degrees(), z: 0.0 });\n"
    } else {
        "    let _ = (ctx, reply);\n"
    };
    format!(
        "use renzora_plugin::script::*;\n\nfn update(ctx: &Ctx, reply: &mut ScriptReply) -> Result<(), String> {{\n{body}    Ok(())\n}}\n\nrenzora_plugin::rust_script!(update);\n"
    )
}
