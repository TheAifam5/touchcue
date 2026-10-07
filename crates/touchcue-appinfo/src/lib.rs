//! Resolution of a process id to an application name, id and icon.
//!
//! [`unit`](mod@unit) parses the systemd unit names desktop environments give to
//! launched applications. On Linux, [`linux::Resolver`] finds the processes
//! holding a device node open and attributes each to its application through
//! its cgroup unit or, failing that, its executable, and to the requester
//! among its ancestors.

pub mod unit;

/// Longest identity text kept from untrusted sources, in chars.
const TEXT_MAX: usize = 128;

#[cfg(target_os = "linux")]
pub mod linux;
