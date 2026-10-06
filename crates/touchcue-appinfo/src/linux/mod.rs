//! Linux attribution of device holders to processes and applications.

mod bounded;
pub mod desktop;
pub mod icon;
pub mod procfs;
#[cfg(test)]
mod test_error;

use std::path::{Path, PathBuf};

use touchcue_core::{AppInfo, ProcessInfo};
use tracing::debug;

use touchcue_core::text::sanitize;

use crate::TEXT_MAX;
use crate::unit::{self, UnitApp};
use desktop::ExeIndex;

/// Maximum number of processes visited from a pid up its ancestry.
const MAX_ANCESTRY: usize = 32;

/// Resolves pids to processes and applications from a procfs tree and XDG data directories.
#[derive(Debug, Clone)]
pub struct Resolver {
    proc_root: PathBuf,
    data_dirs: Vec<PathBuf>,
    icon_size: u16,
    locales: Vec<String>,
}

impl Resolver {
    /// Creates a resolver; desktop entry names are localized from the current locale environment.
    #[must_use]
    pub fn new(proc_root: PathBuf, data_dirs: Vec<PathBuf>, icon_size: u16) -> Self {
        Self {
            proc_root,
            data_dirs,
            icon_size,
            locales: desktop::locales(),
        }
    }

    /// Creates a resolver over `/proc` and the XDG data directories, with 64-pixel icons.
    #[must_use]
    pub fn system() -> Self {
        Self::new(PathBuf::from("/proc"), desktop::default_data_dirs(), 64)
    }

    /// Returns the sorted pids, other than this process, holding `node` open, each with its start time.
    ///
    /// Pass the start time to [`Self::process`] and [`Self::app`] so that a
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

    /// Returns the application owning `pid`, searching it and up to 31 ancestors.
    ///
    /// The nearest process in a systemd application unit wins; its desktop
    /// entry supplies name, icon and window class. When the unit's
    /// application id has no entry, the entry matching that process's
    /// executable by [`ExeIndex::find`], or else by [`ExeIndex::find_by_dir`],
    /// supplies them and its file stem becomes the id; when neither exists,
    /// the unit's application id is both id and name. When no process is in
    /// such a unit, the nearest process whose executable matches an entry by
    /// [`ExeIndex::find`] wins, and otherwise the nearest one matching by
    /// [`ExeIndex::find_by_dir`]. `exe` and `pid` describe the matched
    /// process. Returns `None` when the start time of `pid` differs from
    /// `expected_start` before or after resolving, or the matched process
    /// was reused.
    ///
    /// The result is a claim, not a verified identity: any process of the
    /// same user can choose its unit name and executable name, and so can
    /// present itself as any installed application. Text fields are passed
    /// through [`sanitize`] with a 128-char cap.
    #[must_use]
    pub fn app(&self, pid: u32, expected_start: u64) -> Option<AppInfo> {
        self.guarded(pid, expected_start, || {
            let ancestry = self.ancestry(pid);
            let (app, matched, started) = ancestry
                .iter()
                .find_map(|&(ancestor, started)| {
                    Some((self.app_from_unit(ancestor)?, ancestor, started))
                })
                .or_else(|| {
                    let index = ExeIndex::build(&self.data_dirs);
                    let nearest = |resolve: fn(&Self, &ExeIndex, u32) -> Option<AppInfo>| {
                        ancestry.iter().find_map(|&(ancestor, started)| {
                            Some((resolve(self, &index, ancestor)?, ancestor, started))
                        })
                    };
                    nearest(Self::app_from_exe).or_else(|| nearest(Self::app_from_exe_dir))
                })?;
            (procfs::start_time(&self.proc_root, matched) == Some(started)).then_some(app)
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
        let name = exe.file_name()?.to_str()?;
        let name = name.strip_suffix(" (deleted)").unwrap_or(name);
        let path = index.find(name)?;
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
            icon: entry
                .icon
                .and_then(|icon| icon::resolve_icon(&icon, self.icon_size)),
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
                icon_size: 64,
                locales: Vec::new(),
            }
        }
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
}
