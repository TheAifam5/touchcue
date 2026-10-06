//! Command-line interface of the touchcue daemon.

pub mod askpass;
pub mod cli;
pub mod gpg;
#[cfg(target_os = "linux")]
pub mod helper_socket;
pub mod scdaemon;
