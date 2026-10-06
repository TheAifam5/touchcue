//! Lookup of freedesktop `.desktop` entries in XDG data directories.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::DesktopEntry;
use tracing::debug;

use super::bounded::{logged, read_bounded};
use touchcue_core::text::sanitize;

use crate::TEXT_MAX;

/// Largest `.desktop` file read, in bytes.
pub const ENTRY_MAX: usize = 64 * 1024;

/// Most `.desktop` files read from one data directory by [`ExeIndex::build`].
pub const INDEX_MAX_PER_DIR: usize = 4096;

/// Programs that run another program, so naming one in `Exec` or `TryExec` does not identify an application.
const WRAPPERS: &[&str] = &[
    "env", "sh", "bash", "dash", "zsh", "fish", "python", "python3", "perl", "ruby", "node",
    "java", "flatpak", "snap",
];

/// Fixed list of directories shared by unrelated programs, where a common parent does not tie an executable to an entry.
///
/// Directories of the form `/nix/store/<hash>/bin` and the home-relative
/// [`SHARED_HOME_DIRS`] are excluded as well.
const SHARED_DIRS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/games",
    "/usr/local/games",
    "/opt/bin",
    "/snap/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

/// Fixed list of shared program directories relative to the home directory.
const SHARED_HOME_DIRS: &[&str] = &[".local/bin", "bin", ".cargo/bin", "go/bin"];

/// Fields of a desktop entry used to describe an application.
///
/// `name` and `wm_class` are passed through [`sanitize`] with a 128-char cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// `Name`, localized for the given locales.
    pub name: Option<String>,
    /// `Icon`, an icon name or an absolute path, unvalidated.
    pub icon: Option<String>,
    /// `StartupWMClass`.
    pub wm_class: Option<String>,
}

/// Returns the first existing `<dir>/applications/<app_id>.desktop`.
///
/// Returns `None` when `app_id` is empty or contains `/` or NUL.
#[must_use]
pub fn find_entry(data_dirs: &[PathBuf], app_id: &str) -> Option<PathBuf> {
    if app_id.is_empty() || app_id.contains(['/', '\0']) {
        return None;
    }
    let file = format!("{app_id}.desktop");
    data_dirs
        .iter()
        .map(|dir| dir.join("applications").join(&file))
        .find(|path| path.is_file())
}

/// Executable names of the desktop entries in a list of data directories.
#[derive(Debug, Clone, Default)]
pub struct ExeIndex {
    entries: Vec<Indexed>,
    homes: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct Indexed {
    path: PathBuf,
    wm_class: Option<String>,
    programs: Vec<String>,
    program_dirs: Vec<PathBuf>,
    visible: bool,
    in_home: bool,
    dir_rank: usize,
}

impl ExeIndex {
    /// Reads the `applications/*.desktop` files, in `data_dirs` order and then by file name.
    ///
    /// At most [`INDEX_MAX_PER_DIR`] files are read from each directory, the
    /// first by file name. Unreadable directories and files, files over
    /// [`ENTRY_MAX`] bytes and unparsable files are skipped. Each entry's
    /// absolute `TryExec` and first `Exec` token are canonicalized once;
    /// bare names and programs that cannot be resolved are left out of
    /// [`Self::find_by_dir`]. The home directory is taken from `$HOME`.
    #[must_use]
    pub fn build(data_dirs: &[PathBuf]) -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        Self::build_capped(data_dirs, INDEX_MAX_PER_DIR, home.as_deref())
    }

    fn build_capped(data_dirs: &[PathBuf], max_per_dir: usize, home: Option<&Path>) -> Self {
        let mut homes: Vec<PathBuf> = Vec::new();
        if let Some(home) = home.filter(|home| home.is_absolute()) {
            homes.push(home.to_path_buf());
            match fs::canonicalize(home) {
                Ok(canonical) => homes.push(canonical),
                Err(error) => {
                    debug!(path = %logged(home), error = &error as &dyn std::error::Error, "home unresolvable");
                }
            }
        }
        homes.dedup();
        let mut entries = Vec::new();
        for (dir_rank, dir) in data_dirs.iter().enumerate() {
            let in_home = homes.iter().any(|home| dir.starts_with(home));
            let applications = dir.join("applications");
            let listing = match fs::read_dir(&applications) {
                Ok(listing) => listing,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        debug!(path = %logged(&applications), error = &error as &dyn std::error::Error, "desktop directory unreadable");
                    }
                    continue;
                }
            };
            let mut files: Vec<PathBuf> = Vec::new();
            for entry in listing {
                match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.extension().is_some_and(|ext| ext == "desktop") {
                            files.push(path);
                        }
                    }
                    Err(error) => {
                        debug!(path = %logged(&applications), error = &error as &dyn std::error::Error, "desktop directory entry unreadable");
                    }
                }
            }
            files.sort();
            if files.len() > max_per_dir {
                debug!(
                    found = files.len(),
                    limit = max_per_dir,
                    "desktop entry limit reached"
                );
                files.truncate(max_per_dir);
            }
            entries.extend(files.into_iter().filter_map(|path| {
                let entry = parse(&path, None)?;
                let tokens: Vec<&str> = [entry.try_exec(), entry.exec().and_then(first_token)]
                    .into_iter()
                    .flatten()
                    .filter(|token| {
                        let program = basename(token);
                        !program.is_empty() && !WRAPPERS.contains(&program)
                    })
                    .collect();
                Some(Indexed {
                    wm_class: entry.startup_wm_class().map(str::to_owned),
                    programs: tokens
                        .iter()
                        .map(|token| basename(token).to_owned())
                        .collect(),
                    program_dirs: tokens
                        .iter()
                        .filter_map(|token| program_dir(token))
                        .collect(),
                    visible: !entry.hidden() && !entry.no_display(),
                    in_home,
                    dir_rank,
                    path,
                })
            }));
        }
        Self { entries, homes }
    }

    /// Returns the first entry matching the executable name `exe_basename`.
    ///
    /// An entry matches when its `StartupWMClass` equals the name ignoring
    /// ASCII case, or when the base name of `TryExec` or of the first `Exec`
    /// token equals it and is not a generic wrapper such as `sh` or `env`.
    #[must_use]
    pub fn find(&self, exe_basename: &str) -> Option<&Path> {
        if exe_basename.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|entry| {
                entry
                    .wm_class
                    .as_deref()
                    .is_some_and(|class| class.eq_ignore_ascii_case(exe_basename))
                    || entry.programs.iter().any(|program| program == exe_basename)
            })
            .map(|entry| entry.path.as_path())
    }

    /// Returns the entry whose program lives in the same directory as the executable `exe`.
    ///
    /// `exe` must be a canonical absolute path, as `/proc/<pid>/exe` reports
    /// it. An entry matches when the canonical `TryExec` or first `Exec`
    /// program has the parent directory of `exe` as its parent. Nothing
    /// matches when that directory is one of the fixed shared directories
    /// such as `/usr/bin`, `/nix/store/<hash>/bin` or `~/.local/bin`. Among
    /// several matches, entries without `Hidden` or `NoDisplay` win, then
    /// entries from data directories outside the home directory, then the
    /// earlier data directory, then the lexically smallest desktop id.
    #[must_use]
    pub fn find_by_dir(&self, exe: &Path) -> Option<&Path> {
        let dir = exe.parent().filter(|dir| !self.is_shared(dir))?;
        self.entries
            .iter()
            .filter(|entry| entry.program_dirs.iter().any(|program| program == dir))
            .min_by(|a, b| {
                (!a.visible, a.in_home, a.dir_rank, a.path.file_stem()).cmp(&(
                    !b.visible,
                    b.in_home,
                    b.dir_rank,
                    b.path.file_stem(),
                ))
            })
            .map(|entry| entry.path.as_path())
    }

    fn is_shared(&self, dir: &Path) -> bool {
        SHARED_DIRS.iter().any(|shared| dir == Path::new(shared))
            || (dir.file_name() == Some(OsStr::new("bin"))
                && dir.parent().and_then(Path::parent) == Some(Path::new("/nix/store")))
            || self.homes.iter().any(|home| {
                SHARED_HOME_DIRS
                    .iter()
                    .any(|shared| dir == home.join(shared))
            })
    }
}

/// Returns the first `.desktop` file matching the executable name `exe_basename`.
///
/// Builds an [`ExeIndex`] over `data_dirs` and applies [`ExeIndex::find`].
#[must_use]
pub fn find_by_exe(data_dirs: &[PathBuf], exe_basename: &str) -> Option<PathBuf> {
    ExeIndex::build(data_dirs)
        .find(exe_basename)
        .map(Path::to_path_buf)
}

/// Returns the parent directory of the canonical path of the absolute `program`.
fn program_dir(program: &str) -> Option<PathBuf> {
    let program = Path::new(program);
    if !program.is_absolute() {
        return None;
    }
    let canonical = match fs::canonicalize(program) {
        Ok(path) => path,
        Err(error) => {
            debug!(path = %logged(program), error = &error as &dyn std::error::Error, "desktop program unresolvable");
            return None;
        }
    };
    if !canonical.is_file() {
        return None;
    }
    canonical.parent().map(Path::to_path_buf)
}

fn first_token(exec: &str) -> Option<&str> {
    let exec = exec.trim_start();
    match exec.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next(),
        None => exec.split_whitespace().next(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parse(path: &Path, locales: Option<&[String]>) -> Option<DesktopEntry> {
    let text = read_bounded(path, ENTRY_MAX)?;
    match DesktopEntry::from_str(path, &text, locales) {
        Ok(entry) => Some(entry),
        Err(error) => {
            debug!(path = %logged(path), error = &error as &dyn std::error::Error, "desktop entry unparsable");
            None
        }
    }
}

/// Reads the `Name`, `Icon` and `StartupWMClass` of the entry at `path`.
///
/// Returns `None` when the file is not a regular file, is larger than
/// [`ENTRY_MAX`] bytes, or cannot be read or parsed.
#[must_use]
pub fn read_entry(path: &Path, locales: &[String]) -> Option<Entry> {
    let entry = parse(path, Some(locales))?;
    Some(Entry {
        name: entry
            .name(locales)
            .and_then(|name| sanitize(&name, TEXT_MAX)),
        icon: entry.icon().map(str::to_owned),
        wm_class: entry
            .startup_wm_class()
            .and_then(|class| sanitize(class, TEXT_MAX)),
    })
}

/// Returns `$XDG_DATA_HOME` (or `$HOME/.local/share`) followed by `$XDG_DATA_DIRS`.
///
/// Empty and relative values are ignored, and `$XDG_DATA_DIRS` defaults to
/// `/usr/local/share:/usr/share`.
#[must_use]
pub fn default_data_dirs() -> Vec<PathBuf> {
    data_dirs_from(
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
        env::var_os("XDG_DATA_DIRS").as_deref(),
    )
}

fn data_dirs_from(
    data_home: Option<&OsStr>,
    home: Option<&OsStr>,
    data_dirs: Option<&OsStr>,
) -> Vec<PathBuf> {
    let absolute = |value: &OsStr| Some(PathBuf::from(value)).filter(|p| p.is_absolute());
    let mut dirs: Vec<PathBuf> = data_home
        .and_then(absolute)
        .or_else(|| home.and_then(absolute).map(|h| h.join(".local/share")))
        .into_iter()
        .collect();
    let system: Vec<PathBuf> = data_dirs
        .into_iter()
        .flat_map(env::split_paths)
        .filter(|p| p.is_absolute())
        .collect();
    if system.is_empty() {
        dirs.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from));
    } else {
        dirs.extend(system);
    }
    dirs
}

/// Returns the message locales from `LC_ALL`, `LC_MESSAGES` or `LANG`, most specific first.
///
/// The first non-empty variable is used. Encoding and modifier are stripped,
/// so `pl_PL.UTF-8` yields `pl_PL` then `pl`. `C` and `POSIX` yield nothing.
#[must_use]
pub fn locales() -> Vec<String> {
    let value = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(env::var_os)
        .find(|value| !value.is_empty());
    match value.map(std::ffi::OsString::into_string) {
        Some(Ok(value)) => locales_from(&value),
        Some(Err(_)) => {
            debug!("locale variable is not UTF-8; using no locales");
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn locales_from(value: &str) -> Vec<String> {
    let locale = value.split(['.', '@']).next().unwrap_or_default().trim();
    if locale.is_empty() || locale == "C" || locale == "POSIX" {
        return Vec::new();
    }
    let mut out = vec![locale.to_owned()];
    if let Some((language, _)) = locale.split_once('_') {
        out.push(language.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::linux::test_error::{TestError, TestResult};

    fn write_entry(dir: &Path, file: &str, body: &str) -> std::io::Result<()> {
        let apps = dir.join("applications");
        fs::create_dir_all(&apps)?;
        fs::write(
            apps.join(file),
            format!("[Desktop Entry]\nType=Application\n{body}"),
        )
    }

    #[test]
    fn finds_entry_in_first_dir() -> TestResult {
        let home = tempfile::tempdir()?;
        let system = tempfile::tempdir()?;
        write_entry(system.path(), "kitty.desktop", "Name=kitty\n")?;
        write_entry(system.path(), "other.desktop", "Name=other\n")?;
        write_entry(home.path(), "other.desktop", "Name=mine\n")?;
        let dirs = [home.path().to_owned(), system.path().to_owned()];
        assert_eq!(
            find_entry(&dirs, "kitty"),
            Some(system.path().join("applications/kitty.desktop"))
        );
        assert_eq!(
            find_entry(&dirs, "other"),
            Some(home.path().join("applications/other.desktop"))
        );
        assert_eq!(find_entry(&dirs, "missing"), None);
        assert_eq!(find_entry(&dirs, "../applications/kitty"), None);
        assert_eq!(find_entry(&dirs, ""), None);
        Ok(())
    }

    #[test]
    fn reads_localized_entry() -> TestResult {
        let dir = tempfile::tempdir()?;
        write_entry(
            dir.path(),
            "org.example.App.desktop",
            "Name=Files\nName[pl]=Pliki\nIcon=org.example.App\nStartupWMClass=example\n",
        )?;
        let path = dir.path().join("applications/org.example.App.desktop");
        let entry = read_entry(&path, &locales_from("pl_PL.UTF-8"))
            .ok_or(TestError::Missing("unreadable"))?;
        assert_eq!(
            entry,
            Entry {
                name: Some("Pliki".to_owned()),
                icon: Some("org.example.App".to_owned()),
                wm_class: Some("example".to_owned()),
            }
        );
        let entry = read_entry(&path, &[]).ok_or(TestError::Missing("unreadable"))?;
        assert_eq!(entry.name.as_deref(), Some("Files"));
        assert_eq!(read_entry(&dir.path().join("missing.desktop"), &[]), None);
        Ok(())
    }

    #[test]
    fn sanitizes_entry_text() -> TestResult {
        let dir = tempfile::tempdir()?;
        write_entry(
            dir.path(),
            "x.desktop",
            "Name=Evil\u{1b}[2J Name\nStartupWMClass=a\u{7}b\n",
        )?;
        let entry = read_entry(&dir.path().join("applications/x.desktop"), &[])
            .ok_or(TestError::Missing("unreadable"))?;
        assert_eq!(entry.name.as_deref(), Some("Evil [2J Name"));
        assert_eq!(entry.wm_class.as_deref(), Some("a b"));
        Ok(())
    }

    #[test]
    fn skips_oversized_and_special_entries() -> TestResult {
        let dir = tempfile::tempdir()?;
        let apps = dir.path().join("applications");
        fs::create_dir_all(&apps)?;
        let padding = "#".repeat(ENTRY_MAX);
        fs::write(
            apps.join("big.desktop"),
            format!("[Desktop Entry]\nName=Big\nExec=tool\n{padding}\n"),
        )?;
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            apps.join("fifo.desktop"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        std::os::unix::fs::symlink("/dev/zero", apps.join("zero.desktop"))?;
        let dirs = [dir.path().to_owned()];
        assert_eq!(read_entry(&apps.join("big.desktop"), &[]), None);
        assert_eq!(read_entry(&apps.join("fifo.desktop"), &[]), None);
        assert_eq!(find_by_exe(&dirs, "tool"), None);
        assert_eq!(find_entry(&dirs, "fifo"), None);
        Ok(())
    }

    #[test]
    fn escaped_slash_unit_is_rejected() {
        let unit = crate::unit::parse(r"app-x\x2fy-1.scope");
        let app_id = unit.map(|unit| unit.app_id);
        assert_eq!(app_id.as_deref(), Some("x/y"));
        let dirs = [PathBuf::from("/usr/share")];
        assert_eq!(app_id.and_then(|id| find_entry(&dirs, &id)), None);
    }

    #[test]
    fn finds_by_exe() -> TestResult {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        write_entry(
            first.path(),
            "b.desktop",
            "Name=B\nExec=\"/opt/My App/bin/tool\" %U\n",
        )?;
        write_entry(
            first.path(),
            "a.desktop",
            "Name=A\nExec=/usr/bin/tool --flag\n",
        )?;
        write_entry(first.path(), "broken.desktop", "not an entry")?;
        write_entry(
            second.path(),
            "wm.desktop",
            "Name=W\nExec=env launcher\nStartupWMClass=Viewer\n",
        )?;
        write_entry(
            second.path(),
            "try.desktop",
            "Name=T\nExec=sh -c x\nTryExec=/usr/bin/probe\n",
        )?;
        fs::write(first.path().join("applications/notes.txt"), "Exec=tool")?;
        let dirs = [first.path().to_owned(), second.path().to_owned()];
        let found = |exe| find_by_exe(&dirs, exe);
        assert_eq!(
            found("tool"),
            Some(first.path().join("applications/a.desktop"))
        );
        assert_eq!(
            found("viewer"),
            Some(second.path().join("applications/wm.desktop"))
        );
        assert_eq!(
            found("probe"),
            Some(second.path().join("applications/try.desktop"))
        );
        assert_eq!(found("sh"), None);
        assert_eq!(found("launch"), None);
        assert_eq!(found(""), None);
        Ok(())
    }

    #[test]
    fn finds_by_dir() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let app = root.join("opt/browser");
        let bin = root.join("bin");
        fs::create_dir_all(&app)?;
        fs::create_dir_all(&bin)?;
        fs::write(app.join("browser"), "")?;
        fs::write(app.join("browser-nightly"), "#!/bin/sh\n")?;
        std::os::unix::fs::symlink(app.join("browser-nightly"), bin.join("browser-nightly"))?;
        let wrapper = bin.join("browser-nightly");
        let wrapper = wrapper
            .to_str()
            .ok_or(TestError::Missing("non-UTF-8 temp path"))?;
        let home = root.join("home");
        let (user, system) = (home.join(".local/share"), root.join("system"));
        let exec = format!("Name=B\nExec={wrapper} %U\n");
        write_entry(&system, "z.desktop", &exec)?;
        write_entry(&system, "b.desktop", &exec)?;
        write_entry(&system, "a.desktop", &format!("{exec}NoDisplay=true\n"))?;
        write_entry(
            &system,
            "missing.desktop",
            "Name=M\nExec=/nonexistent/gone\n",
        )?;
        write_entry(&user, "a.desktop", &exec)?;
        let exe = app.join("browser");

        let index = ExeIndex::build_capped(std::slice::from_ref(&system), INDEX_MAX_PER_DIR, None);
        assert_eq!(index.find("browser"), None);
        assert_eq!(
            index.find_by_dir(&exe),
            Some(system.join("applications/b.desktop").as_path())
        );
        assert_eq!(index.find_by_dir(&root.join("other/browser")), None);

        let dirs = [user.clone(), system.clone()];
        assert_eq!(
            ExeIndex::build_capped(&dirs, INDEX_MAX_PER_DIR, Some(&home)).find_by_dir(&exe),
            Some(system.join("applications/b.desktop").as_path())
        );
        assert_eq!(
            ExeIndex::build_capped(&dirs, INDEX_MAX_PER_DIR, None).find_by_dir(&exe),
            Some(user.join("applications/a.desktop").as_path())
        );
        Ok(())
    }

    #[test]
    fn bare_and_home_bin_programs_never_match_by_dir() -> TestResult {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        let home = root.join("home");
        let local_bin = home.join(".local/bin");
        fs::create_dir_all(&local_bin)?;
        fs::write(local_bin.join("tool"), "")?;
        fs::write(local_bin.join("tool-launcher"), "#!/bin/sh\n")?;
        let launcher = local_bin.join("tool-launcher");
        let launcher = launcher
            .to_str()
            .ok_or(TestError::Missing("non-UTF-8 temp path"))?;
        let data = root.join("share");
        write_entry(&data, "abs.desktop", &format!("Name=A\nExec={launcher}\n"))?;
        write_entry(&data, "bare.desktop", "Name=B\nExec=tool-launcher\n")?;
        let dirs = [data.clone()];
        let exe = local_bin.join("tool");
        assert_eq!(
            ExeIndex::build_capped(&dirs, INDEX_MAX_PER_DIR, Some(&home)).find_by_dir(&exe),
            None
        );
        assert_eq!(
            ExeIndex::build_capped(&dirs, INDEX_MAX_PER_DIR, None).find_by_dir(&exe),
            Some(data.join("applications/abs.desktop").as_path())
        );
        Ok(())
    }

    #[test]
    fn shared_dirs_never_match_by_dir() {
        let index = ExeIndex::build_capped(&[], INDEX_MAX_PER_DIR, Some(Path::new("/home/u")));
        for dir in SHARED_DIRS {
            assert!(index.is_shared(Path::new(dir)), "{dir}");
        }
        for dir in SHARED_HOME_DIRS {
            assert!(index.is_shared(&Path::new("/home/u").join(dir)), "{dir}");
        }
        assert!(index.is_shared(Path::new("/nix/store/abc-hello-1.0/bin")));
        assert!(!index.is_shared(Path::new("/nix/store/abc-hello-1.0/lib")));
        assert!(!index.is_shared(Path::new("/nix/store/bin")));
        assert!(!index.is_shared(Path::new("/opt/brave.com/brave")));
        assert!(!index.is_shared(Path::new("/usr/lib/firefox")));
        assert!(!index.is_shared(Path::new("/home/u/apps/tool")));
        assert_eq!(index.find_by_dir(Path::new("/usr/bin/x")), None);
    }

    #[test]
    fn index_reads_at_most_cap_per_dir() -> TestResult {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        for (file, exe) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            write_entry(
                first.path(),
                &format!("{file}.desktop"),
                &format!("Name={file}\nExec={exe}\n"),
            )?;
        }
        write_entry(second.path(), "d.desktop", "Name=d\nExec=delta\n")?;
        let dirs = [first.path().to_owned(), second.path().to_owned()];
        let index = ExeIndex::build_capped(&dirs, 2, None);
        assert!(index.find("alpha").is_some());
        assert!(index.find("beta").is_some());
        assert_eq!(index.find("gamma"), None);
        assert!(index.find("delta").is_some());
        assert!(ExeIndex::build(&dirs).find("gamma").is_some());
        Ok(())
    }

    #[test]
    fn data_dirs_follow_xdg() {
        let os = |s: &'static str| Some(OsStr::new(s));
        assert_eq!(
            data_dirs_from(os("/xdg/home"), os("/home/u"), os("/a:/b")),
            ["/xdg/home", "/a", "/b"].map(PathBuf::from)
        );
        assert_eq!(
            data_dirs_from(os("relative"), os("/home/u"), None),
            ["/home/u/.local/share", "/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
        assert_eq!(
            data_dirs_from(None, None, os("")),
            ["/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
        assert_eq!(
            data_dirs_from(None, None, os("rel::/c")),
            [PathBuf::from("/c")]
        );
    }

    #[test]
    fn locales_strip_encoding_and_modifier() {
        assert_eq!(locales_from("pl_PL.UTF-8"), ["pl_PL", "pl"]);
        assert_eq!(locales_from("sr_RS@latin"), ["sr_RS", "sr"]);
        assert_eq!(locales_from("de"), ["de"]);
        assert_eq!(locales_from("C.UTF-8"), Vec::<String>::new());
        assert_eq!(locales_from("POSIX"), Vec::<String>::new());
        assert_eq!(locales_from(""), Vec::<String>::new());
    }
}
