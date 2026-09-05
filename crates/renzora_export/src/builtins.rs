//! Built-in export choices and verification of the actual runtime artifact.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use renzora::runtime_capabilities::{RuntimeBuiltinCapabilities, RUNTIME_CAPABILITIES_LEN};

pub(crate) fn defaults() -> HashSet<String> {
    renzora::BUILTIN_RUNTIME_PLUGIN_IDS
        .iter()
        .map(|id| id.to_string())
        .collect()
}

/// Canonical ordering makes packed configuration and build inputs repeatable.
pub(crate) fn selected(wanted: &HashSet<String>, disabled_features: &[String]) -> Vec<String> {
    renzora::BUILTIN_RUNTIME_PLUGIN_IDS
        .iter()
        .filter(|id| {
            wanted.contains(**id) && !disabled_features.iter().any(|disabled| disabled == **id)
        })
        .map(|id| (*id).to_owned())
        .collect()
}

pub(crate) fn omitted(selected: &[String]) -> Vec<String> {
    renzora::BUILTIN_RUNTIME_PLUGIN_IDS
        .iter()
        .filter(|id| !selected.iter().any(|selected| selected == **id))
        .map(|id| (*id).to_owned())
        .collect()
}

// Bounded memory and overlap preserve records crossing a read boundary. Equal
// duplicates are harmless (e.g. a universal binary); conflicting slices aren't.
fn scan(mut reader: impl Read) -> Result<(Option<RuntimeBuiltinCapabilities>, bool), String> {
    let mut bytes = vec![0; 64 * 1024 + RUNTIME_CAPABILITIES_LEN];
    let mut carry = 0;
    let mut found = None;
    let mut packed = false;
    loop {
        let read = reader
            .read(&mut bytes[carry..])
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        let end = carry + read;
        packed |= bytes[..end].windows(4).any(|window| window == b"UPX!");
        for window in bytes[..end].windows(RUNTIME_CAPABILITIES_LEN) {
            if let Some(capabilities) = RuntimeBuiltinCapabilities::decode(window) {
                if found.is_some_and(|previous| previous != capabilities) {
                    return Err("Runtime contains conflicting built-in capability records".into());
                }
                found = Some(capabilities);
            }
        }
        carry = end.min(RUNTIME_CAPABILITIES_LEN - 1);
        bytes.copy_within(end - carry..end, 0);
    }
    Ok((found, packed))
}

pub(crate) fn scan_file(path: &Path) -> Result<(Option<RuntimeBuiltinCapabilities>, bool), String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("Read {}: {error}", path.display()))?;
    let mut signature = [0; 4];
    let read = file
        .read(&mut signature)
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    if read == 4 && signature == *b"PK\x03\x04" {
        // Mobile and web templates wrap their runtime in ZIP/APK. Inspect the
        // decompressed code entries, never assets that might contain old bytes.
        let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
        let mut found = None;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|error| error.to_string())?;
            if !entry.is_file() {
                continue;
            }
            // iOS application executables have no required filename suffix.
            // Inspect code formats, not names, and ignore ordinary assets.
            let mut header = [0; 8];
            match entry.read_exact(&mut header) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => continue,
                Err(error) => return Err(error.to_string()),
            }
            let code = matches!(
                &header[..4],
                b"\x7fELF"
                    | b"\0asm"
                    | b"\xfe\xed\xfa\xce"
                    | b"\xce\xfa\xed\xfe"
                    | b"\xfe\xed\xfa\xcf"
                    | b"\xcf\xfa\xed\xfe"
                    | b"\xca\xfe\xba\xbe"
                    | b"\xbe\xba\xfe\xca"
                    | b"\xca\xfe\xba\xbf"
                    | b"\xbf\xba\xfe\xca"
            ) || &header == b"!<arch>\n";
            if !code {
                continue;
            }
            if let Some(record) = scan(std::io::Cursor::new(header).chain(entry))?.0 {
                if found.is_some_and(|previous| previous != record) {
                    return Err("Template contains conflicting runtime capability records".into());
                }
                found = Some(record);
            }
        }
        return Ok((found, false));
    }
    scan(file)
}

/// Check metadata without executing the runtime or changing the template.
pub(crate) fn verify(
    path: &Path,
    selected: &[String],
) -> Result<RuntimeBuiltinCapabilities, String> {
    let (record, packed) = scan_file(path)?;
    let record = match record {
        Some(record) => Some(record),
        None if packed => {
            let upx = crate::upx::locate().ok_or(
                "This runtime is UPX-compressed. Install UPX or set RENZORA_UPX to verify its built-in features, or use an unpacked matching template",
            )?;
            let temp = tempfile::tempdir().map_err(|error| error.to_string())?;
            let copy = temp.path().join(path.file_name().ok_or("Runtime has no filename")?);
            std::fs::copy(path, &copy).map_err(|error| error.to_string())?;
            let output = std::process::Command::new(upx).args(["-d", "-q"])
                .arg(&copy).output().map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!("Cannot inspect packed runtime: {}", String::from_utf8_lossy(&output.stderr)));
            }
            scan_file(&copy)?.0
        }
        None => None,
    }.ok_or("This runtime template predates built-in plugin selection or has unsupported metadata. Rebuild/download a matching runtime template before exporting")?;
    let missing: Vec<_> = selected.iter().filter(|id| !record.contains(id)).collect();
    if !missing.is_empty() {
        return Err(format!("Runtime template does not contain selected built-ins: {}. Choose a matching full template or rebuild with these features", missing.iter().map(|id| id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    Ok(record)
}

pub(crate) fn verify_lean(path: &Path, selected: &[String]) -> Result<(), String> {
    let record = verify(path, selected)?;
    let retained: Vec<_> = omitted(selected)
        .into_iter()
        .filter(|id| record.contains(id))
        .collect();
    if !retained.is_empty() {
        return Err(format!(
            "Lean build retained omitted built-ins: {}",
            retained.join(", ")
        ));
    }
    Ok(())
}

/// Both client and server packages use the same serialized project contract.
pub(crate) fn write_project_config(
    packer: &mut renzora_rpak::RpakPacker,
    config: &renzora::ProjectConfig,
) -> Result<(), String> {
    let bytes = toml::to_string_pretty(config)
        .map_err(|error| error.to_string())?
        .into_bytes();
    packer.add_file("project.toml", bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a newly built installed-kit runtime with the acceptance plugin, UPX and a display"]
    fn installed_runtime_executes_separate_appended_and_packed_selection() {
        let runtime =
            std::path::PathBuf::from(std::env::var_os("RENZORA_PHASE6_RUNTIME").expect("runtime"));
        let all: Vec<_> = renzora::BUILTIN_RUNTIME_PLUGIN_IDS
            .iter()
            .map(|id| id.to_string())
            .collect();
        verify(&runtime, &all).expect("actual runtime capabilities");
        let temp = tempfile::tempdir().unwrap();
        let packed = temp.path().join("packed-runtime");
        std::fs::copy(&runtime, &packed).unwrap();
        let upx = crate::upx::locate().expect("UPX is required for this acceptance test");
        crate::upx::compress_in_place(&upx, &packed).expect("compress real runtime");
        assert!(
            scan_file(&packed).unwrap().0.is_some(),
            "packed metadata remains readable without UPX"
        );
        verify(&packed, &all).expect("inspect compressed runtime without executing it");
        for (lane, source, appended) in [
            ("all-builtins", &runtime, false),
            ("separate", &runtime, false),
            ("appended", &runtime, true),
            ("packed", &packed, true),
        ] {
            let output = temp.path().join(lane);
            std::fs::create_dir(&output).unwrap();
            let config = renzora::ProjectConfig {
                builtin_runtime_plugins: Some(if lane == "all-builtins" {
                    all.clone()
                } else {
                    vec!["spline".into()]
                }),
                ..Default::default()
            };
            let mut packer = renzora_rpak::RpakPacker::new();
            write_project_config(&mut packer, &config).unwrap();
            packer.add_file("scenes/main.bsn", b"// renzora interim bsn v1\n".to_vec());
            let binary = output.join(if cfg!(windows) { "game.exe" } else { "game" });
            if appended {
                packer.append_to_binary(source, &binary, 1).unwrap();
            } else {
                packer.write_to_file(&output.join("game.rpak"), 1).unwrap();
                std::fs::copy(source, &binary).unwrap();
            }
            let log = std::fs::File::create(output.join("runtime.log")).unwrap();
            let mut child = std::process::Command::new(&binary)
                .current_dir(&output)
                .env("RENZORA_PHASE5_PROBE_OUTPUT", &output)
                .env("RENZORA_PHASE5_PROBE_FRAME_LIMIT", "10")
                .env("RENZORA_PHASE6_BUILTIN_PROBE", "1")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .expect("launch packaged game without repairing permissions");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{lane}: runtime timed out: {}",
                        std::fs::read_to_string(output.join("runtime.log")).unwrap()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            };
            assert!(
                status.success(),
                "{lane}: {}",
                std::fs::read_to_string(output.join("runtime.log")).unwrap()
            );
            let states = std::fs::read_to_string(output.join("builtin-probe"))
                .expect("real startup inventory");
            for id in renzora::BUILTIN_RUNTIME_PLUGIN_IDS {
                let expected = if *id == "spline" || lane == "all-builtins" {
                    "Loaded"
                } else {
                    "Disabled"
                };
                assert!(
                    states
                        .lines()
                        .any(|line| line == format!("{id}={expected}")),
                    "{lane}: {states}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(output.join("runtime-probe")).unwrap(),
                "42"
            );
            assert!(!output.join("editor-probe").exists());
            eprintln!("{lane}: real runtime executed with the expected built-ins");
        }
    }

    #[test]
    fn lean_validation_rejects_code_that_should_have_been_omitted() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("runtime");
        std::fs::write(&binary, RuntimeBuiltinCapabilities::new([true; 8]).encode()).unwrap();
        assert!(verify(&binary, &[]).is_ok());
        assert!(verify_lean(&binary, &[]).unwrap_err().contains("retained"));
        std::fs::write(
            &binary,
            RuntimeBuiltinCapabilities::new([false; 8]).encode(),
        )
        .unwrap();
        assert!(verify_lean(&binary, &[]).is_ok());
    }

    #[test]
    fn separate_and_appended_packages_keep_explicit_selection() {
        let temp = tempfile::tempdir().unwrap();
        let config = renzora::ProjectConfig {
            builtin_runtime_plugins: Some(vec!["spline".into()]),
            ..Default::default()
        };
        let mut packer = renzora_rpak::RpakPacker::new();
        write_project_config(&mut packer, &config).unwrap();
        let archive = temp.path().join("game.rpak");
        packer.write_to_file(&archive, 1).unwrap();
        let separate = renzora_rpak::RpakArchive::from_file(&archive).unwrap();
        let binary = temp.path().join("runtime");
        std::fs::write(&binary, RuntimeBuiltinCapabilities::new([true; 8]).encode()).unwrap();
        let mut packer = renzora_rpak::RpakPacker::new();
        write_project_config(&mut packer, &config).unwrap();
        let game = temp.path().join("game");
        packer.append_to_binary(&binary, &game, 1).unwrap();
        let appended = renzora_rpak::RpakArchive::from_binary(&game)
            .unwrap()
            .unwrap();
        for archive in [separate, appended] {
            let bytes = archive.get("project.toml").unwrap();
            let actual: renzora::ProjectConfig =
                toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
            assert_eq!(
                actual.builtin_runtime_plugins,
                config.builtin_runtime_plugins
            );
        }
    }

    #[test]
    fn choices_are_canonical_and_do_not_reenable_removed_stacks() {
        let all = renzora::BUILTIN_RUNTIME_PLUGIN_IDS
            .iter()
            .map(|id| id.to_string())
            .collect();
        let keep = selected(&all, &["clouds".into(), "text3d".into()]);
        assert_eq!(omitted(&keep), ["text3d", "clouds"]);
        assert_eq!(selected(&HashSet::new(), &[]), Vec::<String>::new());
    }

    #[test]
    fn scanner_handles_chunk_boundaries_duplicates_and_conflicts() {
        let full = RuntimeBuiltinCapabilities::new([true; 8]);
        for offset in [0, 65535, 65536, 65536 + RUNTIME_CAPABILITIES_LEN - 1] {
            let mut bytes = vec![7; offset];
            bytes.extend_from_slice(&full.encode());
            bytes.extend_from_slice(&full.encode());
            assert_eq!(scan(bytes.as_slice()).unwrap(), (Some(full), false));
            bytes.extend_from_slice(&RuntimeBuiltinCapabilities::new([false; 8]).encode());
            assert!(scan(bytes.as_slice()).is_err());
        }
    }

    #[test]
    fn older_or_incomplete_templates_are_refused_even_for_empty_selection() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("game");
        std::fs::write(&binary, b"old runtime").unwrap();
        assert!(verify(&binary, &[]).unwrap_err().contains("predates"));
        std::fs::write(
            &binary,
            RuntimeBuiltinCapabilities::new([false; 8]).encode(),
        )
        .unwrap();
        assert!(verify(&binary, &[]).is_ok());
        assert!(verify(&binary, &["spline".into()])
            .unwrap_err()
            .contains("spline"));
    }

    #[test]
    fn mobile_and_web_zip_templates_inspect_code_not_assets() {
        use std::io::Write;
        for (name, header) in [
            ("lib/runtime.so", b"\x7fELF\0\0\0\0"),
            ("runtime.a", b"!<arch>\n"),
            ("runtime.wasm", b"\0asm\x01\0\0\0"),
            (
                "Payload/RenzoraRuntime.app/RenzoraRuntime",
                b"\xcf\xfa\xed\xfe\0\0\0\0",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("template.zip");
            let mut archive = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            archive.start_file(name, options).unwrap();
            archive.write_all(header).unwrap();
            archive
                .write_all(&RuntimeBuiltinCapabilities::new([true; 8]).encode())
                .unwrap();
            archive
                .start_file("assets/old-record.dat", options)
                .unwrap();
            archive
                .write_all(&RuntimeBuiltinCapabilities::new([false; 8]).encode())
                .unwrap();
            archive.finish().unwrap();
            assert!(verify(&path, &["clouds".into()]).is_ok());
        }
    }
}
