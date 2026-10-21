//! Non-dumpable state of the wrapper processes that carry Assuan traffic.

/// Makes this process non-dumpable, so other processes of the same user
/// cannot read its memory or attach to it. The setting is reset on exec.
///
/// # Errors
///
/// Returns the error of `prctl(PR_SET_DUMPABLE)`.
#[cfg(target_os = "linux")]
pub fn forbid() -> Result<(), rustix::io::Errno> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
}
