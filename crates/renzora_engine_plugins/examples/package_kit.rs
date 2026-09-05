//! Release packaging entry point; dependency vendoring is a separate pinned step.

use std::path::PathBuf;

use renzora_engine_plugins::packaging::{package_build_kit, BuildKitPackageInputs};
use renzora_engine_plugins::BuildKitRequirement;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 6 {
        return Err("usage: package_kit <engine-root> <vendor-root> <runtime-files> <new-kit-path> <engine-build> <target>".into());
    }
    let rustc = std::process::Command::new("rustc").arg("-vV").output()?;
    if !rustc.status.success() {
        return Err("rustc identity lookup failed".into());
    }
    let inputs = BuildKitPackageInputs {
        engine_root: PathBuf::from(&arguments[0]),
        vendor_root: PathBuf::from(&arguments[1]),
        runtime_root: PathBuf::from(&arguments[2]),
        destination: PathBuf::from(&arguments[3]),
        requirement: BuildKitRequirement {
            engine_build: arguments[4]
                .to_str()
                .ok_or("engine build must be UTF-8")?
                .into(),
            target: arguments[5].to_str().ok_or("target must be UTF-8")?.into(),
            profile: "dist".into(),
            toolchain_stamp: String::from_utf8(rustc.stdout)?,
        },
    };
    let started = std::time::Instant::now();
    let manifest = package_build_kit(&inputs)?;
    let bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
    println!(
        "kit={} files={} bytes={} elapsed_seconds={:.3}",
        manifest.content_hash,
        manifest.files.len(),
        bytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
