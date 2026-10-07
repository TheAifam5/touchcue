//! Choice of the icon theme for application and rule icons.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use touchcue_appinfo::linux::icon::{self, IconLookup, is_theme_name};
use touchcue_appinfo::linux::settings::{self, SettingsFile};
use touchcue_core::text::sanitize;
use touchcue_ipc::portal::{Portal, PortalError};

/// Longest theme name recorded in logs, in chars.
const LOGGED_THEME_MAX: usize = 128;
/// Longest wait for the desktop portal: connecting to the session bus and
/// reading both settings.
const PORTAL_DEADLINE: Duration = Duration::from_millis(1500);
/// Longest wait for the whole detection, in milliseconds, reading the
/// chosen theme and its ancestors included.
const DETECTION_TIMEOUT_MS: u64 = 3000;
/// [`DETECTION_TIMEOUT_MS`] as a duration.
const DETECTION_TIMEOUT: Duration = Duration::from_millis(DETECTION_TIMEOUT_MS);

/// A portal setting that can name the icon theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalKey {
    /// `org.gnome.desktop.interface` `icon-theme`.
    Gnome,
    /// `org.kde.kdeglobals.Icons` `Theme`.
    Kde,
}

impl PortalKey {
    /// Returns the namespace and key of the setting.
    pub const fn setting(self) -> (&'static str, &'static str) {
        match self {
            Self::Gnome => ("org.gnome.desktop.interface", "icon-theme"),
            Self::Kde => ("org.kde.kdeglobals.Icons", "Theme"),
        }
    }
}

/// Where the icon theme came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// `icons.theme` in the configuration.
    Config,
    /// A setting of the XDG desktop portal.
    Portal(PortalKey),
    /// A desktop settings file.
    File(SettingsFile),
    /// No source named an installed theme; hicolor.
    Default,
}

impl ThemeSource {
    /// Returns the name shown by `touchcue check` and in logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Portal(PortalKey::Gnome) => "portal org.gnome.desktop.interface",
            Self::Portal(PortalKey::Kde) => "portal org.kde.kdeglobals.Icons",
            Self::File(SettingsFile::Gtk4) => "gtk-4.0/settings.ini",
            Self::File(SettingsFile::Gtk3) => "gtk-3.0/settings.ini",
            Self::File(SettingsFile::Kdeglobals) => "kdeglobals",
            Self::File(SettingsFile::Qt6ct) => "qt6ct.conf",
            Self::File(SettingsFile::Qt5ct) => "qt5ct.conf",
            Self::Default => "default",
        }
    }
}

/// The chosen icon theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub source: ThemeSource,
}

/// Sources of the icon theme other than the configuration.
pub trait Sources {
    /// Returns whether the desktop is KDE, whose portal setting is then read
    /// before GNOME's.
    fn kde(&self) -> bool;
    /// Returns the portal setting `key`, or `None` without a session bus.
    async fn portal(&mut self, key: PortalKey) -> Option<Result<Option<String>, PortalError>>;
    /// Returns the theme named by the first desktop settings file that names one.
    async fn files(&mut self) -> Option<(String, SettingsFile)>;
    /// Reads theme `name`, its ancestors and hicolor, and returns whether
    /// `name` is installed.
    async fn load(&mut self, name: &str) -> bool;
}

/// Returns the icon theme: the first installed of `config`, the portal
/// settings read within [`PORTAL_DEADLINE`] and the first settings file,
/// else hicolor.
///
/// A portal setting that fails, is empty or cannot name a theme directory
/// is skipped. A named theme that is not installed is skipped with a
/// warning. The chosen theme is read before this returns.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn detect(config: Option<&str>, sources: &mut impl Sources) -> Theme {
    let theme = if let Some(theme) = first_installed(config, sources).await {
        theme
    } else {
        if !sources.load(icon::DEFAULT_THEME).await {
            tracing::debug!("hicolor icon theme not installed");
        }
        default_theme()
    };
    tracing::info!(
        theme = sanitize(&theme.name, LOGGED_THEME_MAX).as_deref(),
        source = theme.source.as_str(),
        "icon theme chosen"
    );
    theme
}

/// Returns the first theme named by `config`, the portal or the settings
/// files that is installed.
async fn first_installed(config: Option<&str>, sources: &mut impl Sources) -> Option<Theme> {
    if let Some(name) = config
        && installed(sources, name, ThemeSource::Config).await
    {
        return Some(Theme {
            name: name.to_owned(),
            source: ThemeSource::Config,
        });
    }
    for (name, source) in portal(sources).await {
        if installed(sources, &name, source).await {
            return Some(Theme { name, source });
        }
    }
    let (name, file) = sources.files().await?;
    let source = ThemeSource::File(file);
    installed(sources, &name, source)
        .await
        .then_some(Theme { name, source })
}

/// Reads theme `name` and returns whether it is installed, warning when not.
async fn installed(sources: &mut impl Sources, name: &str, source: ThemeSource) -> bool {
    if sources.load(name).await {
        return true;
    }
    tracing::warn!(
        theme = sanitize(name, LOGGED_THEME_MAX).as_deref(),
        source = source.as_str(),
        "icon theme not installed; trying the next source"
    );
    false
}

fn default_theme() -> Theme {
    Theme {
        name: icon::DEFAULT_THEME.to_owned(),
        source: ThemeSource::Default,
    }
}

/// Returns the usable portal settings, KDE's first on KDE; none when no
/// portal answers or [`PORTAL_DEADLINE`] passes.
async fn portal(sources: &mut impl Sources) -> Vec<(String, ThemeSource)> {
    let keys = if sources.kde() {
        [PortalKey::Kde, PortalKey::Gnome]
    } else {
        [PortalKey::Gnome, PortalKey::Kde]
    };
    let mut found = Vec::new();
    let read = async {
        for key in keys {
            let (namespace, name) = key.setting();
            match sources.portal(key).await {
                None => {
                    tracing::debug!("no desktop portal; skipping it");
                    return;
                }
                Some(Ok(Some(theme))) if is_theme_name(&theme) => {
                    found.push((theme, ThemeSource::Portal(key)));
                }
                Some(Ok(Some(_))) => {
                    tracing::debug!(
                        namespace,
                        key = name,
                        "portal setting names no usable theme"
                    );
                }
                Some(Ok(None)) => {
                    tracing::debug!(namespace, key = name, "portal setting is empty");
                }
                Some(Err(error)) => tracing::debug!(
                    namespace,
                    key = name,
                    error = &error as &dyn std::error::Error,
                    "portal setting unavailable"
                ),
            }
        }
    };
    match tokio::time::timeout(PORTAL_DEADLINE, read).await {
        Ok(()) => found,
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "desktop portal did not answer in time; skipping it"
            );
            Vec::new()
        }
    }
}

/// Returns whether `XDG_CURRENT_DESKTOP`, a `:`-separated list, names KDE.
fn is_kde(current_desktop: Option<&OsStr>) -> bool {
    current_desktop
        .and_then(OsStr::to_str)
        .is_some_and(|value| {
            value
                .split(':')
                .any(|name| name.eq_ignore_ascii_case("KDE"))
        })
}

/// Returns the icon theme from [`detect`] over the session's sources, and the
/// lookup of that theme, read; `lookup` gives the directories searched.
///
/// Detection that exceeds [`DETECTION_TIMEOUT`] yields hicolor, read on the
/// first lookup.
pub async fn detect_system(config: Option<&str>, lookup: IconLookup) -> (Theme, IconLookup) {
    let mut sources = SystemSources::new(lookup);
    match tokio::time::timeout(DETECTION_TIMEOUT, detect(config, &mut sources)).await {
        Ok(theme) => {
            let lookup = sources.into_lookup(&theme.name);
            (theme, lookup)
        }
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                timeout_ms = DETECTION_TIMEOUT_MS,
                "icon theme detection timed out; using hicolor"
            );
            let lookup = sources.base.with_theme(icon::DEFAULT_THEME.to_owned());
            (default_theme(), lookup)
        }
    }
}

/// State of the portal connection of [`SystemSources`].
enum Bus {
    NotTried,
    Unavailable,
    Connected(Portal),
}

/// The session's portal, settings files and installed themes.
pub struct SystemSources {
    kde: bool,
    bus: Bus,
    config_home: Option<PathBuf>,
    /// Gives the directories searched for themes.
    base: IconLookup,
    /// The theme last read by [`Sources::load`].
    loaded: Option<IconLookup>,
}

impl SystemSources {
    /// Creates the sources; the session bus is connected on the first portal
    /// read, and themes are read from the directories of `base`.
    pub fn new(base: IconLookup) -> Self {
        Self {
            kde: is_kde(std::env::var_os("XDG_CURRENT_DESKTOP").as_deref()),
            bus: Bus::NotTried,
            config_home: settings::config_home(),
            base,
            loaded: None,
        }
    }

    /// Returns the lookup of theme `name`: the one read by
    /// [`Sources::load`], else one that reads it on the first lookup.
    fn into_lookup(self, name: &str) -> IconLookup {
        match self.loaded {
            Some(loaded) if loaded.theme() == name => loaded,
            _ => self.base.with_theme(name.to_owned()),
        }
    }
}

impl Sources for SystemSources {
    fn kde(&self) -> bool {
        self.kde
    }

    async fn portal(&mut self, key: PortalKey) -> Option<Result<Option<String>, PortalError>> {
        if matches!(self.bus, Bus::NotTried) {
            self.bus = match Portal::connect().await {
                Ok(portal) => Bus::Connected(portal),
                Err(error) => {
                    tracing::debug!(
                        error = &error as &dyn std::error::Error,
                        "cannot reach the desktop portal for the icon theme"
                    );
                    Bus::Unavailable
                }
            };
        }
        let Bus::Connected(portal) = &self.bus else {
            return None;
        };
        let (namespace, name) = key.setting();
        Some(portal.read_string(namespace, name).await)
    }

    async fn files(&mut self) -> Option<(String, SettingsFile)> {
        let config_home = self.config_home.clone()?;
        match tokio::task::spawn_blocking(move || settings::icon_theme(&config_home)).await {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "reading the icon theme settings failed"
                );
                None
            }
        }
    }

    async fn load(&mut self, name: &str) -> bool {
        let lookup = self.base.with_theme(name.to_owned());
        let reader = lookup.clone();
        match tokio::task::spawn_blocking(move || reader.load()).await {
            Ok(installed) => {
                self.loaded = Some(lookup);
                installed
            }
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "reading the icon theme failed"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Answer of the fake portal for one key.
    #[derive(Clone, Copy)]
    enum Answer {
        Set(&'static str),
        Empty,
        Failed,
        /// Answers after an hour.
        Hang,
    }

    /// Sources with fixed answers; `portal` holds the answers for GNOME and
    /// KDE, and `None` for no session bus.
    struct Fake {
        kde: bool,
        portal: Option<[Answer; 2]>,
        files: Option<(&'static str, SettingsFile)>,
        installed: &'static [&'static str],
    }

    impl Sources for Fake {
        fn kde(&self) -> bool {
            self.kde
        }

        async fn portal(&mut self, key: PortalKey) -> Option<Result<Option<String>, PortalError>> {
            let index = match key {
                PortalKey::Gnome => 0,
                PortalKey::Kde => 1,
            };
            let answer = *self.portal.as_ref()?.get(index)?;
            Some(match answer {
                Answer::Set(theme) => Ok(Some(theme.to_owned())),
                Answer::Empty => Ok(None),
                Answer::Failed => Err(PortalError::NotString),
                Answer::Hang => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(Some("Late".to_owned()))
                }
            })
        }

        fn files(&mut self) -> impl Future<Output = Option<(String, SettingsFile)>> {
            std::future::ready(self.files.map(|(name, file)| (name.to_owned(), file)))
        }

        fn load(&mut self, name: &str) -> impl Future<Output = bool> {
            std::future::ready(self.installed.contains(&name))
        }
    }

    const ALL: &[&str] = &["Conf", "Gnome", "Kde", "Gtk", "Late"];

    fn fake() -> Fake {
        Fake {
            kde: false,
            portal: Some([Answer::Set("Gnome"), Answer::Set("Kde")]),
            files: Some(("Gtk", SettingsFile::Gtk3)),
            installed: ALL,
        }
    }

    async fn theme(config: Option<&str>, mut fake: Fake) -> (String, ThemeSource) {
        let theme = detect(config, &mut fake).await;
        (theme.name, theme.source)
    }

    #[tokio::test]
    async fn config_wins_then_portal_then_files_then_hicolor() {
        assert_eq!(
            theme(Some("Conf"), fake()).await,
            ("Conf".to_owned(), ThemeSource::Config)
        );
        assert_eq!(
            theme(None, fake()).await,
            ("Gnome".to_owned(), ThemeSource::Portal(PortalKey::Gnome))
        );
        let kde = Fake {
            portal: Some([Answer::Failed, Answer::Set("Kde")]),
            ..fake()
        };
        assert_eq!(
            theme(None, kde).await,
            ("Kde".to_owned(), ThemeSource::Portal(PortalKey::Kde))
        );
        let unusable = Fake {
            portal: Some([Answer::Set("a/b"), Answer::Set("../x")]),
            ..fake()
        };
        assert_eq!(
            theme(None, unusable).await,
            ("Gtk".to_owned(), ThemeSource::File(SettingsFile::Gtk3))
        );
        let no_bus = Fake {
            portal: None,
            ..fake()
        };
        assert_eq!(
            theme(None, no_bus).await,
            ("Gtk".to_owned(), ThemeSource::File(SettingsFile::Gtk3))
        );
        let nothing = Fake {
            portal: Some([Answer::Empty, Answer::Failed]),
            files: None,
            ..fake()
        };
        assert_eq!(
            theme(None, nothing).await,
            ("hicolor".to_owned(), ThemeSource::Default)
        );
    }

    #[tokio::test]
    async fn kde_desktops_read_the_kde_setting_first() {
        let on_kde = Fake {
            kde: true,
            ..fake()
        };
        assert_eq!(
            theme(None, on_kde).await,
            ("Kde".to_owned(), ThemeSource::Portal(PortalKey::Kde))
        );
        assert!(is_kde(Some(OsStr::new("KDE"))));
        assert!(is_kde(Some(OsStr::new("ubuntu:kde"))));
        assert!(!is_kde(Some(OsStr::new("GNOME:KDEish"))));
        assert!(!is_kde(None));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_portal_falls_through_to_files() {
        let slow = Fake {
            portal: Some([Answer::Hang, Answer::Set("Kde")]),
            ..fake()
        };
        assert_eq!(
            theme(None, slow).await,
            ("Gtk".to_owned(), ThemeSource::File(SettingsFile::Gtk3))
        );
    }

    #[tokio::test]
    async fn missing_themes_fall_through_to_the_next_source() {
        let config_missing = Fake {
            installed: &["Gnome", "Kde", "Gtk"],
            ..fake()
        };
        assert_eq!(
            theme(Some("Conf"), config_missing).await,
            ("Gnome".to_owned(), ThemeSource::Portal(PortalKey::Gnome))
        );
        let gnome_missing = Fake {
            installed: &["Kde", "Gtk"],
            ..fake()
        };
        assert_eq!(
            theme(None, gnome_missing).await,
            ("Kde".to_owned(), ThemeSource::Portal(PortalKey::Kde))
        );
        let portal_missing = Fake {
            installed: &["Gtk"],
            ..fake()
        };
        assert_eq!(
            theme(None, portal_missing).await,
            ("Gtk".to_owned(), ThemeSource::File(SettingsFile::Gtk3))
        );
    }

    #[tokio::test]
    async fn missing_theme_falls_back_to_hicolor() {
        let missing = Fake {
            portal: Some([Answer::Set("Gnome"), Answer::Empty]),
            files: None,
            installed: &["Conf"],
            ..fake()
        };
        assert_eq!(
            theme(None, missing).await,
            ("hicolor".to_owned(), ThemeSource::Default)
        );
    }
}
