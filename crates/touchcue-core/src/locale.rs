//! Language of the built-in prompt text.

/// Language of the built-in templates, `request.action` and the outcome
/// suffix of a lingering request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Language {
    #[default]
    English,
    Polish,
    German,
}

impl Language {
    /// Returns the language of a locale name such as `pl_PL.UTF-8`: the part
    /// before the first `_`, `.` or `@`, compared ignoring ASCII case. `C`,
    /// `POSIX` and unknown languages are English.
    #[must_use]
    pub fn from_locale(locale: &str) -> Self {
        let language = locale.split(['_', '.', '@']).next().unwrap_or_default();
        if language.eq_ignore_ascii_case("pl") {
            Self::Polish
        } else if language.eq_ignore_ascii_case("de") {
            Self::German
        } else {
            Self::English
        }
    }

    /// Returns the language of the first non-empty of `configured`, then
    /// `env`, which holds the values of `LC_ALL`, `LC_MESSAGES` and `LANG`
    /// in that order; English when all are absent or empty.
    #[must_use]
    pub fn detect(configured: Option<&str>, env: [Option<&str>; 3]) -> Self {
        std::iter::once(configured)
            .chain(env)
            .flatten()
            .find(|locale| !locale.is_empty())
            .map_or(Self::English, Self::from_locale)
    }

    /// Returns the entry of `texts`, which lists English, Polish and German
    /// in that order, for this language.
    pub(crate) fn pick(self, texts: [&'static str; 3]) -> &'static str {
        let [english, polish, german] = texts;
        match self {
            Self::English => english,
            Self::Polish => polish,
            Self::German => german,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_names_map_to_languages() {
        let cases = [
            ("pl", Language::Polish),
            ("pl_PL.UTF-8", Language::Polish),
            ("PL_pl", Language::Polish),
            ("de_DE@euro", Language::German),
            ("de.UTF-8", Language::German),
            ("en_US.UTF-8", Language::English),
            ("C", Language::English),
            ("C.UTF-8", Language::English),
            ("POSIX", Language::English),
            ("fr_FR.UTF-8", Language::English),
            ("plx", Language::English),
            ("", Language::English),
        ];
        for (locale, language) in cases {
            assert_eq!(Language::from_locale(locale), language, "{locale}");
        }
    }

    #[test]
    fn lc_all_precedes_lc_messages_and_lang() {
        let env = [
            Some("de_DE.UTF-8"),
            Some("pl_PL.UTF-8"),
            Some("en_US.UTF-8"),
        ];
        assert_eq!(Language::detect(None, env), Language::German);
        let env = [None, Some("pl_PL.UTF-8"), Some("de_DE.UTF-8")];
        assert_eq!(Language::detect(None, env), Language::Polish);
        let env = [None, None, Some("de_DE.UTF-8")];
        assert_eq!(Language::detect(None, env), Language::German);
    }

    #[test]
    fn empty_values_are_skipped() {
        let env = [Some(""), Some(""), Some("pl_PL.UTF-8")];
        assert_eq!(Language::detect(Some(""), env), Language::Polish);
        assert_eq!(
            Language::detect(None, [Some(""), None, Some("")]),
            Language::English
        );
        assert_eq!(Language::detect(None, [None; 3]), Language::English);
    }

    #[test]
    fn c_and_posix_are_english_and_end_the_search() {
        let env = [Some("C"), Some("pl_PL.UTF-8"), None];
        assert_eq!(Language::detect(None, env), Language::English);
        let env = [None, Some("POSIX"), Some("de_DE.UTF-8")];
        assert_eq!(Language::detect(None, env), Language::English);
    }

    #[test]
    fn unknown_language_is_english() {
        let env = [Some("fr_FR.UTF-8"), None, Some("pl_PL.UTF-8")];
        assert_eq!(Language::detect(None, env), Language::English);
    }

    #[test]
    fn configured_locale_overrides_the_environment() {
        let env = [Some("de_DE.UTF-8"), None, None];
        assert_eq!(Language::detect(Some("pl"), env), Language::Polish);
        assert_eq!(Language::detect(Some("en"), env), Language::English);
    }
}
