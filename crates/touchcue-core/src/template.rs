//! Prompt templates in Jinja syntax, rendered by minijinja.
//!
//! Every placeholder key `a.b` of [`KNOWN`] is the attribute `b` of the
//! variable `a`, and every value is a string, so comparisons compare
//! strings. An absent value is undefined: it renders empty and is false,
//! like an empty string. Built-in filters other than [`REMOVED_FILTERS`],
//! built-in tests and the global functions `range`, `dict` and `namespace`
//! are available; macros, `include`, `extends` and `import` are not. Output
//! is not escaped, and one trailing newline of the template is removed.
//!
//! Templates are trusted configuration. [`FUEL`] and [`OUTPUT_MAX`] bound a
//! render's instructions and output, but not the strings it builds on the
//! way: one `*` may build 100 MB, and `~` may double a string per
//! instruction.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::ops::Range;
use std::sync::Arc;

use minijinja::{AutoEscape, Environment, ErrorKind, UndefinedBehavior};

use crate::placeholders::KNOWN;

/// Most instructions one render executes; a render that needs more fails.
/// The default templates and the documented examples need at most 30.
pub const FUEL: u64 = 1000;
/// Longest rendered output, in bytes; a render that writes more fails.
pub const OUTPUT_MAX: usize = 64 * 1024;
/// Deepest nesting of blocks, loops and calls while rendering.
pub const RECURSION_LIMIT: usize = 32;
/// Built-in filters that are not available, because they allocate memory in
/// proportion to an argument rather than to their input.
pub const REMOVED_FILTERS: [&str; 4] = ["format", "indent", "slice", "batch"];

/// A compiled template whose variables are all in [`KNOWN`].
///
/// Clones share the compiled form. Two templates are equal when their names
/// and sources are equal.
#[derive(Clone)]
pub struct Template {
    name: String,
    source: String,
    env: Arc<Environment<'static>>,
    /// Why `source` is not in `env`, for a template made by [`Template::uncompiled`].
    error: Option<TemplateError>,
}

/// A template that does not compile or names an unknown placeholder.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TemplateError {
    #[error(transparent)]
    Syntax { source: Arc<minijinja::Error> },
    /// A variable or attribute outside [`KNOWN`]; `span` is its first
    /// occurrence in the template text, preferring one inside a block.
    #[error("unknown placeholder `{name}`")]
    UnknownKey {
        name: String,
        span: Option<Range<usize>>,
    },
    /// A method call, such as `app.name.upper()`; `name` is the path
    /// before the parentheses.
    #[error("methods are not supported: `{name}()`; use filters such as `| upper`")]
    MethodCall {
        name: String,
        span: Option<Range<usize>>,
    },
    /// Text with no Jinja block that holds a single-brace placeholder of the
    /// previous template syntax, such as `{app.name|"x"}`; `span` is that
    /// placeholder's `{` and namespace.
    #[error(
        "single braces are plain text; write placeholders as `{{{{ app.name }}}}` and fallbacks with `or` instead of `|`"
    )]
    OldSyntax { span: Range<usize> },
}

/// A template that failed while rendering, such as for running out of
/// [`FUEL`], exceeding [`OUTPUT_MAX`] or an operation on values of the
/// wrong type.
#[derive(Debug, thiserror::Error)]
#[error("cannot render `{name}`")]
pub struct RenderError {
    /// Name of the template: the configuration field it comes from.
    pub name: String,
    source: minijinja::Error,
}

impl TemplateError {
    /// Returns what is wrong, without the template name and line.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Syntax { source } => match source.detail() {
                Some(detail) => detail.to_owned(),
                None => source.kind().to_string(),
            },
            Self::UnknownKey { .. } | Self::MethodCall { .. } | Self::OldSyntax { .. } => {
                self.to_string()
            }
        }
    }

    /// Returns the byte range of the template text the error points at.
    #[must_use]
    pub fn span(&self) -> Option<Range<usize>> {
        match self {
            Self::Syntax { source } => source.range(),
            Self::UnknownKey { span, .. } | Self::MethodCall { span, .. } => span.clone(),
            Self::OldSyntax { span } => Some(span.clone()),
        }
    }
}

impl PartialEq for TemplateError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Syntax { source: a }, Self::Syntax { source: b }) => {
                a.kind() == b.kind() && a.detail() == b.detail() && a.range() == b.range()
            }
            (Self::UnknownKey { name: a, span: x }, Self::UnknownKey { name: b, span: y })
            | (Self::MethodCall { name: a, span: x }, Self::MethodCall { name: b, span: y }) => {
                a == b && x == y
            }
            (Self::OldSyntax { span: x }, Self::OldSyntax { span: y }) => x == y,
            _ => false,
        }
    }
}

impl Eq for TemplateError {}

impl RenderError {
    /// Returns the kind of the failure.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.source.kind()
    }
}

impl PartialEq for RenderError {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.kind() == other.kind()
    }
}

impl Eq for RenderError {}

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template")
            .field("name", &self.name)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Template {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.source == other.source
    }
}

impl Eq for Template {}

impl Template {
    /// Compiles `source` as the template `name`, which names it in render
    /// errors.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError::Syntax`] for a syntax error, including a
    /// macro, `include`, `extends` or `import` statement,
    /// [`TemplateError::OldSyntax`] for text in the previous template
    /// syntax, [`TemplateError::MethodCall`] for a method call, and
    /// [`TemplateError::UnknownKey`] for a variable that is neither a key
    /// of [`KNOWN`] written as `a.b`, nor a global function, nor set by the
    /// template.
    pub fn parse(name: &str, source: &str) -> Result<Self, TemplateError> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Chainable);
        env.set_auto_escape_callback(|_| AutoEscape::None);
        // Debug info would copy placeholder values into errors.
        env.set_debug(false);
        env.set_fuel(Some(FUEL));
        env.set_recursion_limit(RECURSION_LIMIT);
        // `debug()` prints every placeholder value.
        env.remove_global("debug");
        for filter in REMOVED_FILTERS {
            env.remove_filter(filter);
        }
        env.add_template_owned(name.to_owned(), source.to_owned())
            .map_err(|error| TemplateError::Syntax {
                source: Arc::new(error),
            })?;
        let template = Self {
            name: name.to_owned(),
            source: source.to_owned(),
            env: Arc::new(env),
            error: None,
        };
        template.check_old_syntax()?;
        template.check_variables()?;
        Ok(template)
    }

    /// Rejects a template without `{{`, `{%` and `{#` that holds `{` followed
    /// by a namespace of [`KNOWN`] and `.`.
    fn check_old_syntax(&self) -> Result<(), TemplateError> {
        let source = self.source.as_str();
        if ["{{", "{%", "{#"].iter().any(|open| source.contains(open)) {
            return Ok(());
        }
        let namespaces: BTreeSet<&str> = KNOWN
            .iter()
            .filter_map(|key| key.split_once('.').map(|(namespace, _)| namespace))
            .collect();
        let old = source.match_indices('{').find_map(|(start, _)| {
            let rest = source.get(start + 1..)?;
            namespaces.iter().find_map(|namespace| {
                let after = rest.strip_prefix(namespace)?;
                after
                    .starts_with('.')
                    .then_some(start..start + 1 + namespace.len())
            })
        });
        match old {
            Some(span) => Err(TemplateError::OldSyntax { span }),
            None => Ok(()),
        }
    }

    /// Rejects the first variable, by position in the source, that is
    /// neither in [`KNOWN`], nor a global, nor set by the template.
    fn check_variables(&self) -> Result<(), TemplateError> {
        let compiled =
            self.env
                .get_template(&self.name)
                .map_err(|error| TemplateError::Syntax {
                    source: Arc::new(error),
                })?;
        let blocks = scan(&self.source);
        let unknown = compiled
            .undeclared_variables(true)
            .into_iter()
            .filter(|name| {
                let root = name.split('.').next().unwrap_or_default();
                !KNOWN.contains(&name.as_str())
                    && !self.env.globals().any(|(global, _)| global == name)
                    && !blocks.assigned.contains(root)
            })
            .map(|name| (blocks.find(&self.source, &name), name))
            .min_by(|(a, x), (b, y)| {
                let start =
                    |span: &Option<Range<usize>>| span.as_ref().map_or(usize::MAX, |s| s.start);
                start(a).cmp(&start(b)).then_with(|| x.cmp(y))
            });
        let Some((span, name)) = unknown else {
            return Ok(());
        };
        let call = span.as_ref().is_some_and(|span| {
            self.source
                .get(span.end..)
                .is_some_and(|rest| rest.trim_start().starts_with('('))
        });
        if call && name.contains('.') {
            Err(TemplateError::MethodCall { name, span })
        } else {
            Err(TemplateError::UnknownKey { name, span })
        }
    }

    /// Returns a template named `name` that failed to compile `source` with
    /// `error`; rendering it fails with `error` as the source.
    #[must_use]
    pub(crate) fn uncompiled(name: &str, source: &str, error: TemplateError) -> Self {
        Self {
            name: name.to_owned(),
            source: source.to_owned(),
            env: Arc::new(Environment::empty()),
            error: Some(error),
        }
    }

    /// Returns the template text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the template name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Renders the template with placeholder `values` keyed by entries of
    /// [`KNOWN`]; keys without a `.` are ignored.
    ///
    /// The output is raw text: callers that display it as markup must
    /// escape it.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError`] when rendering fails, such as for running out
    /// of [`FUEL`], writing more than [`OUTPUT_MAX`] bytes, an operation on
    /// values of the wrong type, or an unknown filter or test.
    pub fn render(&self, values: &BTreeMap<String, String>) -> Result<String, RenderError> {
        self.render_counted(values).map(|(text, _)| text)
    }

    /// Renders as [`Template::render`] and returns the fuel used.
    fn render_counted(
        &self,
        values: &BTreeMap<String, String>,
    ) -> Result<(String, Option<u64>), RenderError> {
        let mut context: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
        for (key, value) in values {
            if let Some((namespace, attribute)) = key.split_once('.') {
                context
                    .entry(namespace)
                    .or_default()
                    .insert(attribute, value);
            }
        }
        let error = |source| RenderError {
            name: self.name.clone(),
            source,
        };
        if let Some(cause) = &self.error {
            return Err(error(
                minijinja::Error::new(ErrorKind::SyntaxError, "template did not compile")
                    .with_source(cause.clone()),
            ));
        }
        let template = self.env.get_template(&self.name).map_err(error)?;
        let mut output = Capped(Vec::new());
        let captured = template
            .render_captured_to(&context, &mut output)
            .map_err(error)?;
        let fuel = captured.state().fuel_levels().map(|(consumed, _)| consumed);
        // Whole `&str` chunks are written, so the bytes are UTF-8.
        let text = String::from_utf8(output.0).map_err(|utf8| {
            error(
                minijinja::Error::new(ErrorKind::WriteFailure, "output is not UTF-8")
                    .with_source(utf8),
            )
        })?;
        Ok((text, fuel))
    }
}

/// Output buffer that rejects a write past [`OUTPUT_MAX`] bytes.
struct Capped(Vec<u8>);

impl io::Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(buf.len()) > OUTPUT_MAX {
            return Err(io::Error::other("output exceeds the limit"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Blocks and assigned names found by reading the template text.
struct Scan<'a> {
    /// Byte ranges of `{{ … }}` and `{% … %}` blocks outside `raw` blocks.
    blocks: Vec<Range<usize>>,
    /// Names after `set`, which `undeclared_variables` misses when the
    /// `set` is inside an `if` or loop body.
    assigned: BTreeSet<&'a str>,
}

/// Reads the blocks and `set` targets of `source`, skipping comments and
/// `raw` blocks.
fn scan(source: &str) -> Scan<'_> {
    let mut scan = Scan {
        blocks: Vec::new(),
        assigned: BTreeSet::new(),
    };
    let mut pos = 0;
    while let Some(offset) = source.get(pos..).and_then(|rest| rest.find('{')) {
        let start = pos + offset;
        let rest = source.get(start..).unwrap_or_default();
        let close = if rest.starts_with("{{") {
            "}}"
        } else if rest.starts_with("{%") {
            "%}"
        } else if rest.starts_with("{#") {
            "#}"
        } else {
            pos = start + 1;
            continue;
        };
        let end = match rest.get(2..).and_then(|inner| inner.find(close)) {
            Some(at) => start + 2 + at + 2,
            None => source.len(),
        };
        pos = end;
        if close == "#}" {
            continue;
        }
        scan.blocks.push(start..end);
        if close != "%}" {
            continue;
        }
        let inner = source.get(start + 2..end).unwrap_or_default();
        let mut words = inner.trim_start_matches(['-', '+']).split_whitespace();
        match words.next() {
            Some("raw") => {
                pos = source
                    .get(end..)
                    .and_then(|after| after.find("endraw"))
                    .and_then(|at| {
                        let from = end + at;
                        source.get(from..)?.find("%}").map(|close| from + close + 2)
                    })
                    .unwrap_or(source.len());
            }
            Some("set") => {
                if let Some(target) = words.next() {
                    let len = target
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .unwrap_or(target.len());
                    if let Some(name) = target.get(..len).filter(|name| !name.is_empty()) {
                        scan.assigned.insert(name);
                    }
                }
            }
            _ => {}
        }
    }
    scan
}

impl Scan<'_> {
    /// Returns the bytes of the first occurrence of `name` in `source` that
    /// is not part of a longer name, preferring one inside a block.
    fn find(&self, source: &str, name: &str) -> Option<Range<usize>> {
        let is_name = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
        let mut found = source.match_indices(name).filter_map(|(start, _)| {
            let end = start + name.len();
            let before = source.get(..start)?.chars().next_back();
            let after = source.get(end..)?.chars().next();
            (!before.is_some_and(is_name) && !after.is_some_and(is_name)).then_some(start..end)
        });
        let first = found.next()?;
        let in_block = |span: &Range<usize>| {
            self.blocks
                .iter()
                .any(|block| block.start <= span.start && span.end <= block.end)
        };
        if in_block(&first) {
            return Some(first);
        }
        Some(found.find(in_block).unwrap_or(first))
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

    fn render(source: &str, pairs: &[(&str, &str)]) -> TestResult<String> {
        Ok(Template::parse("t", source)?.render(&values(pairs))?)
    }

    fn rejected(source: &str) -> TestResult<TemplateError> {
        match Template::parse("t", source) {
            Ok(_) => Err(format!("{source:?} was accepted").into()),
            Err(error) => Ok(error),
        }
    }

    fn render_error(source: &str, pairs: &[(&str, &str)]) -> TestResult<ErrorKind> {
        match Template::parse("t", source)?.render(&values(pairs)) {
            Ok(_) => Err(format!("{source:?} rendered").into()),
            Err(error) => Ok(error.kind()),
        }
    }

    #[test]
    fn placeholders_render_and_absent_ones_are_empty() -> TestResult {
        let source = "Touch {{ device.vendor }} now";
        assert_eq!(
            render(source, &[("device.vendor", "Yubico")])?,
            "Touch Yubico now"
        );
        assert_eq!(render(source, &[])?, "Touch  now");
        assert_eq!(render("[{{ app.name }}]", &[("process.name", "x")])?, "[]");
        Ok(())
    }

    #[test]
    fn or_takes_the_first_non_empty_value() -> TestResult {
        let source = "{{ app.name or process.name or \"An application\" }}";
        let both = [("app.name", "Firefox"), ("process.name", "firefox")];
        assert_eq!(render(source, &both)?, "Firefox");
        assert_eq!(
            render(source, &[("app.name", ""), ("process.name", "ssh")])?,
            "ssh"
        );
        assert_eq!(render(source, &[])?, "An application");
        Ok(())
    }

    #[test]
    fn conditions_and_filters() -> TestResult {
        let optional = "{{ requester.name }}{% if app.name %} ({{ app.name }}){% endif %}";
        let pairs = [("requester.name", "claude"), ("app.name", "kitty")];
        assert_eq!(render(optional, &pairs)?, "claude (kitty)");
        assert_eq!(render(optional, &[("requester.name", "claude")])?, "claude");
        assert_eq!(
            render(optional, &[("requester.name", "claude"), ("app.name", "")])?,
            "claude"
        );

        let branch =
            "{% if request.method == \"ssh\" %}SSH{% else %}{{ request.method }}{% endif %}";
        assert_eq!(render(branch, &[("request.method", "ssh")])?, "SSH");
        assert_eq!(render(branch, &[("request.method", "fido2")])?, "fido2");

        assert_eq!(
            render("{{ requester.name | upper }}", &[("requester.name", "gpg")])?,
            "GPG"
        );
        assert_eq!(render("{{ app.name | default(\"none\") }}", &[])?, "none");
        let count = "{% if request.count | int > 1 %}again{% endif %}";
        assert_eq!(render(count, &[("request.count", "2")])?, "again");
        assert_eq!(render(count, &[("request.count", "1")])?, "");
        Ok(())
    }

    #[test]
    fn comparisons_are_between_strings() -> TestResult {
        let source = "{% if request.count == \"2\" %}two{% endif %}{% if request.count == 2 %}int{% endif %}";
        assert_eq!(render(source, &[("request.count", "2")])?, "two");
        Ok(())
    }

    #[test]
    fn output_is_not_escaped_and_one_trailing_newline_is_removed() -> TestResult {
        assert_eq!(
            render("<b>{{ app.name }}</b>\n", &[("app.name", "a&b")])?,
            "<b>a&b</b>"
        );
        Ok(())
    }

    #[test]
    fn whitespace_control_trims_around_blocks() -> TestResult {
        let source = "a\n{%- if app.name %}\n  {{- app.name }}\n{%- endif %}\nb";
        assert_eq!(render(source, &[("app.name", "x")])?, "ax\nb");
        Ok(())
    }

    #[test]
    fn set_with_and_for_bind_their_own_names() -> TestResult {
        let source =
            "{% set n = requester.name %}{% with a = app.name %}{{ n }}/{{ a }}{% endwith %}";
        assert_eq!(
            render(source, &[("requester.name", "nu"), ("app.name", "foot")])?,
            "nu/foot"
        );
        let source = "{% if app.name %}{% set n = app.name %}{% endif %}[{{ n }}]";
        assert_eq!(render(source, &[("app.name", "foot")])?, "[foot]");
        assert_eq!(render(source, &[])?, "[]");
        let source = "{% if app.name %}{% set n %}{{ app.name }}!{% endset %}{% endif %}{{ n }}";
        assert_eq!(render(source, &[("app.name", "foot")])?, "foot!");
        let source = "{% for c in request.method %}{{ loop.index }}{{ c }}{% endfor %}";
        assert_eq!(render(source, &[("request.method", "ssh")])?, "1s2s3h");
        let source = "{% for i in range(3) %}{{ i }}{% endfor %}";
        assert_eq!(render(source, &[])?, "012");
        let source = "{% filter upper %}{{ app.name }}{% endfilter %}{% raw %} {{ }}{% endraw %}";
        assert_eq!(render(source, &[("app.name", "foot")])?, "FOOT {{ }}");
        Ok(())
    }

    #[test]
    fn unknown_names_are_rejected_at_their_first_occurrence() -> TestResult {
        for (source, name, span) in [
            ("x {{ app.title }}", "app.title", Some(5..14)),
            ("{{ app.name }}{{ app.nope }}", "app.nope", Some(17..25)),
            ("see app.nope {{ app.nope }}", "app.nope", Some(16..24)),
            ("{{ app }}", "app", Some(3..6)),
            ("{{ app[\"name\"] }}", "app", Some(3..6)),
            ("{{ app.name.first }}", "app.name.first", Some(3..17)),
            ("{{ colour }}", "colour", Some(3..9)),
            ("{{ debug() }}", "debug", Some(3..8)),
            ("{% set n = app.nope %}{{ n }}", "app.nope", Some(11..19)),
            ("{{ app . nope }}", "app.nope", None),
            (
                "{% raw %}{% set fake = 1 %}{% endraw %}{{ fake }}",
                "fake",
                Some(42..46),
            ),
            ("{# {% set n = 1 %} #}{{ n }}", "n", Some(24..25)),
        ] {
            assert_eq!(
                rejected(source)?,
                TemplateError::UnknownKey {
                    name: name.to_owned(),
                    span,
                },
                "{source:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn method_calls_are_rejected() -> TestResult {
        let error = rejected("{{ app.name.upper() }}")?;
        assert_eq!(
            error,
            TemplateError::MethodCall {
                name: "app.name.upper".to_owned(),
                span: Some(3..17),
            }
        );
        assert_eq!(
            error.reason(),
            "methods are not supported: `app.name.upper()`; use filters such as `| upper`"
        );
        Ok(())
    }

    #[test]
    fn old_syntax_is_rejected() -> TestResult {
        for (source, span) in [
            ("Touch {device.vendor|\"your security key\"}", 6..13),
            (
                "{requester.label|process.name|\"An application\"} is waiting for {request.method}",
                0..10,
            ),
            ("{app.name}", 0..4),
        ] {
            assert_eq!(
                rejected(source)?,
                TemplateError::OldSyntax { span },
                "{source:?}"
            );
        }
        assert_eq!(render("{x} {apps.y}", &[])?, "{x} {apps.y}");
        assert_eq!(
            render("{app.name} {{ app.name }}", &[("app.name", "a")])?,
            "{app.name} a"
        );
        Ok(())
    }

    #[test]
    fn syntax_errors_carry_a_span() -> TestResult {
        for (source, span, message) in [
            (
                "{{ app.name }",
                12..13,
                "unexpected `}`, expected end of variable block",
            ),
            ("ab\ncd {% if %}", 12..14, "unexpected end of block"),
            (
                "{% macro m() %}{% endmacro %}",
                3..8,
                "unknown statement macro",
            ),
            ("{% include \"x\" %}", 3..10, "unknown statement include"),
            ("{% extends \"x\" %}", 3..10, "unknown statement extends"),
            ("{% import \"x\" as y %}", 3..9, "unknown statement import"),
        ] {
            let error = rejected(source)?;
            assert!(matches!(error, TemplateError::Syntax { .. }), "{source:?}");
            assert_eq!(error.span(), Some(span), "{source:?}");
            assert_eq!(error.reason(), message, "{source:?}");
        }
        Ok(())
    }

    #[test]
    fn render_failures_are_errors() -> TestResult {
        let template = Template::parse("templates.body", "{{ request.count + 1 }}")?;
        let Err(error) = template.render(&values(&[("request.count", "2")])) else {
            return Err("the template rendered".into());
        };
        assert_eq!(error.name, "templates.body");
        assert_eq!(error.kind(), ErrorKind::InvalidOperation);
        assert_eq!(
            render_error("{{ app.name | nosuch }}", &[])?,
            ErrorKind::UnknownFilter
        );
        Ok(())
    }

    #[test]
    fn allocating_filters_are_removed() -> TestResult {
        for source in [
            "{{ \"%999999999999s\" | format(\"a\") }}",
            "{{ \"{:>999999999999}\" | format(\"a\") }}",
            "{{ app.name | indent(999999999999) }}",
            "{{ request.method | slice(999999999999) }}",
            "{{ request.method | batch(999999999999) }}",
        ] {
            assert_eq!(
                render_error(source, &[("app.name", "a"), ("request.method", "u2f")])?,
                ErrorKind::UnknownFilter,
                "{source:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn fuel_and_output_cap_stop_large_renders() -> TestResult {
        let detail = "x".repeat(200);
        let nested =
            "{% for a in request.detail %}{% for b in request.detail %}.{% endfor %}{% endfor %}";
        assert_eq!(
            render_error(nested, &[("request.detail", &detail)])?,
            ErrorKind::OutOfFuel
        );
        let doubled = "{% set ns = namespace(s=\"x\" * 1000) %}{% for i in range(10) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}{{ ns.s }}";
        assert_eq!(render_error(doubled, &[])?, ErrorKind::WriteFailure);
        let long_loop = "{% for i in range(1000) %}.{% endfor %}";
        assert_eq!(render_error(long_loop, &[])?, ErrorKind::OutOfFuel);
        let long = "{{ request.detail * 400 }}";
        assert_eq!(
            render_error(long, &[("request.detail", &detail)])?,
            ErrorKind::WriteFailure
        );
        Ok(())
    }

    #[test]
    fn default_and_documented_templates_need_little_fuel() -> TestResult {
        let all: Vec<(&str, String)> = KNOWN.iter().map(|key| (*key, "x".repeat(200))).collect();
        let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        for source in [
            "Touch {{ device.vendor or \"your security key\" }}",
            "{{ requester.label or process.name or \"An application\" }} is waiting for {{ request.method }}",
            "{{ requester.label or \"ssh\" }} is signing in{% if request.detail %}: {{ request.detail }}{% endif %}",
            "{{ requester.name }}{% if app.name %} ({{ app.name }}){% endif %}",
            "{% if request.method == \"openpgp\" %}a{% else %}{{ requester.name | upper }}{% endif %}",
        ] {
            for pairs in [&all[..], &[]] {
                let (_, fuel) = Template::parse("t", source)?.render_counted(&values(pairs))?;
                let fuel = fuel.ok_or("no fuel tracking")?;
                assert!(fuel <= 30, "{source:?} used {fuel}");
            }
        }
        Ok(())
    }
}
