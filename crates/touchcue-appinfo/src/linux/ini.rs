//! Parsing of INI-style files: icon theme indexes and desktop settings.

use std::collections::BTreeMap;

/// Most sections kept; further sections are ignored with their keys.
pub(crate) const MAX_SECTIONS: usize = 8192;

/// Keys and values of an INI file, by section.
///
/// Parsing follows the key files of `GLib`: a leading UTF-8 byte order mark is
/// skipped, a section given twice is merged into one, and of a key given
/// twice in a section the last value wins. Lines are trimmed. Empty lines,
/// lines starting with `#` or `;`, lines outside a section and lines without
/// `=` are skipped. Keys and values are trimmed around `=`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Ini {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Ini {
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let name = name.trim().to_owned();
                current = if sections.contains_key(&name) || sections.len() < MAX_SECTIONS {
                    sections.entry(name.clone()).or_default();
                    Some(name)
                } else {
                    None
                };
                continue;
            }
            let (Some(section), Some((key, value))) = (current.as_ref(), line.split_once('='))
            else {
                continue;
            };
            if let Some(keys) = sections.get_mut(section) {
                keys.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
        Self { sections }
    }

    /// Returns the value of `key` in `section`.
    pub(crate) fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }
}

/// Returns the non-empty, trimmed entries of the comma-separated `value`.
pub(crate) fn list(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sections_keys_and_lists() {
        let ini = Ini::parse(
            "\u{feff}# comment\nstray=1\n[Icon Theme]\n Name = Papirus \nInherits=breeze,, hicolor,\n\
             ; note\nnot a pair\n[16x16/apps]\nSize=16\nSize=32\n[Icon Theme]\nComment=Other\n",
        );
        assert_eq!(ini.get("Icon Theme", "Name"), Some("Papirus"));
        assert_eq!(ini.get("Icon Theme", "Comment"), Some("Other"));
        assert_eq!(
            ini.get("Icon Theme", "Inherits")
                .map(|v| list(v).collect::<Vec<_>>()),
            Some(vec!["breeze", "hicolor"])
        );
        assert_eq!(ini.get("16x16/apps", "Size"), Some("32"));
        assert_eq!(ini.get("Icon Theme", "stray"), None);
        assert_eq!(ini.get("Missing", "Name"), None);
    }

    #[test]
    fn byte_order_mark_is_skipped_only_at_the_start() {
        assert_eq!(Ini::parse("\u{feff}[A]\nk=v\n").get("A", "k"), Some("v"));
        assert_eq!(Ini::parse("[A]\n\u{feff}k=v\n").get("A", "k"), None);
    }

    #[test]
    fn malformed_text_yields_nothing() {
        assert_eq!(Ini::parse("[unclosed\nkey=value\n=\n"), Ini::default());
    }

    #[test]
    fn sections_beyond_the_cap_are_ignored() {
        let mut text = String::from("[first]\na=1\n");
        text.extend((0..30_000).map(|n| format!("[s{n}]\nk=v\n")));
        text.push_str("[first]\nb=2\n");
        let ini = Ini::parse(&text);
        assert_eq!(ini.sections.len(), MAX_SECTIONS);
        assert_eq!(ini.get("s0", "k"), Some("v"));
        assert_eq!(ini.get("s29999", "k"), None);
        assert_eq!(ini.get("first", "a"), Some("1"));
        assert_eq!(ini.get("first", "b"), Some("2"));
    }
}
