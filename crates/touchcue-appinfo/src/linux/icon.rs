//! Resolution of icon names and paths to files, following the freedesktop
//! Icon Theme Specification.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tracing::{debug, trace};

use super::bounded::{logged, read_text};
use super::ini::{self, Ini};

/// Theme searched after the selected theme and its parents.
pub const DEFAULT_THEME: &str = "hicolor";
/// Largest `index.theme` read, in bytes.
pub const INDEX_MAX: usize = 256 * 1024;
/// Image formats looked up and accepted, compared ignoring ASCII case for
/// absolute paths; the popup draws only these.
const EXTENSIONS: &[&str] = &["png", "svg"];
/// Extensions removed from an icon name before it is looked up, as some
/// desktop entries name `Icon=app.png`.
const NAME_EXTENSIONS: &[&str] = &["png", "svg", "xpm"];
/// Most directories of one theme considered.
const MAX_THEME_DIRS: usize = 4096;
/// Most themes searched, the selected theme, its ancestors and hicolor included.
const MAX_THEMES: usize = 16;
/// Most theme names tried while following `Inherits`, missing themes included.
const MAX_VISITED: usize = 64;
/// Deepest `Inherits` chain followed from the selected theme.
const MAX_INHERIT_DEPTH: usize = 8;
/// Most files checked for one icon name; a lookup that reaches it returns
/// what it found so far.
const MAX_LOOKUP_CHECKS: usize = 16_384;
/// Most icon names whose result is remembered; the memory is emptied when full.
const MAX_CACHED: usize = 512;
/// Directory of unthemed icons searched last.
const PIXMAPS: &str = "/usr/share/pixmaps";

/// Returns the directories searched for icon themes and unthemed icons, in
/// precedence order: `~/.icons`, `icons` in each of `data_dirs`, then
/// `/usr/share/pixmaps`.
///
/// `home` is ignored unless absolute.
#[must_use]
pub fn base_dirs(home: Option<&Path>, data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| home.join(".icons"))
        .into_iter()
        .chain(data_dirs.iter().map(|dir| dir.join("icons")))
        .chain([PathBuf::from(PIXMAPS)])
        .collect()
}

/// Returns whether `name` can name an icon theme directory: non-empty, free
/// of control characters and `/`, and neither `.` nor `..`.
#[must_use]
pub fn is_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

/// Results of name lookups: the file found, or `None` for none.
type Cache = BTreeMap<String, Option<PathBuf>>;

/// Finds icon files in an icon theme, its parents and hicolor at one size.
///
/// The themes are read once, by [`Self::load`] or the first lookup; a theme
/// installed later is not seen. The result of a name lookup is remembered:
/// a found file is checked to still exist on every lookup, and a name found
/// in no theme stays unfound. Clones share the remembered results.
#[derive(Debug, Clone)]
pub struct IconLookup {
    base_dirs: Vec<PathBuf>,
    home: Option<PathBuf>,
    theme: String,
    size: u16,
    chain: Arc<OnceLock<Vec<Theme>>>,
    cache: Arc<Mutex<Cache>>,
}

impl IconLookup {
    /// Creates a lookup of `theme` in `base_dirs` at `size` pixels; `home`
    /// expands `~/` in [`Self::resolve_config`].
    #[must_use]
    pub fn new(base_dirs: Vec<PathBuf>, home: Option<PathBuf>, theme: String, size: u16) -> Self {
        Self {
            base_dirs,
            home,
            theme,
            size,
            chain: Arc::new(OnceLock::new()),
            cache: Arc::new(Mutex::new(Cache::new())),
        }
    }

    /// Returns a lookup with `theme` selected in place of the current one,
    /// with no themes read and nothing remembered.
    #[must_use]
    pub fn with_theme(&self, theme: String) -> Self {
        Self::new(self.base_dirs.clone(), self.home.clone(), theme, self.size)
    }

    /// Returns the selected theme.
    #[must_use]
    pub fn theme(&self) -> &str {
        &self.theme
    }

    /// Reads the selected theme, its ancestors and hicolor, unless already
    /// read, and returns whether the selected theme was found.
    #[must_use]
    pub fn load(&self) -> bool {
        self.chain()
            .first()
            .is_some_and(|theme| theme.name == self.theme)
    }

    /// Returns the file for `icon`, an absolute path or an icon name.
    ///
    /// An absolute path is returned only when it is a regular file, after
    /// following symlinks, with a `.png` or `.svg` extension. A name, with a
    /// `.png`, `.svg` or `.xpm` extension removed, is looked up in the
    /// selected theme, its parents, hicolor, and then unthemed in each base
    /// directory, checking at most [`MAX_LOOKUP_CHECKS`] files. A value
    /// containing control characters, a relative path containing `/`, and a
    /// non-UTF-8 result yield `None`.
    #[must_use]
    pub fn resolve(&self, icon: &str) -> Option<String> {
        if icon.is_empty() || icon.chars().any(char::is_control) {
            return None;
        }
        let path = Path::new(icon);
        if path.is_absolute() {
            let image = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    EXTENSIONS
                        .iter()
                        .any(|known| ext.eq_ignore_ascii_case(known))
                });
            return (image && is_file(path)).then(|| icon.to_owned());
        }
        if icon.contains('/') {
            return None;
        }
        let name = strip_extension(icon);
        if name.is_empty() {
            return None;
        }
        let found = self.find(name)?;
        match found.into_os_string().into_string() {
            Ok(path) => Some(path),
            Err(path) => {
                debug!(path = %logged(Path::new(&path)), "icon path is not UTF-8");
                None
            }
        }
    }

    /// Returns the file for a configured `icon`, taking a leading `~/`
    /// relative to the home directory; other values as [`Self::resolve`].
    ///
    /// A `~/` value yields `None` without a home directory, and when the rest
    /// is absolute or contains `..`.
    #[must_use]
    pub fn resolve_config(&self, icon: &str) -> Option<String> {
        let Some(relative) = icon.strip_prefix("~/") else {
            return self.resolve(icon);
        };
        let inside = Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        if !inside {
            debug!("`~/` icon leaves the home directory; skipping it");
            return None;
        }
        let Some(home) = self.home.as_deref().filter(|home| home.is_absolute()) else {
            debug!("no absolute home directory; skipping a `~/` icon");
            return None;
        };
        let path = home.join(relative);
        let Some(path) = path.to_str() else {
            debug!(path = %logged(&path), "icon path is not UTF-8");
            return None;
        };
        self.resolve(path)
    }

    /// Returns the file of icon `name`, from the remembered result while its
    /// file exists, else from a lookup.
    fn find(&self, name: &str) -> Option<PathBuf> {
        let cached = self.cached().get(name).cloned();
        match cached {
            Some(Some(file)) if is_file(&file) => return Some(file),
            Some(None) => return None,
            Some(Some(_)) | None => {}
        }
        let mut checks = Checks::new(MAX_LOOKUP_CHECKS);
        let size = u32::from(self.size);
        let found = self
            .chain()
            .iter()
            .find_map(|theme| theme.lookup(name, size, &mut checks))
            .or_else(|| self.unthemed(name, &mut checks));
        if checks.exhausted() {
            debug!(icon = %logged(Path::new(name)), "icon lookup reached its file check limit");
            return found;
        }
        if checks.failed {
            // An unreadable file may become readable, so the result is not final.
            return found;
        }
        let mut cache = self.cached();
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(name.to_owned(), found.clone());
        found
    }

    fn cached(&self) -> MutexGuard<'_, Cache> {
        // The cache holds only finished results, so a panicking holder left it consistent.
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns the themes searched, in order: the selected theme and its
    /// ancestors depth-first, then hicolor.
    fn chain(&self) -> &[Theme] {
        self.chain.get_or_init(|| {
            let mut chain = Vec::new();
            let mut visited = Vec::new();
            self.visit(&self.theme, 0, &mut chain, &mut visited);
            if let Some(hicolor) = load_theme(&self.base_dirs, DEFAULT_THEME) {
                chain.push(hicolor);
            } else {
                debug!("hicolor icon theme not found");
            }
            debug!(theme = %logged(Path::new(&self.theme)), themes = chain.len(), "icon themes loaded");
            chain
        })
    }

    /// Adds theme `name` and its ancestors to `chain`, except hicolor, which
    /// [`Self::chain`] adds last; missing themes do not count towards
    /// [`MAX_THEMES`].
    fn visit(&self, name: &str, depth: usize, chain: &mut Vec<Theme>, visited: &mut Vec<String>) {
        if name == DEFAULT_THEME
            || !is_theme_name(name)
            || chain.len() >= MAX_THEMES.saturating_sub(1)
            || visited.len() >= MAX_VISITED
            || visited.iter().any(|seen| seen == name)
        {
            return;
        }
        visited.push(name.to_owned());
        let Some(theme) = load_theme(&self.base_dirs, name) else {
            debug!(theme = %logged(Path::new(name)), "icon theme not found");
            return;
        };
        let parents = theme.parents.clone();
        chain.push(theme);
        if depth >= MAX_INHERIT_DEPTH {
            return;
        }
        for parent in &parents {
            self.visit(parent, depth.saturating_add(1), chain, visited);
        }
    }

    /// Returns `<base>/<icon>.<ext>` from the first base directory holding it.
    fn unthemed(&self, icon: &str, checks: &mut Checks) -> Option<PathBuf> {
        self.base_dirs.iter().find_map(|base| {
            EXTENSIONS
                .iter()
                .map(|ext| base.join(format!("{icon}.{ext}")))
                .find(|path| checks.is_file(path))
        })
    }
}

/// Returns `icon` without a `.png`, `.svg` or `.xpm` extension, compared
/// ignoring ASCII case.
fn strip_extension(icon: &str) -> &str {
    match icon.rsplit_once('.') {
        Some((stem, ext))
            if NAME_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known)) =>
        {
            stem
        }
        _ => icon,
    }
}

/// File checks of one lookup: how many it may still make, and whether one
/// was refused or failed.
struct Checks {
    left: usize,
    refused: bool,
    failed: bool,
}

impl Checks {
    fn new(limit: usize) -> Self {
        Self {
            left: limit,
            refused: false,
            failed: false,
        }
    }

    /// Returns whether `path` is a regular file; `false` without checking
    /// once no checks are left, and for a file that cannot be inspected.
    fn is_file(&mut self, path: &Path) -> bool {
        let Some(left) = self.left.checked_sub(1) else {
            self.refused = true;
            return false;
        };
        self.left = left;
        match file_state(path) {
            Ok(file) => file,
            Err(error) => {
                trace!(path = %logged(path), error = &error as &dyn std::error::Error, "icon file unreadable");
                self.failed = true;
                false
            }
        }
    }

    /// Returns whether a check was refused for lack of checks left.
    fn exhausted(&self) -> bool {
        self.refused
    }
}

/// An icon theme: its directories in every base directory and its index.
#[derive(Debug, Clone)]
struct Theme {
    name: String,
    /// `<base>/<name>` for each base directory holding the theme, in base order.
    roots: Vec<PathBuf>,
    dirs: Vec<Dir>,
    parents: Vec<String>,
}

/// A directory of a theme, as described by its `index.theme` section.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dir {
    path: PathBuf,
    size: u32,
    scale: u32,
    kind: Kind,
    min: u32,
    max: u32,
    threshold: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed,
    Scalable,
    Threshold,
}

impl Theme {
    /// Returns the file of `icon` at `size`: from the first directory that
    /// matches the size, else from the directory closest to it, the earlier
    /// one on a tie.
    fn lookup(&self, icon: &str, size: u32, checks: &mut Checks) -> Option<PathBuf> {
        let mut closest: Option<(u32, PathBuf)> = None;
        for dir in &self.dirs {
            if checks.exhausted() {
                break;
            }
            let matches = dir.matches(size);
            let distance = dir.distance(size);
            if !matches && closest.as_ref().is_some_and(|(best, _)| distance >= *best) {
                continue;
            }
            let Some(file) = self.file(dir, icon, checks) else {
                continue;
            };
            if matches {
                return Some(file);
            }
            closest = Some((distance, file));
        }
        closest.map(|(_, file)| file)
    }

    fn file(&self, dir: &Dir, icon: &str, checks: &mut Checks) -> Option<PathBuf> {
        self.roots.iter().find_map(|root| {
            EXTENSIONS
                .iter()
                .map(|ext| root.join(&dir.path).join(format!("{icon}.{ext}")))
                .find(|path| checks.is_file(path))
        })
    }
}

impl Dir {
    fn matches(&self, size: u32) -> bool {
        if self.scale != 1 {
            return false;
        }
        match self.kind {
            Kind::Fixed => size == self.size,
            Kind::Scalable => (self.min..=self.max).contains(&size),
            Kind::Threshold => (self.size.saturating_sub(self.threshold)
                ..=self.size.saturating_add(self.threshold))
                .contains(&size),
        }
    }

    fn distance(&self, size: u32) -> u32 {
        let scaled = |value: u32| value.saturating_mul(self.scale);
        let (low, high) = match self.kind {
            Kind::Fixed => return scaled(self.size).abs_diff(size),
            Kind::Scalable => (scaled(self.min), scaled(self.max)),
            Kind::Threshold => (
                scaled(self.size.saturating_sub(self.threshold)),
                scaled(self.size.saturating_add(self.threshold)),
            ),
        };
        if size < low {
            scaled(self.min).saturating_sub(size)
        } else if size > high {
            size.saturating_sub(scaled(self.max))
        } else {
            0
        }
    }
}

/// Reads theme `name` from the first `index.theme` among `base_dirs`, or
/// `None` when no base directory holds a readable one.
fn load_theme(base_dirs: &[PathBuf], name: &str) -> Option<Theme> {
    let roots: Vec<PathBuf> = base_dirs
        .iter()
        .map(|base| base.join(name))
        .filter(|root| root.is_dir())
        .collect();
    let text = roots.iter().find_map(|root| {
        let path = root.join("index.theme");
        match read_text(&path, INDEX_MAX) {
            Ok(text) => Some(text),
            Err(error) if error.is_not_found() => None,
            Err(error) => {
                debug!(path = %logged(&path), error = &error as &dyn std::error::Error, "icon theme index unreadable");
                None
            }
        }
    })?;
    Some(parse_theme(name, &text, roots))
}

fn parse_theme(name: &str, text: &str, roots: Vec<PathBuf>) -> Theme {
    const SECTION: &str = "Icon Theme";
    let ini = Ini::parse(text);
    let dirs = ["Directories", "ScaledDirectories"]
        .into_iter()
        .filter_map(|key| ini.get(SECTION, key))
        .flat_map(ini::list)
        .filter(|name| is_relative_dir(name))
        .take(MAX_THEME_DIRS)
        .filter_map(|name| parse_dir(&ini, name))
        .collect();
    let parents = ini
        .get(SECTION, "Inherits")
        .into_iter()
        .flat_map(ini::list)
        .filter(|name| is_theme_name(name))
        .map(str::to_owned)
        .collect();
    Theme {
        name: name.to_owned(),
        roots,
        dirs,
        parents,
    }
}

/// Returns whether `name` is a relative path that stays inside the theme.
fn is_relative_dir(name: &str) -> bool {
    !name.chars().any(char::is_control)
        && Path::new(name)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Returns the directory `name` described by its section, or `None` when
/// the section has no valid `Size`.
fn parse_dir(ini: &Ini, name: &str) -> Option<Dir> {
    let number = |key: &str| {
        let value = ini.get(name, key)?;
        match value.parse::<u32>() {
            Ok(number) => Some(number),
            Err(error) => {
                trace!(
                    dir = name,
                    key,
                    error = &error as &dyn std::error::Error,
                    "invalid icon theme number"
                );
                None
            }
        }
    };
    let size = number("Size")?;
    let kind = match ini.get(name, "Type") {
        Some("Fixed") => Kind::Fixed,
        Some("Scalable") => Kind::Scalable,
        _ => Kind::Threshold,
    };
    Some(Dir {
        path: PathBuf::from(name),
        size,
        scale: number("Scale").unwrap_or(1),
        kind,
        min: number("MinSize").unwrap_or(size),
        max: number("MaxSize").unwrap_or(size),
        threshold: number("Threshold").unwrap_or(2),
    })
}

/// Returns whether `path` is a regular file after following symlinks.
fn is_file(path: &Path) -> bool {
    match file_state(path) {
        Ok(file) => file,
        Err(error) => {
            trace!(path = %logged(path), error = &error as &dyn std::error::Error, "icon file unreadable");
            false
        }
    }
}

/// Returns whether `path` is a regular file after following symlinks; a
/// missing file is not an error.
fn file_state(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::linux::test_error::{TestError, TestResult};

    /// Icon theme directories under a temporary root, with `user` and
    /// `system` base directories.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> TestResult<Self> {
            Ok(Self {
                dir: tempfile::tempdir()?,
            })
        }

        fn base(&self, base: &str) -> PathBuf {
            self.dir.path().join(base)
        }

        fn bases(&self) -> Vec<PathBuf> {
            vec![self.base("user"), self.base("system"), self.base("pixmaps")]
        }

        /// Writes the `index.theme` of `theme` in `base`.
        fn index(&self, base: &str, theme: &str, text: &str) -> TestResult {
            let root = self.base(base).join(theme);
            fs::create_dir_all(&root)?;
            fs::write(root.join("index.theme"), text)?;
            Ok(())
        }

        /// Writes `file` below `base`, returning its path.
        fn icon(&self, base: &str, file: &str) -> TestResult<String> {
            let path = self.base(base).join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, b"icon")?;
            path.into_os_string()
                .into_string()
                .map_err(TestError::NonUtf8Path)
        }

        fn lookup(&self, theme: &str) -> IconLookup {
            IconLookup::new(self.bases(), Some(self.base("home")), theme.to_owned(), 64)
        }
    }

    const SIZES: &str = "[Icon Theme]\nName=Sizes\n\
        Directories=16x16/apps,48x48/apps,64x64/apps,96x96/apps,scalable/apps,64x64@2/apps\n\
        ScaledDirectories=64x64@2/apps\n\
        [16x16/apps]\nSize=16\nType=Fixed\n\
        [48x48/apps]\nSize=48\nType=Threshold\n\
        [64x64/apps]\nSize=64\nType=Fixed\n\
        [96x96/apps]\nSize=96\nType=Fixed\n\
        [scalable/apps]\nSize=128\nMinSize=8\nMaxSize=512\nType=Scalable\n\
        [64x64@2/apps]\nSize=64\nScale=2\nType=Fixed\n";

    #[test]
    fn absolute_path_must_be_regular_image() -> TestResult {
        let fx = Fixture::new()?;
        let png = fx.icon("files", "app.PNG")?;
        let text = fx.icon("files", "app.txt")?;
        let xpm = fx.icon("files", "app.xpm")?;
        let folder = fx.base("files").join("folder.svg");
        fs::create_dir(&folder)?;
        let fifo = fx.base("files").join("fifo.png");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        let lookup = fx.lookup(DEFAULT_THEME);
        assert_eq!(lookup.resolve(&png).as_deref(), Some(png.as_str()));
        assert_eq!(lookup.resolve(&text), None);
        assert_eq!(lookup.resolve(&xpm), None);
        assert_eq!(lookup.resolve(&folder.to_string_lossy()), None);
        assert_eq!(lookup.resolve(&fifo.to_string_lossy()), None);
        assert_eq!(lookup.resolve("/nonexistent/touchcue-icon.png"), None);
        assert_eq!(lookup.resolve(&format!("{png}\n")), None);
        Ok(())
    }

    #[test]
    fn rejects_relative_paths_and_empty_names() -> TestResult {
        let fx = Fixture::new()?;
        fx.icon("pixmaps", "icons/app.png")?;
        let lookup = fx.lookup(DEFAULT_THEME);
        assert_eq!(lookup.resolve("icons/app.png"), None);
        assert_eq!(lookup.resolve(""), None);
        Ok(())
    }

    #[test]
    fn prefers_exact_size_then_closest() -> TestResult {
        let fx = Fixture::new()?;
        fx.index("system", "Sizes", SIZES)?;
        let exact = fx.icon("system", "Sizes/64x64/apps/term.png")?;
        fx.icon("system", "Sizes/16x16/apps/term.png")?;
        fx.icon("system", "Sizes/scalable/apps/term.svg")?;
        let closest = fx.icon("system", "Sizes/96x96/apps/near.png")?;
        fx.icon("system", "Sizes/16x16/apps/near.png")?;
        fx.icon("system", "Sizes/64x64@2/apps/near.png")?;
        let scalable = fx.icon("system", "Sizes/scalable/apps/vector.svg")?;
        fx.icon("system", "Sizes/96x96/apps/vector.png")?;
        let threshold = fx.icon("system", "Sizes/48x48/apps/small.png")?;
        fx.icon("system", "Sizes/16x16/apps/small.png")?;
        let lookup = fx.lookup("Sizes");
        assert_eq!(lookup.resolve("term").as_deref(), Some(exact.as_str()));
        assert_eq!(lookup.resolve("near").as_deref(), Some(closest.as_str()));
        assert_eq!(lookup.resolve("vector").as_deref(), Some(scalable.as_str()));
        assert_eq!(lookup.resolve("small").as_deref(), Some(threshold.as_str()));
        let at_50 = IconLookup::new(fx.bases(), None, "Sizes".to_owned(), 50);
        assert_eq!(at_50.resolve("small").as_deref(), Some(threshold.as_str()));
        Ok(())
    }

    #[test]
    fn size_distance_follows_the_spec() {
        let dir = |kind, size, min, max| Dir {
            path: PathBuf::new(),
            size,
            scale: 1,
            kind,
            min,
            max,
            threshold: 2,
        };
        assert_eq!(dir(Kind::Fixed, 48, 48, 48).distance(64), 16);
        assert_eq!(dir(Kind::Scalable, 128, 8, 512).distance(64), 0);
        assert_eq!(dir(Kind::Scalable, 128, 96, 512).distance(64), 32);
        assert_eq!(dir(Kind::Threshold, 48, 48, 48).distance(50), 0);
        assert_eq!(dir(Kind::Threshold, 48, 48, 48).distance(64), 16);
        assert_eq!(dir(Kind::Threshold, 96, 96, 96).distance(64), 32);
        assert!(
            !Dir {
                scale: 2,
                ..dir(Kind::Fixed, 64, 64, 64)
            }
            .matches(64)
        );
    }

    #[test]
    fn follows_inheritance_then_hicolor_then_pixmaps() -> TestResult {
        let fx = Fixture::new()?;
        let apps = |name: &str, inherits: &str| {
            format!(
                "[Icon Theme]\nName={name}\nInherits={inherits}\nDirectories=apps\n\
                 [apps]\nSize=64\nType=Fixed\n"
            )
        };
        fx.index("system", "Child", &apps("Child", "Missing,Loop,Parent"))?;
        fx.index("system", "Loop", &apps("Loop", "Child"))?;
        fx.index("system", "Parent", &apps("Parent", ""))?;
        fx.index("system", DEFAULT_THEME, &apps("Hicolor", ""))?;
        let child = fx.icon("system", "Child/apps/a.png")?;
        let parent = fx.icon("system", "Parent/apps/b.png")?;
        fx.icon("system", "hicolor/apps/b.png")?;
        let hicolor = fx.icon("system", "hicolor/apps/c.svg")?;
        fx.icon("pixmaps", "c.png")?;
        let pixmap = fx.icon("pixmaps", "d.png")?;
        let lookup = fx.lookup("Child");
        assert_eq!(lookup.resolve("a").as_deref(), Some(child.as_str()));
        assert_eq!(lookup.resolve("b").as_deref(), Some(parent.as_str()));
        assert_eq!(lookup.resolve("c").as_deref(), Some(hicolor.as_str()));
        assert_eq!(lookup.resolve("d").as_deref(), Some(pixmap.as_str()));
        assert_eq!(lookup.resolve("missing"), None);
        assert_eq!(lookup.chain().len(), 4);
        Ok(())
    }

    #[test]
    fn user_directory_and_first_index_win() -> TestResult {
        let fx = Fixture::new()?;
        fx.index(
            "user",
            "Theme",
            "[Icon Theme]\nDirectories=apps\n[apps]\nSize=64\nType=Fixed\n",
        )?;
        fx.index(
            "system",
            "Theme",
            "[Icon Theme]\nDirectories=other\n[other]\nSize=64\nType=Fixed\n",
        )?;
        let user = fx.icon("user", "Theme/apps/a.png")?;
        fx.icon("system", "Theme/apps/a.png")?;
        let system = fx.icon("system", "Theme/apps/b.svg")?;
        fx.icon("system", "Theme/other/c.png")?;
        let lookup = fx.lookup("Theme");
        assert_eq!(lookup.resolve("a").as_deref(), Some(user.as_str()));
        assert_eq!(lookup.resolve("b").as_deref(), Some(system.as_str()));
        assert_eq!(lookup.resolve("c"), None);
        Ok(())
    }

    #[test]
    fn unusable_indexes_and_dirs_are_skipped() -> TestResult {
        let fx = Fixture::new()?;
        fx.index("user", "Big", &"#".repeat(INDEX_MAX.saturating_add(1)))?;
        fx.index(
            "system",
            "Big",
            "[Icon Theme]\nDirectories=../escape,apps,nosize\n\
             [../escape]\nSize=64\n[apps]\nSize=64\n[nosize]\nType=Fixed\n",
        )?;
        fx.icon("system", "escape/a.png")?;
        let found = fx.icon("system", "Big/apps/b.png")?;
        fx.icon("system", "Big/nosize/c.png")?;
        let lookup = fx.lookup("Big");
        assert_eq!(lookup.resolve("a"), None);
        assert_eq!(lookup.resolve("b").as_deref(), Some(found.as_str()));
        assert_eq!(lookup.resolve("c"), None);
        assert!(lookup.load());
        assert!(!fx.lookup("Absent").load());
        assert!(!fx.lookup("../Big").load());
        Ok(())
    }

    #[test]
    fn config_icon_expands_home() -> TestResult {
        let fx = Fixture::new()?;
        let svg = fx.icon("home", "icons/key.svg")?;
        let lookup = fx.lookup(DEFAULT_THEME);
        assert_eq!(
            lookup.resolve_config("~/icons/key.svg").as_deref(),
            Some(svg.as_str())
        );
        assert_eq!(lookup.resolve_config(&svg).as_deref(), Some(svg.as_str()));
        assert_eq!(lookup.resolve_config("~/icons/missing.svg"), None);
        let homeless = IconLookup::new(fx.bases(), None, DEFAULT_THEME.to_owned(), 64);
        assert_eq!(homeless.resolve_config("~/icons/key.svg"), None);
        let relative = IconLookup::new(
            fx.bases(),
            Some(PathBuf::from("home")),
            DEFAULT_THEME.to_owned(),
            64,
        );
        assert_eq!(relative.resolve_config("~/icons/key.svg"), None);
        fx.icon("files", "escape.svg")?;
        assert_eq!(lookup.resolve_config("~/../files/escape.svg"), None);
        assert_eq!(lookup.resolve_config("~//etc/escape.svg"), None);
        assert_eq!(
            lookup.resolve_config("~/./icons/key.svg").as_deref(),
            Some(
                fx.base("home")
                    .join("./icons/key.svg")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        Ok(())
    }

    #[test]
    fn missing_themes_do_not_count_and_hicolor_comes_last() -> TestResult {
        let fx = Fixture::new()?;
        let apps = |inherits: &str| {
            format!(
                "[Icon Theme]\nInherits={inherits}\nDirectories=apps\n[apps]\nSize=64\nType=Fixed\n"
            )
        };
        let missing: Vec<String> = (0..20).map(|n| format!("Missing{n}")).collect();
        let themes: Vec<String> = (0..20).map(|n| format!("T{n}")).collect();
        let inherits = format!("{},hicolor,{}", missing.join(","), themes.join(","));
        fx.index("system", "Root", &apps(&inherits))?;
        for theme in &themes {
            fx.index("system", theme, &apps(""))?;
        }
        fx.index("system", DEFAULT_THEME, &apps(""))?;
        let hicolor = fx.icon("system", "hicolor/apps/h.png")?;
        let lookup = fx.lookup("Root");
        let names: Vec<&str> = lookup.chain().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), MAX_THEMES);
        assert_eq!(names.first(), Some(&"Root"));
        assert_eq!(names.get(1), Some(&"T0"));
        assert_eq!(names.last(), Some(&DEFAULT_THEME));
        assert_eq!(lookup.resolve("h").as_deref(), Some(hicolor.as_str()));
        Ok(())
    }

    #[test]
    fn name_extensions_are_removed() -> TestResult {
        let fx = Fixture::new()?;
        let png = fx.icon("pixmaps", "app.png")?;
        let lookup = fx.lookup(DEFAULT_THEME);
        for name in ["app", "app.png", "app.SVG", "app.xpm"] {
            assert_eq!(
                lookup.resolve(name).as_deref(),
                Some(png.as_str()),
                "{name}"
            );
        }
        assert_eq!(lookup.resolve(".png"), None);
        Ok(())
    }

    #[test]
    fn results_are_remembered_and_hits_rechecked() -> TestResult {
        let fx = Fixture::new()?;
        let lookup = fx.lookup(DEFAULT_THEME);
        assert_eq!(lookup.resolve("late"), None);
        fx.icon("pixmaps", "late.png")?;
        assert_eq!(lookup.resolve("late"), None);
        assert!(fx.lookup(DEFAULT_THEME).resolve("late").is_some());
        let user = fx.icon("user", "moved.png")?;
        let system = fx.icon("system", "moved.png")?;
        assert_eq!(lookup.resolve("moved").as_deref(), Some(user.as_str()));
        fs::remove_file(&user)?;
        assert_eq!(lookup.resolve("moved").as_deref(), Some(system.as_str()));
        Ok(())
    }

    #[test]
    fn lookups_stop_at_the_check_limit() -> TestResult {
        let fx = Fixture::new()?;
        fx.index("system", "Sizes", SIZES)?;
        fx.icon("system", "Sizes/scalable/apps/far.svg")?;
        let chain = fx.lookup("Sizes");
        let theme = chain.chain().first().ok_or(TestError::Missing("theme"))?;
        let mut few = Checks::new(4);
        assert_eq!(theme.lookup("far", 64, &mut few), None);
        assert!(few.exhausted());
        let mut exact = Checks::new(10);
        assert!(theme.lookup("far", 64, &mut exact).is_some());
        assert!(!exact.exhausted());
        let mut enough = Checks::new(MAX_LOOKUP_CHECKS);
        assert!(theme.lookup("far", 64, &mut enough).is_some());
        assert!(!enough.exhausted());
        Ok(())
    }

    #[test]
    fn failed_checks_are_not_remembered() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let fx = Fixture::new()?;
        let locked = fx.base("user");
        fs::create_dir_all(&locked)?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        let denied = matches!(
            fs::metadata(locked.join("probe.png")),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied
        );
        let lookup = fx.lookup(DEFAULT_THEME);
        let missed = lookup.resolve("late");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;
        // Root reads the directory anyway, so nothing fails to remember.
        if !denied {
            return Ok(());
        }
        assert_eq!(missed, None);
        let late = fx.icon("pixmaps", "late.png")?;
        assert_eq!(lookup.resolve("late").as_deref(), Some(late.as_str()));
        Ok(())
    }

    #[test]
    fn base_dirs_put_home_first_and_pixmaps_last() {
        let data = [PathBuf::from("/data/home"), PathBuf::from("/usr/share")];
        assert_eq!(
            base_dirs(Some(Path::new("/home/u")), &data),
            [
                "/home/u/.icons",
                "/data/home/icons",
                "/usr/share/icons",
                PIXMAPS
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            base_dirs(Some(Path::new("home")), &[]),
            vec![PathBuf::from(PIXMAPS)]
        );
    }

    #[test]
    fn theme_names_exclude_paths() {
        assert!(is_theme_name("Papirus-Dark"));
        for name in ["", ".", "..", "a/b", "a\nb"] {
            assert!(!is_theme_name(name), "{name:?}");
        }
    }
}
