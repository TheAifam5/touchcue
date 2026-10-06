//! Platform sources of security key touch requests.

pub mod descriptor;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod uevent;
pub mod vendor;

/// Failure of a detector to start, run or stop.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error("{context}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[cfg(target_os = "linux")]
    #[error("detector task failed")]
    Join(#[from] tokio::task::JoinError),
    /// The semaphore that serializes sysfs scans was closed.
    #[cfg(target_os = "linux")]
    #[error("sysfs scan slot closed")]
    ScanSlotClosed,
}
