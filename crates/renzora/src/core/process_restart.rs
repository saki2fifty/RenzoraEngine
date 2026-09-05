//! Shared process launch boundary for editor replacement.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Child, Command};

/// Launch an explicitly selected executable without exiting the current process.
///
/// Arguments are passed directly to the program, never through a shell. The
/// caller owns the child and must wait for startup acknowledgement before
/// exiting or marking a replacement known-good. A successful spawn alone does
/// not establish successful editor startup.
pub fn spawn_replacement_process<I, S>(executable: &Path, arguments: I) -> io::Result<Child>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    replacement_command(executable, arguments)?.spawn()
}

fn replacement_command<I, S>(executable: &Path, arguments: I) -> io::Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // A restart must never resolve another executable from PATH or a changed
    // working directory. Generation/stamp verification belongs to the caller.
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "replacement executable must have an absolute path",
        ));
    }
    let mut command = Command::new(executable);
    command.args(arguments);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn rejects_path_lookup_and_relative_executables() {
        for path in ["renzora-editor", "./renzora-editor", "../renzora-editor"] {
            let error = spawn_replacement_process(Path::new(path), ["--editor"])
                .expect_err("relative launch must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn preserves_literal_arguments_without_shell_interpretation() {
        let executable = std::env::current_exe().expect("test executable");
        let arguments = [
            OsString::from("project with spaces"),
            OsString::from("$(not-a-command); & | \"quoted\""),
            OsString::from(""),
        ];
        let command = replacement_command(&executable, &arguments).expect("command");
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            arguments
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
        assert!(command.get_current_dir().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;
        let executable = std::env::current_exe().expect("test executable");
        let argument = OsString::from_vec(vec![b'p', 0xff]);
        let command = replacement_command(&executable, [&argument]).expect("command");
        assert_eq!(command.get_args().next(), Some(argument.as_os_str()));
    }

    #[test]
    fn spawn_failure_returns_to_the_caller() {
        let invalid = std::env::current_exe()
            .expect("test executable")
            .join("not-an-executable");
        assert!(spawn_replacement_process(&invalid, std::iter::empty::<OsString>()).is_err());
    }

    #[test]
    fn launches_explicit_child_and_retains_its_handle() {
        let executable = std::env::current_exe().expect("test executable");
        let mut child = spawn_replacement_process(
            &executable,
            [
                "--exact",
                "core::process_restart::tests::replacement_child",
                "--ignored",
            ],
        )
        .expect("launch child test process");
        assert!(child.wait().expect("reap child").success());
    }

    #[test]
    #[ignore = "invoked only by the replacement process test"]
    fn replacement_child() {}
}
