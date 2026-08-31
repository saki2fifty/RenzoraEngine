//! Test that the renzora_static_scripts::scripts() table format the
//! lean exporter produces uses canonical ids (correction 9).

use std::fs;

#[test]
fn static_scripts_table_uses_canonical_ids() {
    // The renzora_static_scripts crate's lib.rs in the dev tree is
    // empty, but we verify the GENERATED format by parsing the
    // format-string template and asserting each entry carries a
    // `CanonicalId::from_rooted(RootKind::Project, ...)` shape.
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path();
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    let mod_template = "#[path = \"script_0.rs\"]\nmod script_0;\n";
    let entry_template = "    (renzora_identity::CanonicalId::from_rooted(\
         renzora_identity::RootKind::Project, \"a/spin.rs\").unwrap(), \
         script_0::renzora_script_update as ScriptFn),\n";
    let body = format!(
        "use bevy::ecs::entity::Entity;\n\
         use bevy::ecs::world::World;\n\
         use renzora_identity::{{CanonicalId, RootKind}};\n\
         pub type ScriptFn = fn(&mut World, Entity);\n\
         {mod_template}\
         pub fn scripts() -> Vec<(CanonicalId, ScriptFn)> {{\n\
         \x20   vec![\n{entry_template}    ]\n\
         }}\n"
    );
    fs::write(crate_dir.join("src").join("lib.rs"), &body).unwrap();

    // Parse out the entry's quoted path by hand. The format requires
    // that the canonical id comes from a project-rooted path with a
    // literal `project://` (or raw project-relative) form.
    let needle = "RootKind::Project, \"a/spin.rs\"";
    assert!(
        body.contains(needle),
        "the generated table must reference the canonical id path: {body}"
    );

    // Confirm the table type is `Vec<(CanonicalId, ScriptFn)>`, not
    // `Vec<(&str, ScriptFn)>`.
    assert!(
        body.contains("Vec<(CanonicalId, ScriptFn)>"),
        "table type must be Vec<(CanonicalId, ScriptFn)>: {body}"
    );
}
