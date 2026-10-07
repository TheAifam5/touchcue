//! Icon theme chosen in desktop settings files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tracing::{debug, trace};

use super::bounded::{logged, read_text};
use super::icon::is_theme_name;
use super::ini::Ini;

/// Largest settings file read, in bytes.
pub const SETTINGS_MAX: usize = 64 * 1024;

/// A settings file that can name the icon theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFile {
    /// `gtk-4.0/settings.ini`, `[Settings] gtk-icon-theme-name`.
    Gtk4,
    /// `gtk-3.0/settings.ini`, `[Settings] gtk-icon-theme-name`.
    Gtk3,
    /// `kdeglobals`, `[Icons] Theme`.
    Kdeglobals,
    /// `qt6ct/qt6ct.conf`, `[Appearance] icon_theme`.
    Qt6ct,
    /// `qt5ct/qt5ct.conf`, `[Appearance] icon_theme`.
    Qt5ct,
}

impl SettingsFile {
    /// Every settings file, in the order [`icon_theme`] reads them.
    pub const ALL: [Self; 5] = [
        Self::Gtk4,
        Self::Gtk3,
        Self::Kdeglobals,
        Self::Qt6ct,
        Self::Qt5ct,
    ];

    /// Returns the path of the file relative to the configuration directory.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Gtk4 => "gtk-4.0/settings.ini",
            Self::Gtk3 => "gtk-3.0/settings.ini",
            Self::Kdeglobals => "kdeglobals",
            Self::Qt6ct => "qt6ct/qt6ct.conf",
            Self::Qt5ct => "qt5ct/qt5ct.conf",
        }
    }

    const fn key(self) -> (&'static str, &'static str) {
        match self {
            Self::Gtk4 | Self::Gtk3 => ("Settings", "gtk-icon-theme-name"),
            Self::Kdeglobals => ("Icons", "Theme"),
            Self::Qt6ct | Self::Qt5ct => ("Appearance", "icon_theme"),
        }
    }
}

/// Returns `$XDG_CONFIG_HOME`, or `$HOME/.config` when it is unset, empty or
/// relative; `None` without an absolute home directory either.
#[must_use]
pub fn config_home() -> Option<PathBuf> {
    config_home_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn config_home_from(config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let absolute = |value: &OsStr| Some(PathBuf::from(value)).filter(|p| p.is_absolute());
    config_home
        .and_then(absolute)
        .or_else(|| home.and_then(absolute).map(|home| home.join(".config")))
}

/// Returns the icon theme named by the first of [`SettingsFile::ALL`] below
/// `config_home` that names one, with that file.
///
/// A missing, unreadable, oversized or non-UTF-8 file, and an empty value
/// or one that cannot name a theme directory, are skipped.
#[must_use]
pub fn icon_theme(config_home: &Path) -> Option<(String, SettingsFile)> {
    SettingsFile::ALL
        .into_iter()
        .find_map(|file| read_theme(&config_home.join(file.path()), file).map(|name| (name, file)))
}

fn read_theme(path: &Path, file: SettingsFile) -> Option<String> {
    let text = match read_text(path, SETTINGS_MAX) {
        Ok(text) => text,
        Err(error) if error.is_not_found() => {
            trace!(path = %logged(path), "no settings file");
            return None;
        }
        Err(error) => {
            debug!(path = %logged(path), error = &error as &dyn std::error::Error, "settings file unreadable");
            return None;
        }
    };
    let (section, key) = file.key();
    let value = Ini::parse(&text).get(section, key)?.to_owned();
    if is_theme_name(&value) {
        Some(value)
    } else {
        debug!(path = %logged(path), "settings file names no usable icon theme");
        None
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    use crate::linux::test_error::TestResult;

    fn write(root: &Path, file: SettingsFile, text: &str) -> TestResult {
        let path = root.join(file.path());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    #[test]
    fn reads_files_in_order() -> TestResult {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        assert_eq!(icon_theme(root), None);
        write(root, SettingsFile::Qt5ct, "[Appearance]\nicon_theme=Qt5\n")?;
        assert_eq!(
            icon_theme(root),
            Some(("Qt5".to_owned(), SettingsFile::Qt5ct))
        );
        write(
            root,
            SettingsFile::Qt6ct,
            "[Appearance]\nicon_theme=Papirus-Dark\n",
        )?;
        assert_eq!(
            icon_theme(root),
            Some(("Papirus-Dark".to_owned(), SettingsFile::Qt6ct))
        );
        write(root, SettingsFile::Kdeglobals, "[Icons]\nTheme=breeze\n")?;
        assert_eq!(
            icon_theme(root),
            Some(("breeze".to_owned(), SettingsFile::Kdeglobals))
        );
        write(
            root,
            SettingsFile::Gtk3,
            "[Settings]\ngtk-icon-theme-name = Adwaita\n",
        )?;
        assert_eq!(
            icon_theme(root),
            Some(("Adwaita".to_owned(), SettingsFile::Gtk3))
        );
        write(
            root,
            SettingsFile::Gtk4,
            "[Settings]\ngtk-icon-theme-name=Papirus\n",
        )?;
        assert_eq!(
            icon_theme(root),
            Some(("Papirus".to_owned(), SettingsFile::Gtk4))
        );
        Ok(())
    }

    #[test]
    fn skips_unusable_files_and_values() -> TestResult {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        write(
            root,
            SettingsFile::Gtk4,
            "[Settings]\ngtk-icon-theme-name=\n",
        )?;
        write(
            root,
            SettingsFile::Gtk3,
            "[Settings]\ngtk-icon-theme-name=../x\n",
        )?;
        write(root, SettingsFile::Kdeglobals, "[Icons\nTheme=broken\n")?;
        let big = format!("[Appearance]\nicon_theme=big\n{}", "#".repeat(SETTINGS_MAX));
        write(root, SettingsFile::Qt6ct, &big)?;
        fs::create_dir_all(root.join(SettingsFile::Qt5ct.path()))?;
        assert_eq!(icon_theme(root), None);
        Ok(())
    }

    #[test]
    fn config_home_prefers_absolute_xdg() {
        let home = Some(OsStr::new("/home/u"));
        assert_eq!(
            config_home_from(Some(OsStr::new("/cfg")), home),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            config_home_from(Some(OsStr::new("cfg")), home),
            Some(PathBuf::from("/home/u/.config"))
        );
        assert_eq!(config_home_from(None, Some(OsStr::new("u"))), None);
    }
}
