//! Text templates with placeholder fallback chains.
//!
//! Grammar:
//!
//! - `{path}` inserts the value of a key from [`KNOWN`]. A path is one or more
//!   segments of `a-z`, `0-9` and `_`, separated by `.`.
//! - `{a|b|"literal"}` inserts the first key with a non-empty value. A
//!   double-quoted literal always applies when reached; inside it `\"` and
//!   `\\` are the only escapes.
//! - `{{` and `}}` insert `{` and `}`.

use std::collections::BTreeMap;
use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;

use crate::placeholders::KNOWN;

/// A parsed template whose keys are all in [`KNOWN`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Template {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Text(String),
    Slot(Vec<Choice>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    Key(String),
    Literal(String),
}

/// A template syntax error located by a byte range of the source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} at bytes {}..{}", span.start, span.end)]
pub struct TemplateError {
    pub span: Range<usize>,
    pub kind: TemplateErrorKind,
}

/// Kind of a [`TemplateError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateErrorKind {
    #[error("unclosed `{{`")]
    UnclosedBrace,
    #[error("empty placeholder")]
    EmptyPlaceholder,
    #[error("invalid character {0:?} in placeholder")]
    InvalidPathChar(char),
    #[error("unknown placeholder key `{0}`")]
    UnknownKey(String),
    #[error("unterminated string literal")]
    UnterminatedString,
    #[error("invalid escape `\\{0}` in string literal")]
    InvalidEscape(char),
    #[error("unmatched `}}`; write `}}}}` for a literal brace")]
    StrayCloseBrace,
}

type Chars<'a> = Peekable<CharIndices<'a>>;

impl Template {
    /// Parses a template.
    ///
    /// # Errors
    ///
    /// Returns the first syntax error, or [`TemplateErrorKind::UnknownKey`]
    /// for a key outside [`KNOWN`].
    pub fn parse(src: &str) -> Result<Self, TemplateError> {
        let mut segments = Vec::new();
        let mut text = String::new();
        let mut chars = src.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '{' if chars.next_if(|&(_, n)| n == '{').is_some() => text.push('{'),
                '}' if chars.next_if(|&(_, n)| n == '}').is_some() => text.push('}'),
                '{' => {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    segments.push(Segment::Slot(parse_slot(src, i, &mut chars)?));
                }
                '}' => return Err(error(i..i + 1, TemplateErrorKind::StrayCloseBrace)),
                _ => text.push(c),
            }
        }
        if !text.is_empty() {
            segments.push(Segment::Text(text));
        }
        Ok(Self { segments })
    }

    /// Renders the template.
    ///
    /// Each placeholder takes the first key whose value is present and
    /// non-empty, or the first literal reached; otherwise it renders empty.
    /// The output is raw text: callers that display it as markup must escape it.
    #[must_use]
    pub fn render(&self, values: &BTreeMap<String, String>) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => out.push_str(text),
                Segment::Slot(choices) => {
                    if let Some(value) = choices.iter().find_map(|choice| match choice {
                        Choice::Key(key) => values.get(key).filter(|v| !v.is_empty()),
                        Choice::Literal(literal) => Some(literal),
                    }) {
                        out.push_str(value);
                    }
                }
            }
        }
        out
    }

    /// Returns every key referenced by the template, in source order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.segments
            .iter()
            .flat_map(|segment| match segment {
                Segment::Slot(choices) => choices.as_slice(),
                Segment::Text(_) => &[],
            })
            .filter_map(|choice| match choice {
                Choice::Key(key) => Some(key.as_str()),
                Choice::Literal(_) => None,
            })
    }
}

/// Returns the parsed form of `Touch {device.vendor|"your security key"}`.
pub(crate) fn default_title() -> Template {
    Template {
        segments: vec![
            Segment::Text("Touch ".to_owned()),
            Segment::Slot(vec![
                Choice::Key("device.vendor".to_owned()),
                Choice::Literal("your security key".to_owned()),
            ]),
        ],
    }
}

/// Returns the parsed form of
/// `{requester.label|process.name|"An application"} is waiting for {request.method}`.
pub(crate) fn default_body() -> Template {
    Template {
        segments: vec![
            Segment::Slot(vec![
                Choice::Key("requester.label".to_owned()),
                Choice::Key("process.name".to_owned()),
                Choice::Literal("An application".to_owned()),
            ]),
            Segment::Text(" is waiting for ".to_owned()),
            Segment::Slot(vec![Choice::Key("request.method".to_owned())]),
        ],
    }
}

fn error(span: Range<usize>, kind: TemplateErrorKind) -> TemplateError {
    TemplateError { span, kind }
}

fn char_span(at: usize, c: char) -> Range<usize> {
    at..at + c.len_utf8()
}

/// Parses the choices of a placeholder whose `{` is at byte `open`.
fn parse_slot(src: &str, open: usize, chars: &mut Chars<'_>) -> Result<Vec<Choice>, TemplateError> {
    let unclosed = || error(open..src.len(), TemplateErrorKind::UnclosedBrace);
    let mut choices = Vec::new();
    loop {
        let &(start, first) = chars.peek().ok_or_else(unclosed)?;
        let choice = if first == '"' {
            chars.next();
            parse_literal(src, start, chars)?
        } else {
            parse_path(src, open, start, chars)?
        };
        choices.push(choice);
        match chars.next() {
            None => return Err(unclosed()),
            Some((_, '|')) => {}
            Some((_, '}')) => return Ok(choices),
            Some((at, c)) => {
                return Err(error(
                    char_span(at, c),
                    TemplateErrorKind::InvalidPathChar(c),
                ));
            }
        }
    }
}

/// Parses a key starting at byte `start`, leaving the delimiter unconsumed.
fn parse_path(
    src: &str,
    open: usize,
    start: usize,
    chars: &mut Chars<'_>,
) -> Result<Choice, TemplateError> {
    let mut end = start;
    while let Some((at, c)) = chars
        .next_if(|&(_, c)| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
    {
        end = at + c.len_utf8();
    }
    match chars.peek() {
        None => return Err(error(open..src.len(), TemplateErrorKind::UnclosedBrace)),
        Some(&(at, c)) if c != '|' && c != '}' => {
            return Err(error(
                char_span(at, c),
                TemplateErrorKind::InvalidPathChar(c),
            ));
        }
        Some(&(at, c)) if start == end => {
            return Err(error(
                open..at + c.len_utf8(),
                TemplateErrorKind::EmptyPlaceholder,
            ));
        }
        Some(_) => {}
    }
    let path = src.get(start..end).unwrap_or_default();
    let bytes = path.as_bytes();
    for (offset, &b) in bytes.iter().enumerate() {
        let prev_dot = offset == 0 || bytes.get(offset - 1) == Some(&b'.');
        let last = offset + 1 == bytes.len();
        if b == b'.' && (prev_dot || last) {
            let at = start + offset;
            return Err(error(at..at + 1, TemplateErrorKind::InvalidPathChar('.')));
        }
    }
    if !KNOWN.contains(&path) {
        return Err(error(
            start..end,
            TemplateErrorKind::UnknownKey(path.to_owned()),
        ));
    }
    Ok(Choice::Key(path.to_owned()))
}

/// Parses a string literal whose opening quote at byte `quote` is consumed.
fn parse_literal(src: &str, quote: usize, chars: &mut Chars<'_>) -> Result<Choice, TemplateError> {
    let unterminated = || error(quote..src.len(), TemplateErrorKind::UnterminatedString);
    let mut literal = String::new();
    loop {
        match chars.next().ok_or_else(unterminated)? {
            (_, '"') => return Ok(Choice::Literal(literal)),
            (at, '\\') => match chars.next().ok_or_else(unterminated)? {
                (_, c @ ('"' | '\\')) => literal.push(c),
                (next, c) => {
                    return Err(error(
                        at..next + c.len_utf8(),
                        TemplateErrorKind::InvalidEscape(c),
                    ));
                }
            },
            (_, c) => literal.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_error::TestResult;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn err(src: &str) -> Option<(Range<usize>, TemplateErrorKind)> {
        match Template::parse(src) {
            Ok(_) => None,
            Err(TemplateError { span, kind }) => Some((span, kind)),
        }
    }

    #[test]
    fn plain_text_and_placeholders_render() -> TestResult {
        let t = Template::parse("Touch {device.vendor} now")?;
        assert_eq!(
            t.render(&values(&[("device.vendor", "Yubico")])),
            "Touch Yubico now"
        );
        assert_eq!(t.render(&values(&[])), "Touch  now");
        Ok(())
    }

    #[test]
    fn fallback_chain_takes_first_non_empty() -> TestResult {
        let t = Template::parse("{app.name|process.name|\"An application\"}")?;
        assert_eq!(
            t.render(&values(&[
                ("app.name", "Firefox"),
                ("process.name", "firefox")
            ])),
            "Firefox"
        );
        assert_eq!(
            t.render(&values(&[("app.name", ""), ("process.name", "ssh")])),
            "ssh"
        );
        assert_eq!(t.render(&values(&[])), "An application");
        let keys: Vec<&str> = t.keys().collect();
        assert_eq!(keys, ["app.name", "process.name"]);
        Ok(())
    }

    #[test]
    fn literal_wins_when_reached() -> TestResult {
        let t = Template::parse("[{\"\"|app.name}]")?;
        assert_eq!(t.render(&values(&[("app.name", "x")])), "[]");
        Ok(())
    }

    #[test]
    fn literal_only_slot() -> TestResult {
        let t = Template::parse("a{\"b\"}c")?;
        assert_eq!(t.render(&values(&[])), "abc");
        assert_eq!(t.keys().count(), 0);
        Ok(())
    }

    #[test]
    fn doubled_brace_adjacent_to_slot() -> TestResult {
        let t = Template::parse("{{{app.name}")?;
        assert_eq!(t.render(&values(&[("app.name", "x")])), "{x");
        let t = Template::parse("{app.name}}}")?;
        assert_eq!(t.render(&values(&[("app.name", "x")])), "x}");
        Ok(())
    }

    #[test]
    fn unresolved_placeholder_renders_empty() -> TestResult {
        let t = Template::parse("a{app.name|process.name}b")?;
        assert_eq!(t.render(&values(&[("app.name", "")])), "ab");
        Ok(())
    }

    #[test]
    fn literal_escapes() -> TestResult {
        let t = Template::parse(r#"{"say \"hi\" \\ {}|"}"#)?;
        assert_eq!(t.render(&values(&[])), r#"say "hi" \ {}|"#);
        Ok(())
    }

    #[test]
    fn doubled_braces_are_literal() -> TestResult {
        let t = Template::parse("{{{request.count}}}")?;
        assert_eq!(t.render(&values(&[("request.count", "2")])), "{2}");
        assert_eq!(Template::parse("}}{{")?.render(&values(&[])), "}{");
        Ok(())
    }

    #[test]
    fn multibyte_text_is_preserved() -> TestResult {
        let t = Template::parse("Berühre {device.vendor}…")?;
        assert_eq!(
            t.render(&values(&[("device.vendor", "ключ")])),
            "Berühre ключ…"
        );
        Ok(())
    }

    #[test]
    fn unclosed_brace() {
        assert_eq!(
            err("ab{app.name"),
            Some((2..11, TemplateErrorKind::UnclosedBrace))
        );
        assert_eq!(err("{"), Some((0..1, TemplateErrorKind::UnclosedBrace)));
        assert_eq!(
            err("{app.name|"),
            Some((0..10, TemplateErrorKind::UnclosedBrace))
        );
        assert_eq!(
            err("{\"x\""),
            Some((0..4, TemplateErrorKind::UnclosedBrace))
        );
    }

    #[test]
    fn empty_placeholder() {
        assert_eq!(
            err("a{}"),
            Some((1..3, TemplateErrorKind::EmptyPlaceholder))
        );
        assert_eq!(
            err("{app.name|}"),
            Some((0..11, TemplateErrorKind::EmptyPlaceholder))
        );
        assert_eq!(
            err("{|app.name}"),
            Some((0..2, TemplateErrorKind::EmptyPlaceholder))
        );
    }

    #[test]
    fn invalid_path_character() {
        assert_eq!(
            err("{app.Name}"),
            Some((5..6, TemplateErrorKind::InvalidPathChar('N')))
        );
        assert_eq!(
            err("{app name}"),
            Some((4..5, TemplateErrorKind::InvalidPathChar(' ')))
        );
        assert_eq!(
            err("{app..name}"),
            Some((5..6, TemplateErrorKind::InvalidPathChar('.')))
        );
        assert_eq!(
            err("{.app}"),
            Some((1..2, TemplateErrorKind::InvalidPathChar('.')))
        );
        assert_eq!(
            err("{app.}"),
            Some((4..5, TemplateErrorKind::InvalidPathChar('.')))
        );
        assert_eq!(
            err("{\"x\"y}"),
            Some((4..5, TemplateErrorKind::InvalidPathChar('y')))
        );
        assert_eq!(
            err("{é}"),
            Some((1..3, TemplateErrorKind::InvalidPathChar('é')))
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert_eq!(
            err("x {app.title}"),
            Some((3..12, TemplateErrorKind::UnknownKey("app.title".to_owned())))
        );
    }

    #[test]
    fn unterminated_string() {
        assert_eq!(
            err("{\"abc"),
            Some((1..5, TemplateErrorKind::UnterminatedString))
        );
        assert_eq!(
            err("{\"abc\\"),
            Some((1..6, TemplateErrorKind::UnterminatedString))
        );
    }

    #[test]
    fn invalid_escape() {
        assert_eq!(
            err(r#"{"a\n"}"#),
            Some((3..5, TemplateErrorKind::InvalidEscape('n')))
        );
    }

    #[test]
    fn stray_close_brace() {
        assert_eq!(
            err("ab}c"),
            Some((2..3, TemplateErrorKind::StrayCloseBrace))
        );
        assert_eq!(err("}}}"), Some((2..3, TemplateErrorKind::StrayCloseBrace)));
    }

    #[test]
    fn error_display_names_kind_and_span() -> TestResult {
        let Err(error) = Template::parse("a}") else {
            return Err("the template was accepted".into());
        };
        assert_eq!(
            error.to_string(),
            "unmatched `}`; write `}}` for a literal brace at bytes 1..2"
        );
        Ok(())
    }
}
