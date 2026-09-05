//! Preserve built-in plugin artwork in source and installed editor releases.

use std::{fs, io, path::Path};

/// Stage workspace thumbnails at the existing plugin-card paths.
///
/// Built-in crates use `renzora_<id>` or `renzora_<id>_editor`. Only artwork is
/// staged, never a manifest or source that the old native loader could compile.
pub fn stage(engine_root: &Path, output: &Path) -> io::Result<()> {
    let crates = engine_root.join("crates");
    for entry in fs::read_dir(crates)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_prefix("renzora_")) else {
            continue;
        };
        let id = id.strip_suffix("_editor").unwrap_or(id);
        let source = entry.path().join("thumbnail.jpg");
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            return Err(io::Error::other(
                "workspace thumbnail must be a regular file",
            ));
        }
        let plugins = output.join("plugins");
        let directory = plugins.join(id);
        let target = directory.join("thumbnail.jpg");
        for path in [&plugins, &directory, &target] {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(io::Error::other("artwork destination must not be a link"));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        fs::create_dir_all(directory)?;
        fs::copy(source, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_artwork_without_staging_a_loadable_plugin() {
        let root = std::env::temp_dir().join(format!("renzora-artwork-{}", std::process::id()));
        fs::create_dir_all(root.join("crates/renzora_spline")).expect("source directory");
        fs::write(root.join("crates/renzora_spline/thumbnail.jpg"), b"art")
            .expect("source artwork");
        let output = root.join("output");
        stage(&root, &output).expect("stage artwork");
        stage(&root, &output).expect("repeat staging");
        assert_eq!(
            fs::read(output.join("plugins/spline/thumbnail.jpg")).unwrap(),
            b"art"
        );
        assert!(!output.join("plugins/spline/Cargo.toml").exists());
        assert!(!output.join("plugins/spline/src").exists());
        fs::remove_dir_all(root).expect("clean test directory");
    }
}
