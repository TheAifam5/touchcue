//! Readers for a Linux procfs tree rooted at a caller-supplied directory.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use touchcue_core::ProcessInfo;
use touchcue_core::text::sanitize;
use tracing::{debug, trace};

use super::bounded::{read_bounded, read_truncated};
use crate::TEXT_MAX;

/// Largest `status`, `stat` or `cgroup` file read, in bytes.
const PROC_FILE_MAX: usize = 64 * 1024;
/// Longest `comm` kept, in bytes.
const COMM_MAX: usize = 64;
/// Longest `cmdline` read, in bytes, and kept, in chars.
pub const CMDLINE_MAX: usize = 4096;

/// Returns the pids with an open descriptor on `target`, sorted and deduplicated, each with its [`start_time`].
///
/// The start time is read before the descriptors, so a pid reused during the
/// scan carries the earlier process's start time and fails a later check.
/// When `target` is a character device, a descriptor matches if its link
/// text is under `/dev/` and it refers to a character device with the same
/// device number; otherwise the link text must equal `target`. `exclude` is
/// left out of the scan. Processes whose descriptors cannot be read, because
/// they belong to another user or exit mid-scan, are skipped.
#[must_use]
pub fn pids_holding(proc_root: &Path, target: &Path, exclude: Option<u32>) -> Vec<(u32, u64)> {
    scan(proc_root, &Target::of(target), exclude)
}

/// Returns the pids with an open descriptor on any of the sockets with
/// inode numbers `inodes`, like [`pids_holding`], in one pass over `proc_root`.
#[must_use]
pub fn pids_holding_sockets(
    proc_root: &Path,
    inodes: &[u32],
    exclude: Option<u32>,
) -> Vec<(u32, u64)> {
    if inodes.is_empty() {
        return Vec::new();
    }
    scan(proc_root, &Target::Sockets(inodes), exclude)
}

fn scan(proc_root: &Path, target: &Target<'_>, exclude: Option<u32>) -> Vec<(u32, u64)> {
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(err) => {
            debug!(kind = ?err.kind(), "cannot list proc root");
            return Vec::new();
        }
    };
    let mut pids: Vec<(u32, u64)> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(error) => {
                trace!(
                    error = &error as &dyn std::error::Error,
                    "proc entry unreadable"
                );
                None
            }
        })
        .filter_map(|entry| pid_of(&entry.file_name()))
        .filter(|&pid| Some(pid) != exclude)
        .filter_map(|pid| {
            let started = start_time(proc_root, pid)?;
            holds(proc_root, pid, target).then_some((pid, started))
        })
        .collect();
    pids.sort_unstable();
    pids.dedup_by_key(|&mut (pid, _)| pid);
    pids
}

/// Returns the pid a `/proc` entry names, or `None` for non-process entries such as `self`.
fn pid_of(name: &std::ffi::OsStr) -> Option<u32> {
    let name = name.to_str()?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match name.parse() {
        Ok(pid) => Some(pid),
        Err(error) => {
            trace!(
                error = &error as &dyn std::error::Error,
                "proc entry pid out of range"
            );
            None
        }
    }
}

enum Target<'a> {
    Device(u64),
    Path(&'a Path),
    /// Socket inode numbers, matched against `socket:[N]` link text.
    Sockets(&'a [u32]),
}

impl<'a> Target<'a> {
    fn of(path: &'a Path) -> Self {
        match fs::metadata(path) {
            Ok(meta) if meta.file_type().is_char_device() => Self::Device(meta.rdev()),
            _ => Self::Path(path),
        }
    }

    fn matches(&self, fd: &Path, link: &Path) -> bool {
        match *self {
            // Only device links are stat'ed, so a descriptor on a hung network file cannot block the scan.
            Self::Device(rdev) => {
                link.starts_with("/dev/")
                    && match fs::metadata(fd) {
                        Ok(meta) => meta.file_type().is_char_device() && meta.rdev() == rdev,
                        Err(error) => {
                            trace!(
                                error = &error as &dyn std::error::Error,
                                "descriptor vanished"
                            );
                            false
                        }
                    }
            }
            Self::Path(path) => link == path,
            Self::Sockets(inodes) => {
                socket_inode(link).is_some_and(|inode| inodes.contains(&inode))
            }
        }
    }
}

/// Returns the inode of `socket:[N]` link text, or `None` for any other link.
fn socket_inode(link: &Path) -> Option<u32> {
    let number = link.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?;
    match number.parse() {
        Ok(inode) => Some(inode),
        Err(error) => {
            trace!(
                error = &error as &dyn std::error::Error,
                "socket inode out of range"
            );
            None
        }
    }
}

fn holds(proc_root: &Path, pid: u32, target: &Target<'_>) -> bool {
    let fds = match fs::read_dir(pid_dir(proc_root, pid).join("fd")) {
        Ok(fds) => fds,
        Err(err) => {
            if !matches!(
                err.kind(),
                ErrorKind::PermissionDenied | ErrorKind::NotFound
            ) {
                debug!(pid, kind = ?err.kind(), "cannot list process descriptors");
            }
            return false;
        }
    };
    fds.filter_map(|fd| match fd {
        Ok(fd) => Some(fd.path()),
        Err(error) => {
            trace!(
                pid,
                error = &error as &dyn std::error::Error,
                "descriptor entry unreadable"
            );
            None
        }
    })
    .any(|fd| match fs::read_link(&fd) {
        Ok(link) => target.matches(&fd, &link),
        Err(error) => {
            trace!(
                pid,
                error = &error as &dyn std::error::Error,
                "descriptor vanished"
            );
            false
        }
    })
}

/// Returns the identity of `pid`, or `None` when its `status` cannot be read.
///
/// `name` is `comm` passed through [`sanitize`] with a 128-char cap.
/// `cmdline` is read up to [`CMDLINE_MAX`] bytes, cut on a char boundary,
/// and passed through [`sanitize`] with a [`CMDLINE_MAX`]-char cap, which
/// turns NUL separators into spaces; it may contain secrets passed as
/// arguments. `uid` is the real uid from `status`. Each field is `None` when
/// unreadable or empty.
#[must_use]
pub fn process_info(proc_root: &Path, pid: u32) -> Option<ProcessInfo> {
    let dir = pid_dir(proc_root, pid);
    let status = read_bounded(&dir.join("status"), PROC_FILE_MAX)?;
    let name = comm(proc_root, pid);
    let cmdline = read_truncated(&dir.join("cmdline"), CMDLINE_MAX)
        .and_then(|raw| sanitize(&raw, CMDLINE_MAX));
    Some(ProcessInfo {
        name,
        exe: match fs::read_link(dir.join("exe")) {
            Ok(exe) => Some(exe),
            Err(error) => {
                trace!(
                    pid,
                    error = &error as &dyn std::error::Error,
                    "executable unreadable"
                );
                None
            }
        },
        pid,
        cmdline,
        uid: status_field(&status, "Uid:"),
    })
}

/// Returns the `comm` of `pid` passed through [`sanitize`] with a 128-char
/// cap, or `None` when it is unreadable or empty.
#[must_use]
pub fn comm(proc_root: &Path, pid: u32) -> Option<String> {
    read_truncated(&pid_dir(proc_root, pid).join("comm"), COMM_MAX)
        .and_then(|comm| sanitize(&comm, TEXT_MAX))
}

/// Returns the real uid of `pid` from `status`.
#[must_use]
pub fn uid(proc_root: &Path, pid: u32) -> Option<u32> {
    let status = read_bounded(&pid_dir(proc_root, pid).join("status"), PROC_FILE_MAX)?;
    status_field(&status, "Uid:")
}

/// Returns the parent of `pid` from `status`, treating pids 0 and 1 as no parent.
#[must_use]
pub fn parent(proc_root: &Path, pid: u32) -> Option<u32> {
    let status = read_bounded(&pid_dir(proc_root, pid).join("status"), PROC_FILE_MAX)?;
    status_field(&status, "PPid:").filter(|&ppid| ppid > 1)
}

/// Returns the start time of `pid` in clock ticks after boot, field 22 of `stat`.
///
/// Two equal readings mean the pid was not reused in between.
#[must_use]
pub fn start_time(proc_root: &Path, pid: u32) -> Option<u64> {
    let stat = read_bounded(&pid_dir(proc_root, pid).join("stat"), PROC_FILE_MAX)?;
    // `comm` may contain spaces and parentheses, so fields are counted after its last `)`.
    let (_, fields) = stat.rsplit_once(')')?;
    let value = fields.split_whitespace().nth(19)?;
    match value.parse() {
        Ok(ticks) => Some(ticks),
        Err(error) => {
            trace!(
                pid,
                error = &error as &dyn std::error::Error,
                "start time unparsable"
            );
            None
        }
    }
}

/// Returns the innermost `.scope` or `.service` segment of the cgroup v2 path of `pid`.
#[must_use]
pub fn cgroup_leaf(proc_root: &Path, pid: u32) -> Option<String> {
    let cgroup = read_bounded(&pid_dir(proc_root, pid).join("cgroup"), PROC_FILE_MAX)?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    path.rsplit('/')
        .find(|segment| {
            segment
                .rsplit_once('.')
                .is_some_and(|(_, kind)| kind == "scope" || kind == "service")
        })
        .map(str::to_owned)
}

fn pid_dir(proc_root: &Path, pid: u32) -> PathBuf {
    proc_root.join(pid.to_string())
}

fn status_field(status: &str, key: &str) -> Option<u32> {
    let value = status.lines().find_map(|line| line.strip_prefix(key))?;
    match value.split_whitespace().next()?.parse() {
        Ok(number) => Some(number),
        Err(error) => {
            trace!(
                key,
                error = &error as &dyn std::error::Error,
                "status field unparsable"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write as _};
    use std::os::unix::fs::symlink;

    use super::*;

    use crate::linux::test_error::{TestError, TestResult};

    const STATUS: &str = "Name:\tssh\nUmask:\t0022\nState:\tS (sleeping)\nPid:\t300\nPPid:\t200\nUid:\t1000\t1001\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";

    fn write_stat(root: &Path, pid: u32, start: u64) -> io::Result<()> {
        let middle = vec!["0"; 18].join(" ");
        fs::write(
            root.join(pid.to_string()).join("stat"),
            format!("{pid} (x) S {middle} {start} 0 0\n"),
        )
    }

    fn fixture(root: &Path, target: &Path) -> io::Result<()> {
        let p300 = root.join("300");
        fs::create_dir_all(p300.join("fd"))?;
        symlink(target, p300.join("fd/3"))?;
        symlink("/dev/null", p300.join("fd/0"))?;
        symlink("/usr/bin/ssh", p300.join("exe"))?;
        fs::write(p300.join("comm"), "ssh\n")?;
        fs::write(p300.join("cmdline"), "ssh\0-T\0host\0")?;
        fs::write(p300.join("status"), STATUS)?;
        fs::write(
            p300.join("cgroup"),
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty-12.scope/sub\n",
        )?;

        let p301 = root.join("301");
        fs::create_dir_all(p301.join("fd"))?;
        symlink(target, p301.join("fd/7"))?;
        symlink(target, p301.join("fd/8"))?;

        let p302 = root.join("302");
        fs::create_dir_all(p302.join("fd"))?;
        symlink("/dev/null", p302.join("fd/0"))?;

        for pid in [300, 301, 302] {
            write_stat(root, pid, u64::from(pid) * 10)?;
        }

        let p304 = root.join("304");
        fs::create_dir_all(p304.join("fd"))?;
        symlink(target, p304.join("fd/3"))?;

        fs::create_dir_all(root.join("303"))?;
        fs::create_dir_all(root.join("self/fd"))?;
        symlink(target, root.join("self/fd/3"))?;
        fs::write(root.join("uptime"), "1.0 1.0\n")?;
        Ok(())
    }

    #[test]
    fn finds_holders_in_fixture() -> TestResult {
        let dir = tempfile::tempdir()?;
        let target = Path::new("/dev/hidraw3");
        fixture(dir.path(), target)?;
        assert_eq!(
            pids_holding(dir.path(), target, None),
            vec![(300, 3000), (301, 3010)]
        );
        assert_eq!(
            pids_holding(dir.path(), target, Some(300)),
            vec![(301, 3010)]
        );
        assert_eq!(
            pids_holding(dir.path(), Path::new("/dev/hidraw4"), None),
            Vec::<(u32, u64)>::new()
        );
        assert_eq!(
            pids_holding(&dir.path().join("missing"), target, None),
            Vec::<(u32, u64)>::new()
        );
        Ok(())
    }

    #[test]
    fn reads_process_fields() -> TestResult {
        let dir = tempfile::tempdir()?;
        fixture(dir.path(), Path::new("/dev/hidraw3"))?;
        let info = process_info(dir.path(), 300).ok_or(TestError::Missing("no info"))?;
        assert_eq!(
            info,
            ProcessInfo {
                name: Some("ssh".to_owned()),
                exe: Some(PathBuf::from("/usr/bin/ssh")),
                pid: 300,
                cmdline: Some("ssh -T host".to_owned()),
                uid: Some(1000),
            }
        );
        assert_eq!(parent(dir.path(), 300), Some(200));
        assert_eq!(
            cgroup_leaf(dir.path(), 300).as_deref(),
            Some("app-kitty-12.scope")
        );
        assert_eq!(process_info(dir.path(), 303), None);
        assert_eq!(process_info(dir.path(), 999), None);
        assert_eq!(parent(dir.path(), 303), None);
        assert_eq!(cgroup_leaf(dir.path(), 303), None);
        Ok(())
    }

    #[test]
    fn init_and_kernel_parents_are_none() -> TestResult {
        let dir = tempfile::tempdir()?;
        for (pid, ppid) in [(10, 1), (11, 0)] {
            let path = dir.path().join(pid.to_string());
            fs::create_dir_all(&path)?;
            fs::write(path.join("status"), format!("PPid:\t{ppid}\n"))?;
        }
        assert_eq!(parent(dir.path(), 10), None);
        assert_eq!(parent(dir.path(), 11), None);
        Ok(())
    }

    #[test]
    fn cgroup_without_unit_segment_is_none() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("5");
        fs::create_dir_all(&path)?;
        fs::write(path.join("cgroup"), "1:name=systemd:/x.scope\n0::/\n")?;
        assert_eq!(cgroup_leaf(dir.path(), 5), None);
        Ok(())
    }

    #[test]
    fn reads_start_time_after_last_paren() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("7");
        fs::create_dir_all(&path)?;
        let middle = vec!["0"; 18].join(" ");
        fs::write(
            path.join("stat"),
            format!("7 (a) b ) S {middle} 4242 0 0\n"),
        )?;
        assert_eq!(start_time(dir.path(), 7), Some(4242));
        fs::write(path.join("stat"), "7 (short) S 1\n")?;
        assert_eq!(start_time(dir.path(), 7), None);
        assert_eq!(start_time(dir.path(), 8), None);
        Ok(())
    }

    #[test]
    fn caps_cmdline() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("9");
        fs::create_dir_all(&path)?;
        fs::write(path.join("status"), STATUS)?;
        let long = format!("tool\0{}", "é".repeat(CMDLINE_MAX));
        fs::write(path.join("cmdline"), long)?;
        let cmdline = process_info(dir.path(), 9)
            .and_then(|info| info.cmdline)
            .ok_or(TestError::Missing("no cmdline"))?;
        assert!(cmdline.len() <= CMDLINE_MAX);
        assert!(cmdline.starts_with("tool é"));
        Ok(())
    }

    #[test]
    fn sanitizes_process_text() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("9");
        fs::create_dir_all(&path)?;
        fs::write(path.join("status"), STATUS)?;
        fs::write(path.join("comm"), "ev\u{1b}[2Jil\u{202e}\n")?;
        fs::write(
            path.join("cmdline"),
            "tool\0--name\0a\u{1b}]0;x\u{7}\u{2066}b\0",
        )?;
        let info = process_info(dir.path(), 9).ok_or(TestError::Missing("no info"))?;
        assert_eq!(info.name.as_deref(), Some("ev [2Jil"));
        assert_eq!(info.cmdline.as_deref(), Some("tool --name a ]0;x b"));
        Ok(())
    }

    #[test]
    fn finds_device_holder_by_identity() -> TestResult {
        let _null = fs::File::open("/dev/null")?;
        let dir = tempfile::tempdir()?;
        let alias = dir.path().join("alias");
        symlink("/dev/null", &alias)?;
        let own = std::process::id();
        let started =
            start_time(Path::new("/proc"), own).ok_or(TestError::Missing("no start time"))?;
        let holders = pids_holding(Path::new("/proc"), &alias, None);
        assert!(holders.contains(&(own, started)), "{holders:?} lacks {own}");
        Ok(())
    }

    #[test]
    fn finds_own_open_file_in_real_proc() -> TestResult {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(b"held")?;
        let own = std::process::id();
        let path = file.path().canonicalize()?;
        let holders = pids_holding(Path::new("/proc"), &path, None);
        assert!(
            holders.iter().any(|&(pid, _)| pid == own),
            "{holders:?} lacks {own}"
        );
        let holders = pids_holding(Path::new("/proc"), &path, Some(own));
        assert!(!holders.iter().any(|&(pid, _)| pid == own));
        Ok(())
    }

    #[test]
    fn finds_holders_of_any_listed_socket() -> TestResult {
        let dir = tempfile::tempdir()?;
        for (pid, link) in [
            (400, "socket:[11]"),
            (401, "socket:[12]"),
            (402, "socket:[13]"),
            (403, "socket:[x]"),
        ] {
            let fd = dir.path().join(pid.to_string()).join("fd");
            fs::create_dir_all(&fd)?;
            symlink(link, fd.join("3"))?;
            write_stat(dir.path(), pid, u64::from(pid) * 10)?;
        }
        assert_eq!(
            pids_holding_sockets(dir.path(), &[11, 13], Some(402)),
            [(400, 4000)]
        );
        assert_eq!(pids_holding_sockets(dir.path(), &[], None), []);
        Ok(())
    }
}
