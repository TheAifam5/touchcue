//! Prompt text shared by the popup and notification backends.

use touchcue_core::text::{outcome_body, sanitize};

use crate::{Command, Prompt};

/// Longest title shown, in characters.
const MAX_TITLE_CHARS: usize = 120;
/// Longest body shown, in characters.
const MAX_BODY_CHARS: usize = 400;

/// Replaces control and invisible characters in the prompt text and caps
/// its length; text with nothing visible becomes empty.
pub(crate) fn sanitize_command(cmd: Command) -> Command {
    let clean = |mut prompt: Prompt| {
        prompt.title = sanitize(&prompt.title, MAX_TITLE_CHARS).unwrap_or_default();
        prompt.body = sanitize(&prompt.body, MAX_BODY_CHARS).unwrap_or_default();
        prompt
    };
    match cmd {
        Command::Show(prompt) => Command::Show(clean(prompt)),
        Command::Update(prompt) => Command::Update(clean(prompt)),
        Command::Hide(id) => Command::Hide(id),
    }
}

/// Returns the body as shown, with the outcome of a cancelled, failed or
/// timed-out request appended; see [`outcome_body`].
pub(crate) fn display_body(prompt: &Prompt) -> String {
    outcome_body(&prompt.body, prompt.state)
}

/// Escapes `&`, `<` and `>` for a notification server that parses body markup.
pub(crate) fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Breaks `text` into lines no wider than `max_width` at word boundaries.
///
/// `advance` returns the width of one character. Explicit newlines are
/// kept, runs of other whitespace collapse to one space, and a word wider
/// than `max_width` is split between characters. At most `max_lines` lines
/// are returned; when text is cut off, the last line ends with an ellipsis.
pub(crate) fn wrap(
    text: &str,
    max_width: f32,
    max_lines: usize,
    advance: impl Fn(char) -> f32,
) -> Vec<String> {
    let width = |s: &str| s.chars().map(&advance).sum::<f32>();
    let space = advance(' ');
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0.0;
        for word in paragraph.split_whitespace() {
            let word_width = width(word);
            let gap = if line.is_empty() { 0.0 } else { space };
            if line_width + gap + word_width <= max_width {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
                line_width += gap + word_width;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0.0;
            }
            for ch in word.chars() {
                let w = advance(ch);
                if line_width + w > max_width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0.0;
                }
                line.push(ch);
                line_width += w;
            }
        }
        lines.push(line);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use touchcue_core::{EndReason, RequestState};

    use super::*;

    fn prompt(body: &str, state: RequestState) -> Prompt {
        Prompt {
            id: touchcue_core::RequestId(1),
            title: "t".to_owned(),
            body: body.to_owned(),
            icon: None,
            state,
        }
    }

    fn mono(_: char) -> f32 {
        1.0
    }

    #[test]
    fn body_shows_outcome() {
        let waiting = prompt("ssh waits", RequestState::Waiting);
        assert_eq!(display_body(&waiting), "ssh waits");
        let touched = prompt("ssh waits", RequestState::Lingering(EndReason::Touched));
        assert_eq!(display_body(&touched), "ssh waits");
        let cancelled = prompt("ssh waits", RequestState::Lingering(EndReason::Cancelled));
        assert_eq!(display_body(&cancelled), "ssh waits (cancelled)");
        let failed = prompt("ssh waits", RequestState::Lingering(EndReason::Failed));
        assert_eq!(display_body(&failed), "ssh waits (cancelled)");
        let timed_out = prompt("", RequestState::Lingering(EndReason::TimedOut));
        assert_eq!(display_body(&timed_out), "(timed out)");
    }

    #[test]
    fn prompt_text_is_sanitized() {
        let mut dirty = prompt(
            &format!("a\u{202E}b\n{}", "x".repeat(500)),
            RequestState::Waiting,
        );
        dirty.title = "\u{1b}[31mTouch\u{200B}".to_owned();
        let cleaned = sanitize_command(Command::Show(dirty));
        assert!(matches!(cleaned, Command::Show(_)));
        let Command::Show(clean) = cleaned else {
            return;
        };
        assert_eq!(clean.title, "[31mTouch");
        assert!(clean.body.starts_with("a b x"));
        assert_eq!(clean.body.chars().count(), MAX_BODY_CHARS);
        assert_eq!(
            sanitize_command(Command::Hide(touchcue_core::RequestId(3))),
            Command::Hide(touchcue_core::RequestId(3))
        );
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("<b>a & b</b>"),
            "&lt;b&gt;a &amp; b&lt;/b&gt;"
        );
        assert_eq!(escape_markup("plain 'text'"), "plain 'text'");
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(
            wrap("the quick brown fox jumps", 10.0, 10, mono),
            ["the quick", "brown fox", "jumps"]
        );
        assert_eq!(wrap("a   b\n\nc", 10.0, 10, mono), ["a b", "", "c"]);
        assert_eq!(wrap("", 10.0, 10, mono), [""]);
    }

    #[test]
    fn splits_long_words() {
        assert_eq!(
            wrap("ab abcdefghij", 4.0, 10, mono),
            ["ab", "abcd", "efgh", "ij"]
        );
        assert_eq!(wrap("abc", 0.0, 10, mono), ["a", "b", "c"]);
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(wrap("a b c d", 1.0, 2, mono), ["a", "b…"]);
    }
}
