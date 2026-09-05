//! Shader Editor — code-based shader authoring with multi-language support.

mod code_panel;
pub mod hot_reload;
mod native_compiler_log;
mod native_preview;
mod native_properties;
pub mod preview;

use bevy::prelude::*;
use renzora_shader::backend::ShaderCompileError;
use renzora_shader::file::ShaderFile;

/// Persistent editor state for the shader code editor.
#[derive(Resource)]
pub struct ShaderEditorState {
    /// The shader file currently being edited.
    pub shader_file: ShaderFile,
    /// File path if loaded from / saved to disk.
    pub file_path: Option<String>,
    /// Dirty flag — source has been modified since last save.
    pub is_modified: bool,
    /// Transpiled WGSL before `@param` constant injection.
    /// Stored so param value changes can re-inject without re-transpiling.
    pub base_wgsl: Option<String>,
    /// Last compiled WGSL output (for preview), with `@param` constants injected.
    pub compiled_wgsl: Option<String>,
    /// Whether the compiled shader is compatible with CodeShaderMaterial preview.
    /// Shaders with custom material bindings (textures, samplers) can't preview.
    pub preview_compatible: bool,
    /// Compilation errors (shown in UI).
    pub compile_errors: Vec<ShaderCompileError>,
    /// Whether to auto-compile on every keystroke.
    pub auto_compile: bool,
    /// Which mesh to display in the shader preview.
    pub preview_mesh: preview::PreviewMesh,
}

impl Default for ShaderEditorState {
    fn default() -> Self {
        Self {
            shader_file: ShaderFile::default(),
            file_path: None,
            is_modified: false,
            base_wgsl: None,
            compiled_wgsl: None,
            compile_errors: Vec::new(),
            auto_compile: true,
            preview_compatible: true,
            preview_mesh: preview::PreviewMesh::default(),
        }
    }
}

#[derive(Default)]
pub struct ShaderEditorPlugin;

impl Plugin for ShaderEditorPlugin {
    fn build(&self, app: &mut App) {
        info!("[editor] ShaderEditorPlugin");
        app.init_resource::<ShaderEditorState>();
        app.init_resource::<renzora::EditorUnsavedWork>()
            .add_systems(
                Last,
                report_unsaved_shader.before(renzora::EnginePluginRestartGate),
            );
        app.add_plugins(preview::ShaderPreviewPlugin);
        app.add_plugins(native_preview::NativeShaderPreview);
        app.add_plugins(native_compiler_log::NativeShaderCompilerLog);
        app.add_plugins(native_properties::NativeShaderProperties);
        app.add_plugins(hot_reload::WgslHotReloadPlugin);
    }
}

fn report_unsaved_shader(
    state: Res<ShaderEditorState>,
    mut unsaved: ResMut<renzora::EditorUnsavedWork>,
) {
    if state.is_changed() {
        unsaved.report("shader", usize::from(state.is_modified));
    }
}

renzora::add!(ShaderEditorPlugin, Editor);

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn dirty_shader_blocks_restart_until_saved() {
        let mut app = App::new();
        app.init_resource::<ShaderEditorState>()
            .init_resource::<renzora::EditorUnsavedWork>()
            .add_systems(Last, report_unsaved_shader);
        app.world_mut()
            .resource_mut::<ShaderEditorState>()
            .is_modified = true;
        app.update();
        assert_eq!(
            app.world().resource::<renzora::EditorUnsavedWork>().0["shader"],
            1
        );
        app.world_mut()
            .resource_mut::<ShaderEditorState>()
            .is_modified = false;
        app.update();
        assert!(app
            .world()
            .resource::<renzora::EditorUnsavedWork>()
            .is_empty());
    }
}
