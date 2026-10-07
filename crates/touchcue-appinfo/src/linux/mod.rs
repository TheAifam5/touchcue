//! Linux attribution of device holders to processes and applications.

mod bounded;
pub mod desktop;
pub mod icon;
mod ini;
pub mod procfs;
pub mod settings;
#[cfg(test)]
mod test_error;

use std::env;
use std::path::{Path, PathBuf};

use touchcue_core::skip::{self, SkipList};
use touchcue_core::{AppInfo, ProcessInfo, Requester};
use tracing::debug;

use touchcue_core::text::sanitize;

use crate::TEXT_MAX;
use crate::unit::{self, UnitApp};
use desktop::ExeIndex;
use icon::IconLookup;

/// Maximum number of processes visited from a pid up its ancestry.
const MAX_ANCESTRY: usize = 32;
/// Most process names listed in a chain, the last walked process included.
const CHAIN_ENTRIES: usize = 8;
/// Longest process name in a chain, in chars.
const CHAIN_NAME_MAX: usize = 32;
/// Separator of chain entries; replaced by a space inside names so that a
/// name cannot fake an entry.
const CHAIN_SEPARATOR: char = '←';

/// Resolves pids to processes and applications from a procfs tree and XDG data directories.
#[derive(Debug, Clone)]
pub struct Resolver {
    proc_root: PathBuf,
    data_dirs: Vec<PathBuf>,
    icons: IconLookup,
    locales: Vec<String>,
    skip: SkipList,
}

/// Application, requester and process chain of a client, from one walk of its ancestry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The application found by [`Resolver::origin`].
    pub app: Option<AppInfo>,
    /// `None` when the client runs the application's executable and every
    /// process between them is skipped, or no process name could be read.
    pub requester: Option<Requester>,
    /// The client's name and its ancestors' names, client first, joined by
    /// ` ← `; see [`Resolver::origin`].
    pub chain: Option<String>,
    /// Every process walked, client first, up to the application's process.
    pub walk: Vec<Step>,
}

/// A process walked by [`Resolver::origin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Name as listed in the chain.
    pub name: String,
    /// Whether the process was passed over when looking for the requester;
    /// `false` for the application's process, which is not considered.
    pub skipped: bool,
}

/// A process of a walked ancestry whose name could be read.
#[derive(Debug)]
struct Member {
    pid: u32,
    /// `comm`, else the executable's file name, sanitized; matched against
    /// the skip list.
    name: String,
    exe: Option<PathBuf>,
    /// Real uid.
    uid: Option<u32>,
}

/// An application found in an ancestry.
struct Found {
    app: AppInfo,
    /// Position of the matched process in the ancestry.
    index: usize,
    /// Whether the match came from the process's systemd unit.
    unit: bool,
}

impl Resolver {
    /// Creates a resolver; desktop entry names are localized from the current
    /// locale environment, and icons are looked up in the hicolor theme in
    /// `~/.icons`, the `icons` directory of each of `data_dirs` and
    /// `/usr/share/pixmaps`, with the home directory taken from `$HOME`.
    #[must_use]
    pub fn new(proc_root: PathBuf, data_dirs: Vec<PathBuf>, icon_size: u16) -> Self {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        let icons = IconLookup::new(
            icon::base_dirs(home.as_deref(), &data_dirs),
            home,
            icon::DEFAULT_THEME.to_owned(),
            icon_size,
        );
        Self {
            proc_root,
            data_dirs,
            icons,
            locales: desktop::locales(),
            skip: SkipList::default(),
        }
    }

    /// Returns the resolver with application icons looked up by `icons`.
    #[must_use]
    pub fn with_icons(self, icons: IconLookup) -> Self {
        Self { icons, ..self }
    }

    /// Returns the icon lookup used for application icons.
    #[must_use]
    pub fn icons(&self) -> &IconLookup {
        &self.icons
    }

    /// Returns the resolver with `skip` used by [`Self::origin`] in place of the defaults.
    #[must_use]
    pub fn with_skip(self, skip: SkipList) -> Self {
        Self { skip, ..self }
    }

    /// Creates a resolver over `/proc` and the XDG data directories, with 64-pixel icons.
    #[must_use]
    pub fn system() -> Self {
        Self::new(PathBuf::from("/proc"), desktop::default_data_dirs(), 64)
    }

    /// Returns the sorted pids, other than this process, holding `node` open, each with its start time.
    ///
    /// Pass the start time to [`Self::process`] and [`Self::origin`] so that a
    /// pid reused after the scan is not attributed.
    #[must_use]
    pub fn holders(&self, node: &Path) -> Vec<(u32, u64)> {
        procfs::pids_holding(&self.proc_root, node, Some(std::process::id()))
    }

    /// Returns the sorted pids, other than this process, holding any of the
    /// sockets with inode numbers `inodes`, each with its start time.
    #[must_use]
    pub fn socket_holders(&self, inodes: &[u32]) -> Vec<(u32, u64)> {
        procfs::pids_holding_sockets(&self.proc_root, inodes, Some(std::process::id()))
    }

    /// Returns `pid` and up to 31 of its ancestors, nearest first.
    #[must_use]
    pub fn lineage(&self, pid: u32) -> Vec<u32> {
        self.ancestry(pid).into_iter().map(|(pid, _)| pid).collect()
    }

    /// Returns the identity of `pid` if its start time is `expected_start`.
    ///
    /// Returns `None` when it cannot be read or its start time differs from
    /// `expected_start` before or after reading, meaning the pid was reused.
    #[must_use]
    pub fn process(&self, pid: u32, expected_start: u64) -> Option<ProcessInfo> {
        self.guarded(pid, expected_start, || {
            procfs::process_info(&self.proc_root, pid)
        })
    }

    /// Returns the application of `pid` as found by [`Self::origin`].
    #[cfg(test)]
    fn app(&self, pid: u32, expected_start: u64) -> Option<AppInfo> {
        self.guarded(pid, expected_start, || {
            self.app_in(&self.ancestry(pid)).map(|found| found.app)
        })
    }

    /// Returns the application, requester and process chain of `pid` if its
    /// start time is `expected_start`.
    ///
    /// The application is searched for in `pid` and up to 31 ancestors.
    /// The nearest process in a systemd application unit wins; its desktop
    /// entry supplies name, icon and window class. When the unit's
    /// application id has no entry, the entry matching that process's
    /// executable by [`ExeIndex::find`], or else by [`ExeIndex::find_by_dir`],
    /// supplies them and its file stem becomes the id; when neither exists,
    /// the unit's application id is both id and name. When no process is in
    /// such a unit, the nearest process whose executable matches an entry by
    /// [`ExeIndex::find`] wins, and otherwise the nearest one matching by
    /// [`ExeIndex::find_by_dir`]. The application's `exe` and `pid` describe
    /// the matched process; it is `None` when the matched process was reused.
    /// Returns `None` when the start time of `pid` differs from
    /// `expected_start` before or after resolving.
    ///
    /// The application is a claim, not a verified identity: any process of
    /// the same user can choose its unit name and executable name, and so
    /// can present itself as any installed application. Text fields are
    /// passed through [`sanitize`] with a 128-char cap.
    ///
    /// The walk then covers
    /// `pid` and its ancestors up to the matched process; when the match came
    /// from a systemd unit, up to the topmost consecutive ancestor in the
    /// same unit, which is the process that started the application. Without
    /// an application it covers every ancestor read. It stops at the first
    /// process whose executable and `comm` are both unreadable or whose start
    /// time changed, meaning its pid was reused.
    ///
    /// The requester is chosen by [`skip::requester`] among the processes
    /// strictly below the application's process, which is the unit root for
    /// a unit match and the matched process for an executable match, or
    /// among every process walked without an application: the topmost one
    /// not skipped. A process is skipped when its name, `comm` or else the
    /// executable's file name without a ` (deleted)` suffix, is on the
    /// resolver's [`SkipList`], when its executable is the application
    /// process's executable, or when its real uid differs from the client's.
    /// When every process is skipped, the requester is the client, unless
    /// the client is the application's process or runs its executable,
    /// ignoring a ` (deleted)` suffix, in which case it is `None`. The
    /// application process's executable is used only while its start time
    /// is unchanged.
    /// When the walk stops early, the requester is the best guess among the
    /// processes read.
    ///
    /// The chain lists at most 8 names, each passed through [`sanitize`]
    /// with a 32-char cap after `←` is replaced by a space. When the walk
    /// covered more processes, it lists the first 7, `…`, and the last
    /// process walked, which is the application's when one was found and
    /// every name up to it was readable. Like the application, names
    /// are claims that any process of the same user can choose.
    #[must_use]
    pub fn origin(&self, pid: u32, expected_start: u64) -> Option<Origin> {
        self.guarded(pid, expected_start, || {
            let ancestry = self.ancestry(pid);
            let found = self.app_in(&ancestry);
            let last = match &found {
                Some(found) if found.unit => self.unit_root(&ancestry, found.index),
                Some(found) => found.index,
                None => ancestry.len().saturating_sub(1),
            };
            let members: Vec<Member> = ancestry
                .iter()
                .take(last.saturating_add(1))
                .map_while(|&(pid, started)| self.member(pid, started))
                .collect();
            let below = if found.is_some() {
                members.len().min(last)
            } else {
                members.len()
            };
            let app_exe = found
                .as_ref()
                .and(ancestry.get(last))
                .and_then(|&(app_pid, started)| self.current_exe(app_pid, started));
            let runs_app = |member: &Member| match (&member.exe, &app_exe) {
                (Some(exe), Some(app)) => same_exe(exe, app),
                _ => false,
            };
            let client_uid = members.first().and_then(|member| member.uid);
            let skipped: Vec<bool> = members
                .iter()
                .take(below)
                .map(|member| {
                    self.skip.skips(&member.name)
                        || runs_app(member)
                        || matches!((client_uid, member.uid), (Some(client), Some(uid)) if client != uid)
                })
                .collect();
            let requester = match skip::requester(&skipped) {
                Some(index) => members.get(index),
                // The client is the application's own process.
                None if found.is_some() && last == 0 => None,
                None => members.first().filter(|client| !runs_app(client)),
            }
            .map(|member| Requester {
                name: Some(member.name.clone()),
                exe: member.exe.clone(),
                pid: member.pid,
            });
            debug!(
                pid,
                requester_pid = requester.as_ref().map(|r| r.pid),
                walked = members.len(),
                "requester chosen"
            );
            let walk = members
                .iter()
                .enumerate()
                .map(|(index, member)| Step {
                    name: chain_name(member),
                    skipped: skipped.get(index).copied().unwrap_or_default(),
                })
                .collect();
            Some(Origin {
                app: found.map(|found| found.app),
                requester,
                chain: chain(&members),
                walk,
            })
        })
    }

    /// Runs `resolve` and keeps its result only if the start time of `pid` is `expected` before and after it.
    fn guarded<T>(
        &self,
        pid: u32,
        expected: u64,
        resolve: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        let current = || procfs::start_time(&self.proc_root, pid) == Some(expected);
        if !current() {
            debug!(pid, "pid reused before resolution");
            return None;
        }
        let built = resolve()?;
        if current() {
            Some(built)
        } else {
            debug!(pid, "pid reused during resolution");
            None
        }
    }

    /// Returns `pid` and its ancestors with their start times, stopping at the first unreadable start time.
    fn ancestry(&self, pid: u32) -> Vec<(u32, u64)> {
        let mut chain: Vec<(u32, u64)> = Vec::new();
        let mut next = Some(pid);
        while let Some(current) = next
            && chain.len() < MAX_ANCESTRY
            && !chain.iter().any(|&(seen, _)| seen == current)
        {
            let Some(started) = procfs::start_time(&self.proc_root, current) else {
                break;
            };
            chain.push((current, started));
            next = procfs::parent(&self.proc_root, current);
        }
        chain
    }

    /// Returns the application of the nearest matching process in
    /// `ancestry`, as described for [`Self::origin`], with its position.
    fn app_in(&self, ancestry: &[(u32, u64)]) -> Option<Found> {
        let found = ancestry
            .iter()
            .enumerate()
            .find_map(|(index, &(ancestor, _))| {
                Some(Found {
                    app: self.app_from_unit(ancestor)?,
                    index,
                    unit: true,
                })
            })
            .or_else(|| {
                let exes = ExeIndex::build(&self.data_dirs);
                let nearest = |resolve: fn(&Self, &ExeIndex, u32) -> Option<AppInfo>| {
                    ancestry
                        .iter()
                        .enumerate()
                        .find_map(|(index, &(ancestor, _))| {
                            Some(Found {
                                app: resolve(self, &exes, ancestor)?,
                                index,
                                unit: false,
                            })
                        })
                };
                nearest(Self::app_from_exe).or_else(|| nearest(Self::app_from_exe_dir))
            })?;
        let &(matched, started) = ancestry.get(found.index)?;
        (procfs::start_time(&self.proc_root, matched) == Some(started)).then_some(found)
    }

    /// Returns the position of the topmost ancestor, from `index` up through
    /// consecutive ancestors, in the same cgroup unit as `ancestry[index]`.
    ///
    /// An ancestor whose start time changed, meaning its pid was reused, ends
    /// the unit there.
    fn unit_root(&self, ancestry: &[(u32, u64)], index: usize) -> usize {
        let leaf = |position: usize| {
            let &(pid, started) = ancestry.get(position)?;
            let leaf = procfs::cgroup_leaf(&self.proc_root, pid)?;
            (procfs::start_time(&self.proc_root, pid) == Some(started)).then_some(leaf)
        };
        let Some(unit) = leaf(index) else {
            return index;
        };
        let mut root = index;
        while leaf(root + 1).as_ref() == Some(&unit) {
            root += 1;
        }
        root
    }

    /// Returns the executable of `pid` if its start time is `started` before
    /// and after reading it.
    fn current_exe(&self, pid: u32, started: u64) -> Option<PathBuf> {
        let current = || procfs::start_time(&self.proc_root, pid) == Some(started);
        if !current() {
            return None;
        }
        let exe = self.exe(pid)?;
        current().then_some(exe)
    }

    /// Returns the names of `pid` if its start time is still `started`.
    fn member(&self, pid: u32, started: u64) -> Option<Member> {
        let exe = self.exe(pid);
        let name = procfs::comm(&self.proc_root, pid).or_else(|| {
            exe.as_deref()
                .and_then(exe_name)
                .and_then(|file| sanitize(file, TEXT_MAX))
        })?;
        let uid = procfs::uid(&self.proc_root, pid);
        if procfs::start_time(&self.proc_root, pid) != Some(started) {
            debug!(pid, "ancestor pid reused");
            return None;
        }
        Some(Member {
            pid,
            name,
            exe,
            uid,
        })
    }

    fn app_from_unit(&self, pid: u32) -> Option<AppInfo> {
        let leaf = procfs::cgroup_leaf(&self.proc_root, pid)?;
        let UnitApp { launcher, app_id } = unit::parse(&leaf)?;
        debug!(pid, "application unit found");
        let entry = desktop::find_entry(&self.data_dirs, &app_id)
            .and_then(|path| desktop::read_entry(&path, &self.locales));
        let container = launcher.filter(|l| l == "flatpak");
        let exe = self.exe(pid);
        if let Some(entry) = entry {
            return Some(self.describe(app_id, entry, exe, pid, container));
        }
        let index = ExeIndex::build(&self.data_dirs);
        if let Some(app) = self
            .app_from_exe(&index, pid)
            .or_else(|| self.app_from_exe_dir(&index, pid))
        {
            return Some(AppInfo { container, ..app });
        }
        Some(AppInfo {
            name: Some(app_id.clone()),
            id: Some(app_id),
            exe,
            pid: Some(pid),
            container,
            ..AppInfo::default()
        })
    }

    fn app_from_exe(&self, index: &ExeIndex, pid: u32) -> Option<AppInfo> {
        let exe = self.exe(pid)?;
        let path = index.find(exe_name(&exe)?)?;
        debug!(pid, "desktop entry matched executable");
        self.app_from_entry(path, exe, pid)
    }

    fn app_from_exe_dir(&self, index: &ExeIndex, pid: u32) -> Option<AppInfo> {
        let exe = self.exe(pid)?;
        let path = index.find_by_dir(&exe)?;
        debug!(pid, "desktop entry matched executable directory");
        self.app_from_entry(path, exe, pid)
    }

    fn app_from_entry(&self, path: &Path, exe: PathBuf, pid: u32) -> Option<AppInfo> {
        let id = sanitize(path.file_stem()?.to_str()?, TEXT_MAX)?;
        let entry = desktop::read_entry(path, &self.locales)?;
        Some(self.describe(id, entry, Some(exe), pid, None))
    }

    fn describe(
        &self,
        id: String,
        entry: desktop::Entry,
        exe: Option<PathBuf>,
        pid: u32,
        container: Option<String>,
    ) -> AppInfo {
        AppInfo {
            name: entry.name,
            id: Some(id),
            icon: entry.icon.and_then(|icon| self.icons.resolve(&icon)),
            exe,
            pid: Some(pid),
            cmdline: None,
            wm_class: entry.wm_class,
            container,
        }
    }

    fn exe(&self, pid: u32) -> Option<PathBuf> {
        match std::fs::read_link(self.proc_root.join(pid.to_string()).join("exe")) {
            Ok(exe) => Some(exe),
            Err(error) => {
                debug!(
                    pid,
                    error = &error as &dyn std::error::Error,
                    "executable unreadable"
                );
                None
            }
        }
    }
}

/// Returns whether two executable links name the same file, ignoring a
/// ` (deleted)` suffix on either.
fn same_exe(a: &Path, b: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let strip = |path: &Path| {
        let bytes = path.as_os_str().as_bytes();
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes).to_vec()
    };
    strip(a) == strip(b)
}

/// Returns the file name of an executable link, without a ` (deleted)` suffix.
fn exe_name(exe: &Path) -> Option<&str> {
    let name = exe.file_name()?.to_str()?;
    Some(name.strip_suffix(" (deleted)").unwrap_or(name))
}

/// Returns the name of `member` with `←` replaced by a space, sanitized and
/// capped at 32 chars, or `?` when nothing visible remains.
fn chain_name(member: &Member) -> String {
    sanitize(&member.name.replace(CHAIN_SEPARATOR, " "), CHAIN_NAME_MAX)
        .unwrap_or_else(|| "?".to_owned())
}

/// Returns the names of `members` joined by ` ← `, as described for [`Resolver::origin`].
fn chain(members: &[Member]) -> Option<String> {
    let names: Vec<String> = if members.len() > CHAIN_ENTRIES {
        let mut names: Vec<String> = members
            .iter()
            .take(CHAIN_ENTRIES - 1)
            .map(chain_name)
            .collect();
        names.push("…".to_owned());
        names.extend(members.last().map(chain_name));
        names
    } else {
        members.iter().map(chain_name).collect()
    };
    (!names.is_empty()).then(|| names.join(&format!(" {CHAIN_SEPARATOR} ")))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::os::unix::fs::symlink;

    use super::*;

    use test_error::{TestError, TestResult};

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> io::Result<Self> {
            let dir = tempfile::tempdir()?;
            fs::create_dir_all(dir.path().join("proc"))?;
            fs::create_dir_all(dir.path().join("share/applications"))?;
            Ok(Self { dir })
        }

        fn process(&self, pid: u32, parent: u32, cgroup: &str, exe: &str) -> io::Result<()> {
            let path = self.dir.path().join("proc").join(pid.to_string());
            fs::create_dir_all(&path)?;
            fs::write(
                path.join("status"),
                format!("PPid:\t{parent}\nUid:\t1000\t1000\t1000\t1000\n"),
            )?;
            fs::write(path.join("cgroup"), format!("0::{cgroup}\n"))?;
            self.started(pid, u64::from(pid))?;
            symlink(exe, path.join("exe"))
        }

        fn started(&self, pid: u32, start: u64) -> io::Result<()> {
            let middle = vec!["0"; 18].join(" ");
            fs::write(
                self.dir
                    .path()
                    .join("proc")
                    .join(pid.to_string())
                    .join("stat"),
                format!("{pid} (x) S {middle} {start} 0 0\n"),
            )
        }

        fn entry(&self, file: &str, body: &str) -> io::Result<()> {
            fs::write(
                self.dir.path().join("share/applications").join(file),
                format!("[Desktop Entry]\nType=Application\n{body}"),
            )
        }

        fn icon(&self) -> TestResult<String> {
            let path = self.dir.path().join("kitty.png");
            fs::write(&path, b"png")?;
            path.into_os_string()
                .into_string()
                .map_err(TestError::NonUtf8Path)
        }

        fn resolver(&self) -> Resolver {
            Resolver {
                proc_root: self.dir.path().join("proc"),
                data_dirs: vec![self.dir.path().join("share")],
                icons: IconLookup::new(Vec::new(), None, icon::DEFAULT_THEME.to_owned(), 64),
                locales: Vec::new(),
                skip: SkipList::default(),
            }
        }

        /// Writes a parent chain of processes from `links`, client first;
        /// each runs `/usr/bin/<name>` with `comm` `<name>` in a session
        /// scope, and the last has no parent. Returns the client's pid.
        fn chain(&self, links: &[(u32, &str)]) -> TestResult<u32> {
            for (position, &(pid, name)) in links.iter().enumerate() {
                let parent = links.get(position + 1).map_or(1, |&(parent, _)| parent);
                self.process(pid, parent, SESSION, &format!("/usr/bin/{name}"))?;
                self.comm(pid, name)?;
            }
            links
                .first()
                .map(|&(pid, _)| pid)
                .ok_or(TestError::Missing("empty chain"))
        }

        fn comm(&self, pid: u32, comm: &str) -> io::Result<()> {
            fs::write(
                self.dir
                    .path()
                    .join("proc")
                    .join(pid.to_string())
                    .join("comm"),
                format!("{comm}\n"),
            )
        }

        fn kitty(&self) -> io::Result<()> {
            self.entry("kitty.desktop", "Name=Kitty\nExec=kitty\n")
        }

        fn origin(&self, pid: u32) -> TestResult<Origin> {
            self.resolver()
                .origin(pid, u64::from(pid))
                .ok_or(TestError::Missing("pid reused"))
        }

        /// Points the `exe` link of `pid` at `exe`.
        fn relink(&self, pid: u32, exe: &str) -> io::Result<()> {
            let link = self
                .dir
                .path()
                .join("proc")
                .join(pid.to_string())
                .join("exe");
            fs::remove_file(&link)?;
            symlink(exe, link)
        }

        fn uid(&self, pid: u32, parent: u32, uid: u32) -> io::Result<()> {
            fs::write(
                self.dir
                    .path()
                    .join("proc")
                    .join(pid.to_string())
                    .join("status"),
                format!("PPid:\t{parent}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
        }

        fn cgroup(&self, pids: &[u32], cgroup: &str) -> io::Result<()> {
            for pid in pids {
                fs::write(
                    self.dir
                        .path()
                        .join("proc")
                        .join(pid.to_string())
                        .join("cgroup"),
                    format!("0::{cgroup}\n"),
                )?;
            }
            Ok(())
        }
    }

    fn requester(origin: &Origin) -> Option<(u32, &str)> {
        let requester = origin.requester.as_ref()?;
        Some((requester.pid, requester.name.as_deref()?))
    }

    fn label(origin: &Origin) -> Option<String> {
        touchcue_core::placeholders::requester_label(
            origin.requester.as_ref().and_then(|r| r.name.as_deref()),
            origin.app.as_ref().and_then(|a| a.name.as_deref()),
        )
    }

    /// Returns the walk with skipped names in brackets, joined by ` ← `.
    fn walk(origin: &Origin) -> String {
        origin
            .walk
            .iter()
            .map(|step| {
                if step.skipped {
                    format!("[{}]", step.name)
                } else {
                    step.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ← ")
    }

    const SESSION: &str = "/user.slice/user-1000.slice/session-2.scope";
    const KITTY: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty-12.scope";

    #[test]
    fn terminal_child_resolves_to_terminal() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(300, 200, SESSION, "/usr/bin/ssh")?;
        fx.process(200, 1, KITTY, "/usr/bin/kitty")?;
        let icon = fx.icon()?;
        fx.entry(
            "kitty.desktop",
            &format!("Name=kitty\nIcon={icon}\nExec=kitty\nStartupWMClass=kitty\n"),
        )?;
        let app = fx
            .resolver()
            .app(300, 300)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(
            app,
            AppInfo {
                name: Some("kitty".to_owned()),
                id: Some("kitty".to_owned()),
                icon: Some(icon),
                exe: Some(PathBuf::from("/usr/bin/kitty")),
                pid: Some(200),
                cmdline: None,
                wm_class: Some("kitty".to_owned()),
                container: None,
            }
        );
        Ok(())
    }

    #[test]
    fn unit_without_entry_uses_app_id() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(
            50,
            1,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.discordapp.Discord-1323191083.scope",
            "/app/discord",
        )?;
        fx.entry("other.desktop", "Name=Other\nExec=/usr/bin/other\n")?;
        let app = fx
            .resolver()
            .app(50, 50)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(app.id.as_deref(), Some("com.discordapp.Discord"));
        assert_eq!(app.name.as_deref(), Some("com.discordapp.Discord"));
        assert_eq!(app.container.as_deref(), Some("flatpak"));
        assert_eq!(app.pid, Some(50));
        assert_eq!(app.icon, None);
        Ok(())
    }

    #[test]
    fn unit_without_entry_falls_back_to_executable() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(
            80,
            1,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.chromium.Chromium-80.scope",
            "/opt/browser/browser",
        )?;
        let icon = fx.icon()?;
        fx.entry(
            "com.example.Browser.desktop",
            &format!("Name=Browser\nIcon={icon}\nExec=/opt/browser/browser %U\n"),
        )?;
        let app = fx
            .resolver()
            .app(80, 80)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(
            app,
            AppInfo {
                name: Some("Browser".to_owned()),
                id: Some("com.example.Browser".to_owned()),
                icon: Some(icon),
                exe: Some(PathBuf::from("/opt/browser/browser")),
                pid: Some(80),
                cmdline: None,
                wm_class: None,
                container: None,
            }
        );
        Ok(())
    }

    #[test]
    fn unit_without_entry_falls_back_to_executable_dir() -> TestResult {
        let fx = Fixture::new()?;
        let root = fs::canonicalize(fx.dir.path())?;
        let app = root.join("opt/browser");
        fs::create_dir_all(&app)?;
        fs::create_dir_all(root.join("bin"))?;
        fs::write(app.join("browser-nightly"), "#!/bin/sh\n")?;
        symlink(
            app.join("browser-nightly"),
            root.join("bin/browser-nightly"),
        )?;
        let exe = app.join("browser");
        let exe = exe
            .to_str()
            .ok_or(TestError::Missing("non-UTF-8 temp path"))?;
        fx.process(
            85,
            1,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.chromium.Chromium-85.scope",
            exe,
        )?;
        let wrapper = root.join("bin/browser-nightly");
        let wrapper = wrapper
            .to_str()
            .ok_or(TestError::Missing("non-UTF-8 temp path"))?;
        let body =
            format!("Name=Browser Nightly\nExec={wrapper} %U\nStartupWMClass=browser-nightly\n");
        fx.entry("com.example.Browser.nightly.desktop", &body)?;
        fx.entry("browser-nightly.desktop", &body)?;
        let app = fx
            .resolver()
            .app(85, 85)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(app.id.as_deref(), Some("browser-nightly"));
        assert_eq!(app.name.as_deref(), Some("Browser Nightly"));
        assert_eq!(app.exe.as_deref(), Some(Path::new(exe)));
        assert_eq!(app.pid, Some(85));
        Ok(())
    }

    #[test]
    fn unit_entry_wins_over_executable() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(
            90,
            1,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.example.Term-90.scope",
            "/usr/bin/term",
        )?;
        fx.entry("org.example.Term.desktop", "Name=Term\nExec=launch-term\n")?;
        fx.entry("a-term.desktop", "Name=Other Term\nTryExec=/usr/bin/term\n")?;
        let app = fx
            .resolver()
            .app(90, 90)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(app.id.as_deref(), Some("org.example.Term"));
        assert_eq!(app.name.as_deref(), Some("Term"));
        Ok(())
    }

    #[test]
    fn falls_back_to_executable() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(410, 400, SESSION, "/usr/bin/ssh")?;
        fx.process(400, 1, SESSION, "/opt/editor/bin/editor (deleted)")?;
        fx.entry(
            "org.example.Editor.desktop",
            "Name=Editor\nExec=/opt/editor/bin/editor %F\n",
        )?;
        let app = fx
            .resolver()
            .app(410, 410)
            .ok_or(TestError::Missing("unresolved"))?;
        assert_eq!(app.id.as_deref(), Some("org.example.Editor"));
        assert_eq!(app.name.as_deref(), Some("Editor"));
        assert_eq!(app.pid, Some(400));
        assert_eq!(app.container, None);
        Ok(())
    }

    #[test]
    fn unmatched_process_is_none() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(10, 1, SESSION, "/usr/bin/ssh")?;
        let resolver = fx.resolver();
        assert_eq!(resolver.app(10, 10), None);
        assert_eq!(resolver.app(99, 99), None);
        Ok(())
    }

    #[test]
    fn ancestry_is_bounded() -> TestResult {
        let fx = Fixture::new()?;
        let leaf = 1000;
        let depth = u32::try_from(MAX_ANCESTRY)?;
        for pid in leaf..=leaf + depth {
            let cgroup = if pid >= leaf + depth - 1 {
                KITTY
            } else {
                SESSION
            };
            fx.process(pid, pid + 1, cgroup, "/usr/bin/sh")?;
        }
        let resolver = fx.resolver();
        assert_eq!(resolver.ancestry(leaf).len(), MAX_ANCESTRY);
        let app = resolver
            .app(leaf, u64::from(leaf))
            .ok_or(TestError::Missing("last ancestor in range unresolved"))?;
        assert_eq!(app.pid, Some(leaf + depth - 1));

        let fx = Fixture::new()?;
        for pid in leaf..=leaf + depth {
            let cgroup = if pid == leaf + depth { KITTY } else { SESSION };
            fx.process(pid, pid + 1, cgroup, "/usr/bin/sh")?;
        }
        assert_eq!(fx.resolver().app(leaf, u64::from(leaf)), None);
        Ok(())
    }

    #[test]
    fn parent_cycle_terminates() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(20, 21, SESSION, "/usr/bin/a")?;
        fx.process(21, 20, SESSION, "/usr/bin/b")?;
        assert_eq!(fx.resolver().ancestry(20), [(20, 20), (21, 21)]);
        Ok(())
    }

    #[test]
    fn reused_pid_is_dropped() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(60, 1, SESSION, "/usr/bin/a")?;
        let resolver = fx.resolver();
        assert_eq!(resolver.process(60, 60).map(|info| info.pid), Some(60));
        assert_eq!(resolver.process(60, 59), None);
        assert_eq!(resolver.guarded(60, 60, || Some(())), Some(()));
        let mut rewrite = Ok(());
        let reused = resolver.guarded(60, 60, || {
            rewrite = fx.started(60, 9999);
            match rewrite {
                Ok(()) => Some(()),
                Err(_) => None,
            }
        });
        rewrite?;
        assert_eq!(reused, None);
        fs::remove_file(fx.dir.path().join("proc/60/stat"))?;
        assert_eq!(resolver.process(60, 60), None);
        Ok(())
    }

    #[test]
    fn unit_text_is_sanitized() -> TestResult {
        let fx = Fixture::new()?;
        fx.process(
            70,
            1,
            r"/app.slice/app-Evil\x1b[2J\x0aText-1.scope",
            "/usr/bin/a",
        )?;
        assert_eq!(fx.resolver().app(70, 71), None);
        let app = fx
            .resolver()
            .app(70, 70)
            .ok_or(TestError::Missing("unresolved"))?;
        for text in [&app.id, &app.name].into_iter().flatten() {
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
        assert_eq!(app.id.as_deref(), Some("Evil [2J Text"));
        Ok(())
    }

    #[test]
    fn tool_started_in_a_multiplexer_is_the_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (108, "gpg"),
            (107, "git"),
            (106, "bash"),
            (105, "claude"),
            (104, "nu"),
            (103, "herdr"),
            (102, "herdr"),
            (101, "nu"),
            (100, "kitty"),
        ])?;
        let origin = fx.origin(client)?;
        assert_eq!(requester(&origin), Some((105, "claude")));
        let exe = origin.requester.as_ref().and_then(|r| r.exe.as_deref());
        assert_eq!(exe, Some(Path::new("/usr/bin/claude")));
        assert_eq!(label(&origin).as_deref(), Some("claude in Kitty"));
        assert_eq!(
            origin.chain.as_deref(),
            Some("gpg ← git ← bash ← claude ← nu ← herdr ← herdr ← … ← kitty")
        );
        assert_eq!(
            walk(&origin),
            "gpg ← git ← [bash] ← claude ← [nu] ← [herdr] ← [herdr] ← [nu] ← kitty"
        );
        Ok(())
    }

    #[test]
    fn command_typed_in_a_terminal_is_the_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(203, "gpg"), (202, "git"), (201, "zsh"), (200, "kitty")])?;
        assert_eq!(label(&fx.origin(client)?).as_deref(), Some("git in Kitty"));

        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (213, "ssh-sk-helper"),
            (212, "ssh"),
            (211, "zsh"),
            (210, "kitty"),
        ])?;
        assert_eq!(label(&fx.origin(client)?).as_deref(), Some("ssh in Kitty"));
        Ok(())
    }

    #[test]
    fn helpers_below_the_requester_are_ignored() -> TestResult {
        for helpers in [
            &[
                (308, "gpg"),
                (307, "git"),
                (306, "sh"),
                (305, "make"),
                (304, "bash"),
            ][..],
            &[(308, "gpg"), (307, "jj")][..],
            &[(308, "gpg"), (307, "pass")][..],
        ] {
            let fx = Fixture::new()?;
            fx.kitty()?;
            let mut links = helpers.to_vec();
            links.extend([(302, "claude"), (301, "nu"), (300, "kitty")]);
            let client = fx.chain(&links)?;
            fx.relink(307, "/usr/bin/bash")?;
            assert_eq!(requester(&fx.origin(client)?), Some((302, "claude")));
        }
        Ok(())
    }

    #[test]
    fn script_is_named_by_comm_not_its_interpreter() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (404, "gpg"),
            (403, "git"),
            (402, "release.sh"),
            (401, "zsh"),
            (400, "kitty"),
        ])?;
        fx.relink(402, "/usr/bin/bash")?;
        assert_eq!(requester(&fx.origin(client)?), Some((402, "release.sh")));
        Ok(())
    }

    #[test]
    fn wrapper_is_skipped() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(453, "gpg"), (452, "timeout"), (451, "zsh"), (450, "kitty")])?;
        assert_eq!(requester(&fx.origin(client)?), Some((453, "gpg")));
        Ok(())
    }

    #[test]
    fn application_helpers_are_skipped() -> TestResult {
        // VS Code: a terminal runs in the pty host, a helper of the editor.
        let fx = Fixture::new()?;
        fx.entry("code.desktop", "Name=Visual Studio Code\nExec=code\n")?;
        let client = fx.chain(&[(503, "git"), (502, "nu"), (501, "code"), (500, "code")])?;
        let origin = fx.origin(client)?;
        assert_eq!(label(&origin).as_deref(), Some("git in Visual Studio Code"));

        // The same editor started in its own unit: the pty host is in the
        // walk and passed over because it runs the editor's executable.
        fx.cgroup(
            &[500, 501, 502, 503],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-code-5.scope",
        )?;
        fx.comm(501, "code-ptyhost")?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(503));
        assert_eq!(walk(&origin), "git ← [nu] ← [code-ptyhost] ← code");
        assert_eq!(label(&origin).as_deref(), Some("git in Visual Studio Code"));
        Ok(())
    }

    #[test]
    fn application_holding_the_device_has_no_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.entry("firefox.desktop", "Name=Firefox\nExec=firefox %u\n")?;
        let client = fx.chain(&[(601, "firefox"), (600, "systemd")])?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.requester, None);
        assert_eq!(label(&origin).as_deref(), Some("Firefox"));
        assert_eq!(origin.chain.as_deref(), Some("firefox"));
        Ok(())
    }

    #[test]
    fn without_application_the_topmost_unskipped_process_is_the_requester() -> TestResult {
        let fx = Fixture::new()?;
        let client = fx.chain(&[
            (703, "ssh"),
            (702, "restic"),
            (701, "backup.sh"),
            (700, "systemd"),
        ])?;
        fx.relink(701, "/usr/bin/bash")?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app, None);
        assert_eq!(label(&origin).as_deref(), Some("backup.sh"));
        assert_eq!(walk(&origin), "ssh ← restic ← backup.sh ← [systemd]");
        Ok(())
    }

    #[test]
    fn skipped_client_is_the_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(752, "sudo"), (751, "zsh"), (750, "kitty")])?;
        assert_eq!(label(&fx.origin(client)?).as_deref(), Some("sudo in Kitty"));
        Ok(())
    }

    #[test]
    fn other_users_processes_are_skipped() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(803, "gpg"), (802, "rootd"), (801, "zsh"), (800, "kitty")])?;
        fx.uid(802, 801, 0)?;
        let origin = fx.origin(client)?;
        assert_eq!(requester(&origin), Some((803, "gpg")));
        assert_eq!(walk(&origin), "gpg ← [rootd] ← [zsh] ← kitty");
        Ok(())
    }

    #[test]
    fn extend_skip_passes_over_an_editor_terminal() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (906, "gpg"),
            (905, "git"),
            (904, "claude"),
            (903, "zsh"),
            (902, "nvim"),
            (901, "zsh"),
            (900, "kitty"),
        ])?;
        assert_eq!(requester(&fx.origin(client)?), Some((902, "nvim")));
        let resolver = fx
            .resolver()
            .with_skip(SkipList::new(None, &["nvim".to_owned()]));
        let origin = resolver
            .origin(client, u64::from(client))
            .ok_or(TestError::Missing("pid reused"))?;
        assert_eq!(requester(&origin), Some((904, "claude")));
        Ok(())
    }

    #[test]
    fn skip_replaces_the_defaults() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(953, "gpg"), (952, "claude"), (951, "nu"), (950, "kitty")])?;
        let origin_with = |skip: &[String]| {
            fx.resolver()
                .with_skip(SkipList::new(Some(skip), &[]))
                .origin(client, u64::from(client))
                .ok_or(TestError::Missing("pid reused"))
        };
        let only_nu = origin_with(&["nu".to_owned()])?;
        assert_eq!(requester(&only_nu), Some((952, "claude")));
        let nothing = origin_with(&[])?;
        assert_eq!(requester(&nothing), Some((951, "nu")));
        assert_eq!(walk(&nothing), "gpg ← claude ← nu ← kitty");
        Ok(())
    }

    #[test]
    fn client_in_the_application_unit_is_the_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(1002, "ykman"), (1001, "zsh"), (1000, "kitty")])?;
        fx.cgroup(&[1000, 1001, 1002], KITTY)?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(1002));
        assert_eq!(label(&origin).as_deref(), Some("ykman in Kitty"));
        Ok(())
    }

    #[test]
    fn comm_falls_back_to_the_executable_name() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(1052, "gpg"), (1051, "zsh"), (1050, "kitty")])?;
        fs::remove_file(fx.dir.path().join("proc/1051/comm"))?;
        fs::remove_file(fx.dir.path().join("proc/1052/comm"))?;
        fx.relink(1052, "/usr/bin/gpg (deleted)")?;
        let origin = fx.origin(client)?;
        assert_eq!(requester(&origin), Some((1052, "gpg")));
        assert_eq!(walk(&origin), "gpg ← [zsh] ← kitty");
        Ok(())
    }

    #[test]
    fn unreadable_ancestor_stops_the_walk() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (1104, "gpg"),
            (1103, "git"),
            (1102, "hidden"),
            (1101, "claude"),
            (1100, "kitty"),
        ])?;
        let hidden = fx.dir.path().join("proc/1102");
        fs::remove_file(hidden.join("exe"))?;
        fs::remove_file(hidden.join("comm"))?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(1100));
        assert_eq!(requester(&origin), Some((1103, "git")));
        assert_eq!(origin.chain.as_deref(), Some("gpg ← git"));
        Ok(())
    }

    #[test]
    fn reused_ancestor_is_dropped() -> TestResult {
        let fx = Fixture::new()?;
        fx.chain(&[(1151, "claude"), (1150, "nu")])?;
        let resolver = fx.resolver();
        assert_eq!(resolver.member(1151, 1151).map(|m| m.pid), Some(1151));
        assert!(resolver.member(1151, 1150).is_none());
        Ok(())
    }

    #[test]
    fn application_client_without_readable_executable_has_no_requester() -> TestResult {
        let fx = Fixture::new()?;
        let client = fx.chain(&[(1201, "firefox"), (1200, "systemd")])?;
        fx.cgroup(
            &[1201],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-3.scope",
        )?;
        fs::remove_file(fx.dir.path().join("proc/1201/exe"))?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(1201));
        assert_eq!(origin.requester, None);
        Ok(())
    }

    #[test]
    fn deleted_executable_still_runs_the_application() -> TestResult {
        let fx = Fixture::new()?;
        fx.entry("code.desktop", "Name=Visual Studio Code\nExec=code\n")?;
        let client = fx.chain(&[
            (1253, "git"),
            (1252, "nu"),
            (1251, "helper"),
            (1250, "code"),
        ])?;
        fx.cgroup(
            &[1250, 1251, 1252, 1253],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-code-5.scope",
        )?;
        fx.relink(1251, "/usr/bin/code (deleted)")?;
        assert_eq!(walk(&fx.origin(client)?), "git ← [nu] ← [helper] ← code");
        Ok(())
    }

    #[test]
    fn helper_client_of_the_application_has_no_requester() -> TestResult {
        let fx = Fixture::new()?;
        fx.entry("code.desktop", "Name=Visual Studio Code\nExec=code\n")?;
        let client = fx.chain(&[(1302, "code-helper"), (1301, "nu"), (1300, "code")])?;
        fx.cgroup(
            &[1300, 1301, 1302],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-code-5.scope",
        )?;
        fx.relink(1302, "/usr/bin/code")?;
        let origin = fx.origin(client)?;
        assert_eq!(walk(&origin), "[code-helper] ← [nu] ← code");
        assert_eq!(origin.requester, None);
        assert_eq!(label(&origin).as_deref(), Some("Visual Studio Code"));
        Ok(())
    }

    #[test]
    fn unreadable_uid_skips_nothing() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (1353, "gpg"),
            (1352, "rootd"),
            (1351, "zsh"),
            (1350, "kitty"),
        ])?;
        fx.uid(1352, 1351, 0)?;
        fs::write(fx.dir.path().join("proc/1353/status"), "PPid:\t1352\n")?;
        assert_eq!(requester(&fx.origin(client)?), Some((1352, "rootd")));

        fx.uid(1353, 1352, 1000)?;
        fs::write(fx.dir.path().join("proc/1352/status"), "PPid:\t1351\n")?;
        assert_eq!(requester(&fx.origin(client)?), Some((1352, "rootd")));
        Ok(())
    }

    #[test]
    fn unreadable_application_executable_skips_by_name_only() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[(1402, "kitty"), (1401, "zsh"), (1400, "kitty")])?;
        fx.cgroup(&[1400, 1401, 1402], KITTY)?;
        fs::remove_file(fx.dir.path().join("proc/1400/exe"))?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(1402));
        assert_eq!(walk(&origin), "kitty ← [zsh] ← kitty");
        assert_eq!(requester(&origin), Some((1402, "kitty")));
        Ok(())
    }

    #[test]
    fn early_stop_in_an_application_unit_guesses_among_the_read() -> TestResult {
        let fx = Fixture::new()?;
        fx.kitty()?;
        let client = fx.chain(&[
            (1454, "gpg"),
            (1453, "git"),
            (1452, "hidden"),
            (1451, "claude"),
            (1450, "kitty"),
        ])?;
        fx.cgroup(&[1450, 1451, 1452, 1453, 1454], KITTY)?;
        fs::remove_file(fx.dir.path().join("proc/1452/exe"))?;
        fs::remove_file(fx.dir.path().join("proc/1452/comm"))?;
        let origin = fx.origin(client)?;
        assert_eq!(origin.app.as_ref().and_then(|a| a.pid), Some(1454));
        assert_eq!(walk(&origin), "gpg ← git");
        assert_eq!(requester(&origin), Some((1453, "git")));
        Ok(())
    }

    #[test]
    fn tmux_server_is_skipped() -> TestResult {
        let fx = Fixture::new()?;
        let client = fx.chain(&[
            (1503, "gpg"),
            (1502, "zsh"),
            (1501, "tmux: server"),
            (1500, "systemd"),
        ])?;
        let origin = fx.origin(client)?;
        assert_eq!(walk(&origin), "gpg ← [zsh] ← [tmux: server] ← [systemd]");
        assert_eq!(requester(&origin), Some((1503, "gpg")));
        Ok(())
    }

    #[test]
    fn chain_names_are_sanitized_and_capped() -> TestResult {
        let fx = Fixture::new()?;
        let client = fx.chain(&[(1201, "gpg"), (1200, "evil")])?;
        fx.comm(1200, "a ← b\nc\u{202e}")?;
        let long = "x".repeat(60);
        fx.comm(1201, &long)?;
        let origin = fx.origin(client)?;
        let chain = origin.chain.ok_or(TestError::Missing("no chain"))?;
        assert_eq!(chain, format!("{} ← a b c", "x".repeat(CHAIN_NAME_MAX)));
        assert_eq!(chain.matches(CHAIN_SEPARATOR).count(), 1);
        Ok(())
    }
}
