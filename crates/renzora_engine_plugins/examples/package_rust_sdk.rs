//! Package the source SDK independently of any compiled engine artifacts.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 2 {
        return Err("usage: package_rust_sdk <engine-root> <new-sdk-directory>".into());
    }
    renzora_engine_plugins::packaging::package_rust_sdk(
        std::path::Path::new(&arguments[0]),
        std::path::Path::new(&arguments[1]),
    )?;
    Ok(())
}
