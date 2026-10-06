//! Sanitizing of untrusted text before it is displayed.

use std::ops::RangeInclusive;

/// Invisible, bidirectional, format and separator characters, replaced so
/// that displayed text reads the same as its content.
const INVISIBLE: &[RangeInclusive<char>] = &[
    '\u{00AD}'..='\u{00AD}',
    '\u{061C}'..='\u{061C}',
    '\u{180E}'..='\u{180E}',
    '\u{200B}'..='\u{200F}',
    '\u{2028}'..='\u{2029}',
    '\u{202A}'..='\u{202E}',
    '\u{2060}'..='\u{2064}',
    '\u{2066}'..='\u{2069}',
    '\u{FEFF}'..='\u{FEFF}',
    '\u{FFF9}'..='\u{FFFB}',
    '\u{E0001}'..='\u{E007F}',
];

/// Returns `s` with control and invisible characters replaced by spaces,
/// whitespace runs collapsed to one space, trimmed, and capped at
/// `max_chars` characters.
///
/// Returns `None` when nothing visible remains.
#[must_use]
pub fn sanitize(s: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_control() || INVISIBLE.iter().any(|r| r.contains(&c)) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(max_chars).collect();
    let out = capped.trim_end();
    (!out.is_empty()).then(|| out.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_become_spaces() {
        assert_eq!(
            sanitize("a\u{1b}[31mb\nc\td", 64).as_deref(),
            Some("a [31mb c d")
        );
    }

    #[test]
    fn bidi_override_is_removed() {
        assert_eq!(
            sanitize("evil\u{202E}fdp.exe", 64).as_deref(),
            Some("evil fdp.exe")
        );
    }

    #[test]
    fn line_separator_is_removed() {
        assert_eq!(sanitize("a\u{2028}b", 64).as_deref(), Some("a b"));
    }

    #[test]
    fn tag_block_is_removed() {
        assert_eq!(
            sanitize("ok\u{E0001}\u{E0041}\u{E007F}", 64).as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn soft_hyphen_is_removed() {
        assert_eq!(sanitize("fire\u{00AD}fox", 64).as_deref(), Some("fire fox"));
    }

    #[test]
    fn caps_on_char_boundaries() {
        assert_eq!(sanitize("ключ-🔑-key", 6).as_deref(), Some("ключ-🔑"));
        assert_eq!(sanitize("ab cd", 3).as_deref(), Some("ab"));
        assert_eq!(sanitize("abc", 0), None);
    }

    #[test]
    fn all_invisible_is_none() {
        assert_eq!(sanitize("\u{200B}\u{FEFF}\u{2066} \n\u{E0020}", 64), None);
        assert_eq!(sanitize("", 64), None);
    }

    #[test]
    fn whitespace_is_collapsed_and_trimmed() {
        assert_eq!(
            sanitize("  a \u{3000}\u{00A0}  b\r\n", 64).as_deref(),
            Some("a b")
        );
    }
}
