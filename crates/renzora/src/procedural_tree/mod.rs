//! Shared tree data derived from Affinator's MIT/Apache-2.0 generator.
//! Attribution and licenses are retained in `renzora_procedural_tree`.
//! Runtime generation hooks are installed by the runtime plugin, not this data crate.

pub mod enums;
pub mod settings;

use bevy::prelude::*;
pub use enums::{LeafBillboard, TreeType};
use serde::{Deserialize, Serialize};
pub use settings::TreeMeshSettings;

/// Authored seed and optional per-tree generation settings.
#[derive(Component, Reflect, Clone, Debug, Serialize, Deserialize)]
#[reflect(Component, Default, Serialize, Deserialize)]
#[type_path = "procedural_tree::tree"]
pub struct Tree {
    pub seed: u64,
    pub tree_mesh_settings_override: Option<TreeMeshSettings>,
    // Asset handles remain excluded, matching existing scene behavior.
    #[serde(skip)]
    #[reflect(ignore)]
    pub bark_material_override: Option<MeshMaterial3d<StandardMaterial>>,
    #[serde(skip)]
    #[reflect(ignore)]
    pub leaf_material_override: Option<MeshMaterial3d<StandardMaterial>>,
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            seed: 0,
            tree_mesh_settings_override: Some(TreeMeshSettings::default()),
            bark_material_override: None,
            leaf_material_override: None,
        }
    }
}

/// Link to a generated leaf child; deliberately not registered as scene data.
#[derive(Component, Reflect)]
#[type_path = "procedural_tree::tree"]
pub struct Leaves(pub Entity);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_names_and_nested_settings_survive_the_move() {
        assert_eq!(Tree::type_path(), "procedural_tree::tree::Tree");
        assert_eq!(
            TreeMeshSettings::type_path(),
            "procedural_tree::tree::settings::TreeMeshSettings"
        );
        assert_eq!(
            TreeType::type_path(),
            "procedural_tree::tree::enums::TreeType"
        );
        let tree = Tree {
            seed: 42,
            ..default()
        };
        let serialized = serde_json::to_string(&tree).unwrap();
        assert!(!serialized.contains("material_override"));
        let restored: Tree = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.seed, 42);
        assert_eq!(
            restored.tree_mesh_settings_override,
            tree.tree_mesh_settings_override
        );
    }
}
