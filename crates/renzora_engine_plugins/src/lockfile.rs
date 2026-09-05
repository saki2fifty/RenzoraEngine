//! Preserve installed engine dependency pins while admitting external packages.

use std::collections::BTreeSet;

use serde::Deserialize;

#[derive(Deserialize)]
struct Lockfile {
    version: u32,
    #[serde(default)]
    package: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

/// Verify that resolution only extends the kit's pinned source packages.
/// Local workspace packages may gain edges to newly declared plugins.
pub(crate) fn verify_extension(baseline: &str, resolved: &str) -> Result<(), String> {
    let parse = |text: &str| -> Result<Lockfile, String> {
        let lock: Lockfile = toml::from_str(text).map_err(|error| error.to_string())?;
        if lock.version != 4 {
            return Err("engine kit requires Cargo.lock version 4".into());
        }
        Ok(lock)
    };
    let baseline = parse(baseline)?;
    let resolved = parse(resolved)?;
    let pins = |lock: Lockfile| -> BTreeSet<_> {
        lock.package
            .into_iter()
            .filter_map(|package| {
                package
                    .source
                    .map(|source| (package.name, package.version, source, package.checksum))
            })
            .collect()
    };
    let original = pins(baseline);
    let selected = pins(resolved);
    if let Some((name, version, _, _)) = original.difference(&selected).next() {
        return Err(format!(
            "dependency resolution changed or removed kit pin {name} {version}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "version = 4\n[[package]]\nname = 'engine-dependency'\nversion = '1.2.3'\nsource = 'registry+https://example.invalid/index'\nchecksum = 'pinned'\n";

    #[test]
    fn permits_new_workspace_packages_without_changing_engine_pins() {
        let resolved = format!("{BASE}\n[[package]]\nname = 'external-plugin'\nversion = '0.1.0'\ndependencies = ['engine-dependency']\n");
        verify_extension(BASE, &resolved).expect("local extension");
    }

    #[test]
    fn refuses_updated_replaced_or_removed_engine_dependencies() {
        for changed in [
            BASE.replace("1.2.3", "1.2.4"),
            BASE.replace("pinned", "changed"),
            "version = 4".into(),
        ] {
            assert!(verify_extension(BASE, &changed).is_err());
        }
    }
}
