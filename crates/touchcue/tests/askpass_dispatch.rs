//! `touchcue-askpass` and `touchcue askpass` reach askpass with the message
//! unparsed.
#![cfg(target_os = "linux")]

use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::process::{Command, Output};

/// Prints its arguments, one per line, in place of a real askpass program.
const FALLBACK: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\"\n";
/// A message that would be a flag if it were parsed.
const MESSAGE: &str = "--help";

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Runs `program` with `args` as a confirmation prompt, answered by the
/// fallback.
fn run(dir: &Path, program: &Path, args: &[&str]) -> Result<Output, TestError> {
    let fallback = dir.join("fallback");
    fs::write(&fallback, FALLBACK)?;
    fs::set_permissions(&fallback, Permissions::from_mode(0o700))?;
    Ok(Command::new(program)
        .args(args)
        .env("SSH_ASKPASS_PROMPT", "confirm")
        .env("TOUCHCUE_ASKPASS_FALLBACK", &fallback)
        .env_remove("TOUCHCUE_LOG")
        .output()?)
}

#[test]
fn askpass_link_passes_the_message_through() -> Result<(), TestError> {
    let dir = tempfile::tempdir()?;
    let link = dir.path().join("touchcue-askpass");
    symlink(env!("CARGO_BIN_EXE_touchcue"), &link)?;
    let output = run(dir.path(), &link, &[MESSAGE])?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"--help\n");
    Ok(())
}

#[test]
fn askpass_subcommand_passes_the_message_through() -> Result<(), TestError> {
    let dir = tempfile::tempdir()?;
    let output = run(
        dir.path(),
        Path::new(env!("CARGO_BIN_EXE_touchcue")),
        &["askpass", MESSAGE],
    )?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"--help\n");
    Ok(())
}
