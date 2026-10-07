//! User configuration loaded from TOML.
//!
//! Every field has a default, so an empty document is a complete
//! configuration. Unknown fields are rejected.

use std::collections::BTreeMap;
use std::ops::Range;

use miette::SourceSpan;
use serde::Deserialize;
use toml::Spanned;

use crate::placeholders::KNOWN;
use crate::skip::{self, SkipList};
use crate::template::{self, Template, TemplateError};

const DEFAULT_TITLE: &str = "Touch {device.vendor|\"your security key\"}";
/// Upper bound of every configured duration, in milliseconds.
const MAX_MS: u64 = 600_000;
/// Default of `hooks.timeout_ms`.
const DEFAULT_HOOK_TIMEOUT_MS: u64 = 5000;
/// Default of `hooks.concurrency`.
const DEFAULT_HOOK_CONCURRENCY: u64 = 4;
/// Upper bound of `hooks.concurrency`.
const MAX_HOOK_CONCURRENCY: u64 = 32;
/// Lower bound of `sources.fido.keepalive_timeout_ms`; progress signals are
/// sent every third of it, so a smaller value would make them spin.
const MIN_KEEPALIVE_MS: u64 = 100;
const DEFAULT_BODY: &str =
    "{requester.label|process.name|\"An application\"} is waiting for {request.method}";

/// Where touch prompts are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputMode {
    Popup,
    Notification,
    Command,
    Both,
    /// Shows nothing; written `none` in TOML.
    #[serde(rename = "none")]
    Off,
}

/// Screen corner, edge or centre of the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Position {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
}

/// Desktop notification urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Urgency {
    Low,
    Normal,
    #[default]
    Critical,
}

/// The `[output]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Output {
    pub mode: OutputMode,
    /// Output used when `mode` is unavailable.
    pub fallback: OutputMode,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            mode: OutputMode::Popup,
            fallback: OutputMode::Notification,
        }
    }
}

/// Outputs, or monitors, that show the popup.
///
/// Written in TOML as `"focused"`, `"all"`, `"cursor"` or a non-empty list
/// of non-empty output names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum OutputTarget {
    /// The output the compositor picks, or on X11 the monitor under the pointer.
    #[default]
    Focused,
    /// Every output.
    All,
    /// The output under the mouse pointer.
    Cursor,
    /// Each listed output that exists.
    Named(Vec<String>),
}

impl<'de> Deserialize<'de> for OutputTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(OutputTargetVisitor)
    }
}

struct OutputTargetVisitor;

impl<'de> serde::de::Visitor<'de> for OutputTargetVisitor {
    type Value = OutputTarget;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"focused\", \"all\", \"cursor\" or a list of output names")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<OutputTarget, E> {
        match value {
            "focused" => Ok(OutputTarget::Focused),
            "all" => Ok(OutputTarget::All),
            "cursor" => Ok(OutputTarget::Cursor),
            other => Err(E::invalid_value(serde::de::Unexpected::Str(other), &self)),
        }
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<OutputTarget, A::Error> {
        let mut names = Vec::new();
        while let Some(name) = seq.next_element::<String>()? {
            if name.is_empty() {
                return Err(serde::de::Error::custom("output names must not be empty"));
            }
            names.push(name);
        }
        if names.is_empty() {
            return Err(serde::de::Error::custom(
                "the output list must name at least one output",
            ));
        }
        Ok(OutputTarget::Named(names))
    }
}

/// Opacity from 0.0, transparent, to 1.0, opaque; never NaN.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Deserialize)]
#[serde(try_from = "f64")]
pub struct Opacity(f32);

// Construction rejects NaN, so equality is reflexive.
impl Eq for Opacity {}

impl Opacity {
    /// Returns the opacity, within 0.0..=1.0.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }
}

/// An opacity outside 0.0..=1.0, or NaN.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("{value} is outside 0.0..=1.0")]
pub struct OpacityError {
    pub value: f64,
}

impl TryFrom<f64> for Opacity {
    type Error = OpacityError;

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is within 0.0..=1.0"
    )]
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value as f32))
        } else {
            Err(OpacityError { value })
        }
    }
}

/// The `[popup]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Popup {
    pub position: Position,
    pub output: OutputTarget,
    /// Minimum time a shown popup stays visible, in milliseconds, at most 600000.
    pub min_display_ms: u64,
    /// Time a request must wait before the popup appears, in milliseconds, at most 600000.
    pub show_delay_ms: u64,
    /// Blocks clicks on the target outputs while a request waits for a touch.
    pub modal: bool,
    /// Darkening of the outputs behind a modal popup.
    pub modal_dim: Opacity,
    /// A click outside the popup hides the popup of the waiting request.
    pub modal_dismiss: bool,
}

impl Default for Popup {
    fn default() -> Self {
        Self {
            position: Position::default(),
            output: OutputTarget::default(),
            min_display_ms: 800,
            show_delay_ms: 0,
            modal: false,
            modal_dim: Opacity(0.4),
            modal_dismiss: true,
        }
    }
}

/// The `[notification]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notification {
    pub urgency: Urgency,
    /// Time after which a notification is withdrawn regardless of state, in seconds, at most 600.
    pub safety_timeout_s: u64,
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            urgency: Urgency::default(),
            safety_timeout_s: 60,
        }
    }
}

/// The `[templates]` section as written by the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Templates {
    pub title: String,
    pub body: String,
}

impl Default for Templates {
    fn default() -> Self {
        Self {
            title: DEFAULT_TITLE.to_owned(),
            body: DEFAULT_BODY.to_owned(),
        }
    }
}

/// One `[[rules]]` entry as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    /// Placeholder values that must all be present and equal, compared
    /// exactly and case-sensitively, for the rule to apply. Never empty.
    pub matches: BTreeMap<String, String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub icon: Option<String>,
    /// Shows nothing for matching requests.
    pub suppress: bool,
}

/// An event that runs `[[hooks]]` commands, written in TOML in snake case.
///
/// One change of a request can fire several events, in the order of the
/// variants: [`Self::Started`], [`Self::Updated`] or [`Self::Ended`] first,
/// then [`Self::Revived`], [`Self::Waiting`] or [`Self::Lingering`], then
/// the outcome. An outcome fires when the request's state becomes it: when
/// the operation ends and lingers for a client retry, or, for a touch, when
/// the request ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// A request started.
    Started,
    /// A request changed, including its state.
    Updated,
    /// A request ended.
    Ended,
    /// A client retry revived a lingering request.
    Revived,
    /// A request started or was revived and waits for a touch.
    Waiting,
    /// A request's operation ended and waits for a client retry.
    Lingering,
    Touched,
    Cancelled,
    Failed,
    TimedOut,
    /// The FIDO watcher started watching a device.
    DeviceAdded,
    /// The FIDO watcher stopped watching a device.
    DeviceRemoved,
    /// The daemon started.
    DaemonStarted,
    /// The daemon is stopping.
    DaemonStopping,
}

impl HookEvent {
    /// Returns the snake case name used in TOML and in `TOUCHCUE_EVENT`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Updated => "updated",
            Self::Ended => "ended",
            Self::Revived => "revived",
            Self::Waiting => "waiting",
            Self::Lingering => "lingering",
            Self::Touched => "touched",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::DeviceAdded => "device_added",
            Self::DeviceRemoved => "device_removed",
            Self::DaemonStarted => "daemon_started",
            Self::DaemonStopping => "daemon_stopping",
        }
    }
}

/// One `[[hooks]]` entry: a command run on events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// Events that run the command; never empty.
    pub on: Vec<HookEvent>,
    /// Placeholder values that must all be present and equal, compared
    /// exactly and case-sensitively; empty matches every event.
    pub matches: BTreeMap<String, String>,
    /// Program and arguments, run without a shell; never empty, and the
    /// program is never empty.
    pub command: Vec<String>,
    /// Time the command may run, 1 to 600000 ms.
    pub timeout_ms: u64,
    /// Most runs of the command at once, 1 to 32.
    pub concurrency: u8,
}

impl Hook {
    /// Returns whether `event` with placeholder `values` runs the command.
    #[must_use]
    pub fn applies(&self, event: HookEvent, values: &BTreeMap<String, String>) -> bool {
        self.on.contains(&event) && matches_all(&self.matches, values)
    }
}

/// The `[sources]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sources {
    pub fido: FidoSource,
    pub gpg: GpgSource,
}

/// The `[sources.fido]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FidoSource {
    pub enabled: bool,
    /// Milliseconds a request survives without a keepalive, 100 to 600000.
    pub keepalive_timeout_ms: u64,
    /// Milliseconds an ended operation waits for a client retry, 1 to 600000.
    pub retry_window_ms: u64,
}

impl Default for FidoSource {
    fn default() -> Self {
        Self {
            enabled: true,
            keepalive_timeout_ms: 1500,
            retry_window_ms: 1000,
        }
    }
}

/// The `[sources.gpg]` section.
///
/// Turns the helper socket on, through which `touchcue scdaemon` and
/// `touchcue askpass` report gpg and ssh operations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpgSource {
    pub enabled: bool,
}

impl Default for GpgSource {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The `[ipc]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ipc {
    pub enabled: bool,
}

impl Default for Ipc {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The `[dbus]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dbus {
    pub enabled: bool,
}

impl Default for Dbus {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The `[compat]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Compat {
    /// Socket protocol of maximbaz/yubikey-touch-detector.
    pub maxbaz_socket: Toggle,
}

/// The `[requester]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequesterConfig {
    /// Process names to skip in place of [`skip::DEFAULT_SKIP`], in its
    /// pattern form; `None` keeps the defaults.
    pub skip: Option<Vec<String>>,
    /// Process names to skip in addition to `skip` or the defaults.
    pub extend_skip: Vec<String>,
}

impl RequesterConfig {
    /// Returns the effective skip list.
    #[must_use]
    pub fn skip_list(&self) -> SkipList {
        SkipList::new(self.skip.as_deref(), &self.extend_skip)
    }
}

/// A feature switch that is off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Toggle {
    pub enabled: bool,
}

/// A configuration error, with the byte span in the TOML source when known.
///
/// As a [`miette::Diagnostic`] it carries a stable code, help where useful,
/// and a label at [`ConfigError::label_span`]. It holds no source text: a
/// caller renders the labels by attaching the TOML text it parsed, for
/// example with `miette::Report::with_source_code`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum ConfigError {
    #[error("{message}")]
    #[diagnostic(
        code(touchcue::config::toml),
        help("check the TOML syntax and the option names and types")
    )]
    Toml {
        message: String,
        #[label("here")]
        span: Option<SourceSpan>,
    },
    /// A template does not parse. `span` is the byte range of the TOML
    /// string value, quotes included, while `source.span` is the byte range
    /// within the template text. `label` is the offending bytes in the TOML
    /// source, or `span` when the TOML string has escapes or line breaks.
    #[error("invalid template in `{field}`")]
    #[diagnostic(
        code(touchcue::config::template),
        help(
            "placeholders are `{{key}}` or `{{a|b|\"literal\"}}`; write `{{{{` and `}}}}` for literal braces"
        )
    )]
    Template {
        field: String,
        span: Option<Range<usize>>,
        #[label("{}", source.kind)]
        label: Option<SourceSpan>,
        source: TemplateError,
    },
    #[error("unknown match key `{key}` in `{field}`")]
    #[diagnostic(
        code(touchcue::config::unknown_match_key),
        help("match keys are placeholder names, such as `app.name` or `request.method`")
    )]
    UnknownMatchKey { field: String, key: String },
    #[error("`rules[{index}].match` is empty or missing")]
    #[diagnostic(
        code(touchcue::config::empty_match),
        help("give the rule at least one `\"key\" = \"value\"` entry in `match`")
    )]
    EmptyMatch { index: usize },
    #[error("`{field}` must not be empty")]
    #[diagnostic(
        code(touchcue::config::empty_hook_field),
        help(
            "list at least one event in `on`, and the program and its arguments in `command`, such as `[\"pw-play\", \"/path/to/sound.oga\"]`"
        )
    )]
    EmptyHookField {
        field: String,
        #[label("empty")]
        span: Option<SourceSpan>,
    },
    #[error("`{field}` is {value}, outside {min}..={max}")]
    #[diagnostic(code(touchcue::config::out_of_range))]
    OutOfRange {
        field: String,
        value: u64,
        min: u64,
        max: u64,
    },
    #[error("`{field}` is not a process name pattern")]
    #[diagnostic(
        code(touchcue::config::invalid_pattern),
        help("write a process name, such as `mise`, or a prefix ending in `*`, such as `git-*`")
    )]
    InvalidPattern {
        field: String,
        #[label("here")]
        span: Option<SourceSpan>,
    },
}

impl ConfigError {
    /// Returns the byte range of the offending value in the TOML source.
    #[must_use]
    pub fn span(&self) -> Option<Range<usize>> {
        match self {
            Self::Toml { span, .. }
            | Self::EmptyHookField { span, .. }
            | Self::InvalidPattern { span, .. } => span.map(range),
            Self::Template { span, .. } => span.clone(),
            Self::UnknownMatchKey { .. } | Self::EmptyMatch { .. } | Self::OutOfRange { .. } => {
                None
            }
        }
    }

    /// Returns the byte range in the TOML source that the diagnostic label
    /// points at: the TOML error's span, the empty hook list, the invalid
    /// skip entry, or the offending template bytes.
    #[must_use]
    pub fn label_span(&self) -> Option<Range<usize>> {
        match self {
            Self::Toml { span, .. }
            | Self::EmptyHookField { span, .. }
            | Self::InvalidPattern { span, .. } => span.map(range),
            Self::Template { label, .. } => label.map(range),
            Self::UnknownMatchKey { .. } | Self::EmptyMatch { .. } | Self::OutOfRange { .. } => {
                None
            }
        }
    }
}

fn range(span: SourceSpan) -> Range<usize> {
    span.offset()..span.offset().saturating_add(span.len())
}

/// Prompt text produced by [`Config::rendered`]; the text is unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub title: String,
    pub body: String,
    pub icon: Option<String>,
}

/// A validated configuration with its templates parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub output: Output,
    pub popup: Popup,
    pub notification: Notification,
    pub sources: Sources,
    pub ipc: Ipc,
    pub dbus: Dbus,
    pub compat: Compat,
    pub requester: RequesterConfig,
    /// `[[hooks]]` entries in configuration order.
    pub hooks: Vec<Hook>,
    templates: Templates,
    title: Template,
    body: Template,
    rules: Vec<CompiledRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledRule {
    rule: Rule,
    title: Option<Template>,
    body: Option<Template>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    output: Output,
    popup: Popup,
    notification: Notification,
    templates: RawTemplates,
    rules: Vec<RawRule>,
    hooks: Vec<RawHook>,
    sources: Sources,
    ipc: Ipc,
    dbus: Dbus,
    compat: Compat,
    requester: RawRequester,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRequester {
    skip: Option<Vec<Spanned<String>>>,
    extend_skip: Vec<Spanned<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawTemplates {
    title: Option<Spanned<String>>,
    body: Option<Spanned<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawRule {
    #[serde(rename = "match")]
    matches: BTreeMap<String, String>,
    title: Option<Spanned<String>>,
    body: Option<Spanned<String>>,
    icon: Option<String>,
    suppress: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawHook {
    on: Option<Spanned<Vec<HookEvent>>>,
    #[serde(rename = "match")]
    matches: BTreeMap<String, String>,
    command: Option<Spanned<Vec<String>>>,
    timeout_ms: u64,
    concurrency: u64,
}

impl Default for RawHook {
    fn default() -> Self {
        Self {
            on: None,
            matches: BTreeMap::new(),
            command: None,
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
            concurrency: DEFAULT_HOOK_CONCURRENCY,
        }
    }
}

impl Default for Config {
    /// Returns the configuration of an empty TOML document.
    fn default() -> Self {
        Self {
            output: Output::default(),
            popup: Popup::default(),
            notification: Notification::default(),
            sources: Sources::default(),
            ipc: Ipc::default(),
            dbus: Dbus::default(),
            compat: Compat::default(),
            requester: RequesterConfig::default(),
            hooks: Vec::new(),
            templates: Templates::default(),
            title: template::default_title(),
            body: template::default_body(),
            rules: Vec::new(),
        }
    }
}

impl Config {
    /// Parses and validates a TOML configuration document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Toml`] for TOML syntax errors, type errors and
    /// unknown fields, [`ConfigError::Template`] for a template that does not
    /// parse, [`ConfigError::UnknownMatchKey`] for a rule matching a key
    /// or hook matching a key outside [`KNOWN`], [`ConfigError::EmptyMatch`]
    /// for a rule without match entries, [`ConfigError::EmptyHookField`] for
    /// a hook without events or program, [`ConfigError::OutOfRange`] for
    /// a number outside its documented range, and
    /// [`ConfigError::InvalidPattern`] for a malformed skip entry.
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(s).map_err(|e| ConfigError::Toml {
            message: e.message().to_owned(),
            span: e.span().map(SourceSpan::from),
        })?;

        let fido = &raw.sources.fido;
        check_range(
            "sources.fido.keepalive_timeout_ms",
            fido.keepalive_timeout_ms,
            MIN_KEEPALIVE_MS,
            MAX_MS,
        )?;
        check_range(
            "sources.fido.retry_window_ms",
            fido.retry_window_ms,
            1,
            MAX_MS,
        )?;
        check_range("popup.min_display_ms", raw.popup.min_display_ms, 0, MAX_MS)?;
        check_range("popup.show_delay_ms", raw.popup.show_delay_ms, 0, MAX_MS)?;
        check_range(
            "notification.safety_timeout_s",
            raw.notification.safety_timeout_s,
            0,
            MAX_MS / 1000,
        )?;
        let requester = compile_requester(raw.requester)?;

        let (title_src, title) = compile(s, raw.templates.title, DEFAULT_TITLE, "templates.title")?;
        let (body_src, body) = compile(s, raw.templates.body, DEFAULT_BODY, "templates.body")?;
        let rules = raw
            .rules
            .into_iter()
            .enumerate()
            .map(|(index, rule)| compile_rule(s, index, rule))
            .collect::<Result<Vec<_>, _>>()?;
        let hooks = raw
            .hooks
            .into_iter()
            .enumerate()
            .map(|(index, hook)| compile_hook(index, hook))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            output: raw.output,
            popup: raw.popup,
            notification: raw.notification,
            sources: raw.sources,
            ipc: raw.ipc,
            dbus: raw.dbus,
            compat: raw.compat,
            requester,
            hooks,
            templates: Templates {
                title: title_src,
                body: body_src,
            },
            title,
            body,
            rules,
        })
    }

    /// Returns the `[templates]` section.
    #[must_use]
    pub fn templates(&self) -> &Templates {
        &self.templates
    }

    /// Returns the rules in configuration order.
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().map(|compiled| &compiled.rule)
    }

    /// Renders the title, body and icon for a request's placeholder values.
    ///
    /// The first rule whose `match` entries all equal entries of `values`
    /// overrides the title, body and icon it sets. Matching is exact and
    /// case-sensitive, and a key absent from `values` does not match.
    /// Returns `None` when the matching rule suppresses output.
    #[must_use]
    pub fn rendered(&self, values: &BTreeMap<String, String>) -> Option<Rendered> {
        let matched = self
            .rules
            .iter()
            .find(|compiled| matches_all(&compiled.rule.matches, values));
        let Some(compiled) = matched else {
            return Some(Rendered {
                title: self.title.render(values),
                body: self.body.render(values),
                icon: None,
            });
        };
        if compiled.rule.suppress {
            return None;
        }
        Some(Rendered {
            title: compiled
                .title
                .as_ref()
                .unwrap_or(&self.title)
                .render(values),
            body: compiled.body.as_ref().unwrap_or(&self.body).render(values),
            icon: compiled.rule.icon.clone(),
        })
    }
}

/// Returns whether every entry of `matches` equals the entry of `values`
/// with the same key.
fn matches_all(matches: &BTreeMap<String, String>, values: &BTreeMap<String, String>) -> bool {
    matches
        .iter()
        .all(|(key, expected)| values.get(key) == Some(expected))
}

fn check_range(field: &str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::OutOfRange {
            field: field.to_owned(),
            value,
            min,
            max,
        })
    }
}

/// Parses the template `value` of the TOML document `toml`, or `default`
/// when it is absent.
fn compile(
    toml: &str,
    value: Option<Spanned<String>>,
    default: &str,
    field: &str,
) -> Result<(String, Template), ConfigError> {
    let (src, span) = match value {
        Some(spanned) => {
            let span = spanned.span();
            (spanned.into_inner(), Some(span))
        }
        None => (default.to_owned(), None),
    };
    let template = Template::parse(&src).map_err(|source| ConfigError::Template {
        field: field.to_owned(),
        label: span
            .as_ref()
            .map(|span| template_label(toml, span, &src, &source.span).into()),
        span,
        source,
    })?;
    Ok((src, template))
}

/// Returns the bytes of `toml` that hold `inner`, a range of the decoded
/// string `value` whose TOML value, quotes included, is at `span`.
///
/// When the text between the quotes differs from `value`, because of
/// escapes or a trimmed line break, returns `span`.
fn template_label(
    toml: &str,
    span: &Range<usize>,
    value: &str,
    inner: &Range<usize>,
) -> Range<usize> {
    let Some(raw) = toml.get(span.clone()) else {
        return span.clone();
    };
    let body = ["'''", "\"\"\"", "'", "\""].iter().find_map(|quote| {
        let body = raw.strip_prefix(quote)?.strip_suffix(quote)?;
        Some((quote.len(), body))
    });
    match body {
        Some((quote, body)) if body == value => {
            let start = span.start.saturating_add(quote);
            start.saturating_add(inner.start)..start.saturating_add(inner.end)
        }
        _ => span.clone(),
    }
}

fn compile_optional(
    toml: &str,
    value: Option<Spanned<String>>,
    field: &str,
) -> Result<(Option<String>, Option<Template>), ConfigError> {
    match value {
        Some(spanned) => {
            let (src, template) = compile(toml, Some(spanned), "", field)?;
            Ok((Some(src), Some(template)))
        }
        None => Ok((None, None)),
    }
}

/// Rejects a key of `matches` outside [`KNOWN`].
fn check_match_keys(field: String, matches: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    match matches.keys().find(|key| !KNOWN.contains(&key.as_str())) {
        Some(key) => Err(ConfigError::UnknownMatchKey {
            field,
            key: key.clone(),
        }),
        None => Ok(()),
    }
}

/// Returns the list in `value`, or [`ConfigError::EmptyHookField`] for
/// `field` when it is absent or empty.
fn non_empty<T>(field: String, value: Option<Spanned<Vec<T>>>) -> Result<Vec<T>, ConfigError> {
    match value {
        Some(spanned) if !spanned.get_ref().is_empty() => Ok(spanned.into_inner()),
        Some(spanned) => Err(ConfigError::EmptyHookField {
            field,
            span: Some(spanned.span().into()),
        }),
        None => Err(ConfigError::EmptyHookField { field, span: None }),
    }
}

/// Returns the `[requester]` section, or [`ConfigError::InvalidPattern`]
/// for the first entry that is not a skip pattern.
fn compile_requester(raw: RawRequester) -> Result<RequesterConfig, ConfigError> {
    let skip = raw
        .skip
        .map(|entries| patterns("requester.skip", entries))
        .transpose()?;
    let extend_skip = patterns("requester.extend_skip", raw.extend_skip)?;
    Ok(RequesterConfig { skip, extend_skip })
}

/// Returns the texts of `entries` of list `field`, each checked by [`skip::is_pattern`].
fn patterns(field: &str, entries: Vec<Spanned<String>>) -> Result<Vec<String>, ConfigError> {
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            if skip::is_pattern(entry.get_ref()) {
                Ok(entry.into_inner())
            } else {
                Err(ConfigError::InvalidPattern {
                    field: format!("{field}[{index}]"),
                    span: Some(entry.span().into()),
                })
            }
        })
        .collect()
}

fn compile_hook(index: usize, raw: RawHook) -> Result<Hook, ConfigError> {
    let on = non_empty(format!("hooks[{index}].on"), raw.on)?;
    let command_span = raw.command.as_ref().map(Spanned::span);
    let command = non_empty(format!("hooks[{index}].command"), raw.command)?;
    if command.first().is_none_or(String::is_empty) {
        return Err(ConfigError::EmptyHookField {
            field: format!("hooks[{index}].command[0]"),
            span: command_span.map(SourceSpan::from),
        });
    }
    check_match_keys(format!("hooks[{index}].match"), &raw.matches)?;
    check_range(
        &format!("hooks[{index}].timeout_ms"),
        raw.timeout_ms,
        1,
        MAX_MS,
    )?;
    let concurrency = match u8::try_from(raw.concurrency) {
        Ok(concurrency) if (1..=MAX_HOOK_CONCURRENCY).contains(&u64::from(concurrency)) => {
            concurrency
        }
        // A value beyond `u8` is out of range as well.
        _ => {
            return Err(ConfigError::OutOfRange {
                field: format!("hooks[{index}].concurrency"),
                value: raw.concurrency,
                min: 1,
                max: MAX_HOOK_CONCURRENCY,
            });
        }
    };
    Ok(Hook {
        on,
        matches: raw.matches,
        command,
        timeout_ms: raw.timeout_ms,
        concurrency,
    })
}

fn compile_rule(toml: &str, index: usize, raw: RawRule) -> Result<CompiledRule, ConfigError> {
    if raw.matches.is_empty() {
        return Err(ConfigError::EmptyMatch { index });
    }
    check_match_keys(format!("rules[{index}].match"), &raw.matches)?;
    let (title_src, title) = compile_optional(toml, raw.title, &format!("rules[{index}].title"))?;
    let (body_src, body) = compile_optional(toml, raw.body, &format!("rules[{index}].body"))?;
    Ok(CompiledRule {
        rule: Rule {
            matches: raw.matches,
            title: title_src,
            body: body_src,
            icon: raw.icon,
            suppress: raw.suppress,
        },
        title,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::TemplateErrorKind;
    use crate::test_error::{TestError, TestResult};

    /// Returns the error of a document that must be rejected.
    fn rejected(src: &str) -> Result<ConfigError, TestError> {
        let Err(error) = Config::from_toml(src) else {
            return Err("the document was accepted".into());
        };
        Ok(error)
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn default_equals_empty_document() -> TestResult {
        let parsed = Config::from_toml("")?;
        assert_eq!(parsed, Config::default());
        assert_eq!(parsed.output.mode, OutputMode::Popup);
        assert_eq!(parsed.output.fallback, OutputMode::Notification);
        assert_eq!(parsed.popup.position, Position::Center);
        assert_eq!(parsed.popup.output, OutputTarget::Focused);
        assert!(!parsed.popup.modal && parsed.popup.modal_dismiss);
        assert!((parsed.popup.modal_dim.get() - 0.4).abs() < f32::EPSILON);
        assert_eq!(parsed.popup.min_display_ms, 800);
        assert_eq!(parsed.notification.urgency, Urgency::Critical);
        assert_eq!(parsed.notification.safety_timeout_s, 60);
        assert!(parsed.sources.fido.enabled && parsed.sources.gpg.enabled);
        assert!(parsed.ipc.enabled && parsed.dbus.enabled);
        assert!(!parsed.compat.maxbaz_socket.enabled);
        assert_eq!(parsed.rules().count(), 0);
        assert_eq!(parsed.hooks, []);
        Ok(())
    }

    #[test]
    fn default_templates_render() -> TestResult {
        assert_eq!(Template::parse(DEFAULT_TITLE)?, template::default_title());
        assert_eq!(Template::parse(DEFAULT_BODY)?, template::default_body());
        let cfg = Config::default();
        let rendered = cfg
            .rendered(&values(&[("request.method", "fido2")]))
            .ok_or("suppressed")?;
        assert_eq!(rendered.title, "Touch your security key");
        assert_eq!(rendered.body, "An application is waiting for fido2");
        assert_eq!(rendered.icon, None);
        Ok(())
    }

    #[test]
    fn default_body_names_label_then_process() {
        let cfg = Config::default();
        let body = |pairs: &[(&str, &str)]| {
            let mut all = vec![("request.method", "openpgp")];
            all.extend_from_slice(pairs);
            cfg.rendered(&values(&all)).map(|rendered| rendered.body)
        };
        let claude = [
            ("requester.label", "claude in Kitty"),
            ("process.name", "gpg"),
        ];
        assert_eq!(
            body(&claude).as_deref(),
            Some("claude in Kitty is waiting for openpgp")
        );
        let firefox = [("requester.label", "Firefox"), ("process.name", "firefox")];
        assert_eq!(
            body(&firefox).as_deref(),
            Some("Firefox is waiting for openpgp")
        );
        let unnamed = [("process.name", "gpg")];
        assert_eq!(
            body(&unnamed).as_deref(),
            Some("gpg is waiting for openpgp")
        );
    }

    #[test]
    fn requester_skip_replaces_and_extends() -> TestResult {
        let defaults = Config::default().requester.skip_list();
        assert!(defaults.skips("bash") && !defaults.skips("nvim"));

        let extended = Config::from_toml("[requester]\nextend_skip = [\"nvim\", \"just-*\"]\n")?;
        assert_eq!(extended.requester.skip, None);
        let list = extended.requester.skip_list();
        assert!(list.skips("nvim") && list.skips("just-x") && list.skips("bash"));

        let replaced =
            Config::from_toml("[requester]\nskip = [\"make\"]\nextend_skip = [\"nvim\"]\n")?;
        let list = replaced.requester.skip_list();
        assert!(list.skips("make") && list.skips("nvim") && !list.skips("bash"));

        let empty = Config::from_toml("[requester]\nskip = []\n")?;
        assert_eq!(empty.requester.skip, Some(Vec::new()));
        assert!(!empty.requester.skip_list().skips("bash"));
        Ok(())
    }

    #[test]
    fn invalid_skip_entries_are_located() -> TestResult {
        for (src, field, span, text) in [
            (
                "[requester]\nskip = [\"ok\", \"a*b\"]\n",
                "requester.skip[1]",
                26..31,
                "\"a*b\"",
            ),
            (
                "[requester]\nextend_skip = [\"\"]\n",
                "requester.extend_skip[0]",
                27..29,
                "\"\"",
            ),
        ] {
            let error = rejected(src)?;
            assert_eq!(
                error,
                ConfigError::InvalidPattern {
                    field: field.to_owned(),
                    span: Some(span.into()),
                }
            );
            assert_eq!(error.label_span().and_then(|s| src.get(s)), Some(text));
        }
        assert!(matches!(
            rejected("[attribution]\nplumbing = []\n")?,
            ConfigError::Toml { .. }
        ));
        Ok(())
    }

    #[test]
    fn sections_parse() -> TestResult {
        let cfg = Config::from_toml(
            r#"
            [output]
            mode = "both"
            fallback = "none"
            [popup]
            position = "bottom-left"
            [notification]
            urgency = "low"
            [sources.fido]
            keepalive_timeout_ms = 2000
            [sources.gpg]
            enabled = false
            [compat.maxbaz_socket]
            enabled = true
            "#,
        )?;
        assert_eq!(cfg.output.mode, OutputMode::Both);
        assert_eq!(cfg.output.fallback, OutputMode::Off);
        assert_eq!(cfg.popup.position, Position::BottomLeft);
        assert_eq!(cfg.popup.min_display_ms, 800);
        assert_eq!(cfg.notification.urgency, Urgency::Low);
        assert_eq!(cfg.sources.fido.keepalive_timeout_ms, 2000);
        assert_eq!(cfg.sources.fido.retry_window_ms, 1000);
        assert!(!cfg.sources.gpg.enabled);
        assert!(cfg.compat.maxbaz_socket.enabled);
        Ok(())
    }

    #[test]
    fn unknown_field_is_rejected() -> TestResult {
        let src = "[popup]\nposition = \"top\"\ncolour = \"red\"\n";
        let err = rejected(src)?;
        assert!(matches!(err, ConfigError::Toml { .. }));
        assert_eq!(err.span().and_then(|s| src.get(s)), Some("colour"));
        assert_eq!(err.label_span().and_then(|s| src.get(s)), Some("colour"));
        assert!(matches!(
            Config::from_toml("verbose = true"),
            Err(ConfigError::Toml { .. })
        ));
        assert!(matches!(
            Config::from_toml("[[rules]]\nwhen = {}"),
            Err(ConfigError::Toml { .. })
        ));
        Ok(())
    }

    #[test]
    fn popup_placement_parses() -> TestResult {
        let popup = |src: &str| Config::from_toml(&format!("[popup]\n{src}")).map(|c| c.popup);
        assert_eq!(popup("position = \"center\"")?.position, Position::Center);
        assert_eq!(
            popup("position = \"top-right\"")?.position,
            Position::TopRight
        );
        assert_eq!(popup("output = \"all\"")?.output, OutputTarget::All);
        assert_eq!(popup("output = \"cursor\"")?.output, OutputTarget::Cursor);
        assert_eq!(popup("output = \"focused\"")?.output, OutputTarget::Focused);
        assert_eq!(
            popup("output = [\"DP-1\", \"HDMI-A-1\"]")?.output,
            OutputTarget::Named(vec!["DP-1".to_owned(), "HDMI-A-1".to_owned()])
        );
        let modal = popup("modal = true\nmodal_dim = 0\nmodal_dismiss = false")?;
        assert!(modal.modal && !modal.modal_dismiss);
        assert!(modal.modal_dim.get().abs() < f32::EPSILON);
        assert!((popup("modal_dim = 1.0")?.modal_dim.get() - 1.0).abs() < f32::EPSILON);
        Ok(())
    }

    #[test]
    fn invalid_popup_placement_is_rejected() -> TestResult {
        let cases = [
            ("output = []", "[]"),
            ("output = [\"DP-1\", \"\"]", "[\"DP-1\", \"\"]"),
            ("output = \"primary\"", "\"primary\""),
            ("output = 1", "1"),
            ("modal_dim = 1.5", "1.5"),
            ("modal_dim = -0.1", "-0.1"),
            ("modal_dim = nan", "nan"),
            ("position = \"middle\"", "\"middle\""),
        ];
        for (body, value) in cases {
            let src = format!("[popup]\n{body}\n");
            let err = rejected(&src)?;
            assert!(matches!(err, ConfigError::Toml { .. }), "{src:?}: {err:?}");
            assert_eq!(err.span().and_then(|s| src.get(s)), Some(value), "{src:?}");
        }
        Ok(())
    }

    #[test]
    fn bad_template_reports_field() -> TestResult {
        let src = "[templates]\nbody = \"{app.nope}\"\n";
        let err = rejected(src)?;
        let ConfigError::Template {
            field,
            span,
            source,
            ..
        } = &err
        else {
            return Err(format!("expected a template error, got {err:?}").into());
        };
        assert_eq!(field, "templates.body");
        assert_eq!(
            span.clone().and_then(|s| src.get(s)),
            Some("\"{app.nope}\"")
        );
        assert_eq!(
            source.kind,
            TemplateErrorKind::UnknownKey("app.nope".to_owned())
        );

        let src = "[[rules]]\nmatch = { \"app.id\" = \"a\" }\n\n[[rules]]\nmatch = { \"app.id\" = \"b\" }\ntitle = \"ab{\"\n";
        let err = rejected(src)?;
        let ConfigError::Template { span, source, .. } = &err else {
            return Err(format!("expected a template error, got {err:?}").into());
        };
        assert_eq!(span.clone().and_then(|s| src.get(s)), Some("\"ab{\""));
        assert_eq!(source.span, 2..3);
        assert!(matches!(
            err,
            ConfigError::Template { ref field, .. } if field == "rules[1].title"
        ));
        Ok(())
    }

    fn out_of_range(src: &str) -> Option<(String, u64)> {
        match Config::from_toml(src) {
            Err(ConfigError::OutOfRange { field, value, .. }) => Some((field, value)),
            _ => None,
        }
    }

    #[test]
    fn durations_are_range_checked() -> TestResult {
        let fido = "[sources.fido]\nkeepalive_timeout_ms = ";
        assert_eq!(
            out_of_range(&format!("{fido}0")),
            Some(("sources.fido.keepalive_timeout_ms".to_owned(), 0))
        );
        assert_eq!(
            out_of_range(&format!("{fido}99")),
            Some(("sources.fido.keepalive_timeout_ms".to_owned(), 99))
        );
        assert_eq!(out_of_range(&format!("{fido}100")), None);
        assert_eq!(
            out_of_range("[sources.fido]\nretry_window_ms = 0"),
            Some(("sources.fido.retry_window_ms".to_owned(), 0))
        );
        assert_eq!(
            out_of_range(&format!("{fido}{}", i64::MAX)),
            Some((
                "sources.fido.keepalive_timeout_ms".to_owned(),
                i64::MAX.unsigned_abs()
            ))
        );
        Config::from_toml(&format!("{fido}600000"))?;
        assert_eq!(
            out_of_range("[popup]\nmin_display_ms = 600001"),
            Some(("popup.min_display_ms".to_owned(), 600_001))
        );
        Config::from_toml("[popup]\nmin_display_ms = 0\nshow_delay_ms = 0")?;
        assert_eq!(
            out_of_range("[popup]\nshow_delay_ms = 9999999"),
            Some(("popup.show_delay_ms".to_owned(), 9_999_999))
        );
        Config::from_toml("[notification]\nsafety_timeout_s = 0")?;
        assert_eq!(
            out_of_range("[notification]\nsafety_timeout_s = 601"),
            Some(("notification.safety_timeout_s".to_owned(), 601))
        );
        Ok(())
    }

    #[test]
    fn extreme_durations_are_rejected() {
        assert_eq!(
            out_of_range(&format!("[popup]\nmin_display_ms = {}", u64::MAX)),
            Some(("popup.min_display_ms".to_owned(), u64::MAX))
        );
        let err = Config::from_toml("[popup]\nmin_display_ms = -1");
        assert!(matches!(err, Err(ConfigError::Toml { .. })));
    }

    #[test]
    fn empty_match_is_rejected() -> TestResult {
        assert_eq!(
            rejected("[[rules]]\nsuppress = true\n")?,
            ConfigError::EmptyMatch { index: 0 }
        );
        assert_eq!(
            rejected("[[rules]]\nmatch = { \"app.id\" = \"x\" }\n[[rules]]\nmatch = {}\n")?,
            ConfigError::EmptyMatch { index: 1 }
        );
        Ok(())
    }

    #[test]
    fn non_string_match_value_is_rejected() -> TestResult {
        let src = "[[rules]]\nmatch = { \"request.count\" = 2 }\n";
        let err = rejected(src)?;
        assert!(matches!(err, ConfigError::Toml { .. }));
        assert_eq!(err.span().and_then(|s| src.get(s)), Some("2"));
        Ok(())
    }

    #[test]
    fn unknown_match_key_is_rejected() -> TestResult {
        let err = rejected("[[rules]]\nmatch = { \"app.colour\" = \"x\" }\n")?;
        assert_eq!(
            err,
            ConfigError::UnknownMatchKey {
                field: "rules[0].match".to_owned(),
                key: "app.colour".to_owned(),
            }
        );
        Ok(())
    }

    #[test]
    fn template_labels_point_at_the_offending_bytes() -> TestResult {
        let cases = [
            ("[templates]\nbody = \"x {app.nope} y\"\n", "app.nope"),
            ("[templates]\ntitle = 'ab}'\n", "}"),
            (
                "[[rules]]\nmatch = { \"app.id\" = \"a\" }\nbody = '''{}'''\n",
                "{}",
            ),
            // An escape shifts the decoded text, so the whole value is labelled.
            ("[templates]\nbody = \"\\u0041{\"\n", "\"\\u0041{\""),
        ];
        for (src, expected) in cases {
            let err = rejected(src)?;
            assert_eq!(
                err.label_span().and_then(|s| src.get(s)),
                Some(expected),
                "{src:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn diagnostics_have_stable_codes() -> TestResult {
        use miette::Diagnostic as _;

        let cases = [
            ("verbose = true", "touchcue::config::toml"),
            ("[templates]\nbody = \"{\"", "touchcue::config::template"),
            (
                "[[rules]]\nmatch = { \"app.colour\" = \"x\" }\n",
                "touchcue::config::unknown_match_key",
            ),
            (
                "[[rules]]\nsuppress = true\n",
                "touchcue::config::empty_match",
            ),
            (
                "[popup]\nmin_display_ms = 600001",
                "touchcue::config::out_of_range",
            ),
            (
                "[[hooks]]\non = []\ncommand = [\"true\"]\n",
                "touchcue::config::empty_hook_field",
            ),
        ];
        for (src, code) in cases {
            let err = rejected(src)?;
            assert_eq!(
                err.code().map(|c| c.to_string()).as_deref(),
                Some(code),
                "{src:?}"
            );
        }
        let src = "[templates]\nbody = \"{app.nope}\"";
        let err = rejected(src)?;
        let labels: Vec<_> = err.labels().into_iter().flatten().collect();
        let [label] = labels.as_slice() else {
            return Err(format!("expected one label, got {labels:?}").into());
        };
        assert_eq!(
            src.get(label.offset()..label.offset() + label.len()),
            Some("app.nope")
        );
        assert_eq!(label.label(), Some("unknown placeholder key `app.nope`"));
        assert!(err.help().is_some());
        Ok(())
    }

    #[test]
    fn first_matching_rule_applies() -> TestResult {
        let cfg = Config::from_toml(
            r#"
            [[rules]]
            match = { "process.name" = "ssh", "request.method" = "fido2" }
            title = "SSH key"
            icon = "ssh-icon"

            [[rules]]
            match = { "process.name" = "ssh" }
            body = "ssh wants {request.method}"

            [[rules]]
            match = { "process.name" = "ssh" }
            suppress = true

            [[rules]]
            match = { "app.id" = "quiet" }
            suppress = true
            "#,
        )?;
        let both = cfg
            .rendered(&values(&[
                ("process.name", "ssh"),
                ("request.method", "fido2"),
            ]))
            .ok_or("suppressed")?;
        assert_eq!(
            both,
            Rendered {
                title: "SSH key".to_owned(),
                body: "ssh is waiting for fido2".to_owned(),
                icon: Some("ssh-icon".to_owned()),
            }
        );
        let second = cfg
            .rendered(&values(&[
                ("process.name", "ssh"),
                ("request.method", "u2f"),
            ]))
            .ok_or("suppressed")?;
        assert_eq!(
            second,
            Rendered {
                title: "Touch your security key".to_owned(),
                body: "ssh wants u2f".to_owned(),
                icon: None,
            }
        );
        assert_eq!(cfg.rendered(&values(&[("app.id", "quiet")])), None);
        assert!(cfg.rendered(&values(&[("app.id", "QUIET")])).is_some());
        assert!(cfg.rendered(&values(&[])).is_some());
        assert_eq!(cfg.rules().count(), 4);
        Ok(())
    }

    #[test]
    fn hooks_parse_with_defaults() -> TestResult {
        let cfg = Config::from_toml(
            r#"
            [[hooks]]
            on = ["started", "timed_out", "device_added", "daemon_stopping"]
            command = ["pw-play", "/sounds/touch.oga"]

            [[hooks]]
            on = ["touched"]
            match = { "app.id" = "org.mozilla.firefox" }
            command = ["notify-send", "Touched"]
            timeout_ms = 600000
            concurrency = 32
            "#,
        )?;
        let [first, second] = cfg.hooks.as_slice() else {
            return Err(format!("expected two hooks, got {:?}", cfg.hooks).into());
        };
        assert_eq!(
            first.on,
            [
                HookEvent::Started,
                HookEvent::TimedOut,
                HookEvent::DeviceAdded,
                HookEvent::DaemonStopping
            ]
        );
        assert!(first.matches.is_empty());
        assert_eq!(first.command, ["pw-play", "/sounds/touch.oga"]);
        assert_eq!((first.timeout_ms, first.concurrency), (5000, 4));
        assert_eq!(second.matches, values(&[("app.id", "org.mozilla.firefox")]));
        assert_eq!((second.timeout_ms, second.concurrency), (600_000, 32));
        Ok(())
    }

    #[test]
    fn hook_applies_to_its_events_and_matches() -> TestResult {
        let cfg = Config::from_toml(
            "[[hooks]]\non = [\"started\", \"ended\"]\nmatch = { \"process.name\" = \"ssh\" }\ncommand = [\"x\"]\n",
        )?;
        let hook = cfg.hooks.first().ok_or("no hook")?;
        let ssh = values(&[("process.name", "ssh"), ("app.name", "foot")]);
        assert!(hook.applies(HookEvent::Started, &ssh));
        assert!(hook.applies(HookEvent::Ended, &ssh));
        assert!(!hook.applies(HookEvent::Updated, &ssh));
        assert!(!hook.applies(HookEvent::Started, &values(&[("process.name", "SSH")])));
        assert!(!hook.applies(HookEvent::Started, &values(&[])));
        Ok(())
    }

    #[test]
    fn hook_event_names_round_trip() -> TestResult {
        let names = [
            "started",
            "updated",
            "ended",
            "revived",
            "waiting",
            "lingering",
            "touched",
            "cancelled",
            "failed",
            "timed_out",
            "device_added",
            "device_removed",
            "daemon_started",
            "daemon_stopping",
        ];
        let list = names.map(|name| format!("\"{name}\"")).join(", ");
        let cfg = Config::from_toml(&format!("[[hooks]]\non = [{list}]\ncommand = [\"x\"]\n"))?;
        let parsed: Vec<&str> = cfg
            .hooks
            .first()
            .ok_or("no hook")?
            .on
            .iter()
            .map(|event| event.as_str())
            .collect();
        assert_eq!(parsed, names);
        Ok(())
    }

    #[test]
    fn unknown_hook_event_is_rejected_with_a_span() -> TestResult {
        let src = "[[hooks]]\non = [\"started\", \"touchd\"]\ncommand = [\"x\"]\n";
        let err = rejected(src)?;
        assert!(matches!(err, ConfigError::Toml { .. }), "{err:?}");
        assert_eq!(
            err.label_span().and_then(|s| src.get(s)),
            Some("\"touchd\"")
        );
        Ok(())
    }

    #[test]
    fn invalid_hooks_are_rejected() -> TestResult {
        fn empty(src: &str) -> Result<(String, Option<String>), TestError> {
            match rejected(src)? {
                ConfigError::EmptyHookField { field, span } => Ok((
                    field,
                    span.map(range).and_then(|s| src.get(s)).map(str::to_owned),
                )),
                other => Err(format!("{src:?}: {other:?}").into()),
            }
        }
        let some = |s: &str| Some(s.to_owned());
        let src = "[[hooks]]\non = []\ncommand = [\"x\"]\n";
        assert_eq!(empty(src)?, ("hooks[0].on".to_owned(), some("[]")));
        assert_eq!(
            empty("[[hooks]]\ncommand = [\"x\"]\n")?,
            ("hooks[0].on".to_owned(), None)
        );
        let src = "[[hooks]]\non = [\"ended\"]\ncommand = []\n";
        assert_eq!(empty(src)?, ("hooks[0].command".to_owned(), some("[]")));
        assert_eq!(
            empty("[[hooks]]\non = [\"ended\"]\n")?,
            ("hooks[0].command".to_owned(), None)
        );
        let src = "[[hooks]]\non = [\"ended\"]\ncommand = [\"x\"]\n[[hooks]]\non = [\"ended\"]\ncommand = [\"\", \"a\"]\n";
        assert_eq!(
            empty(src)?,
            ("hooks[1].command[0]".to_owned(), some("[\"\", \"a\"]"))
        );

        assert_eq!(
            rejected(
                "[[hooks]]\non = [\"ended\"]\ncommand = [\"x\"]\nmatch = { \"app.colour\" = \"x\" }\n"
            )?,
            ConfigError::UnknownMatchKey {
                field: "hooks[0].match".to_owned(),
                key: "app.colour".to_owned(),
            }
        );
        let src = "[[hooks]]\non = [\"ended\"]\ncommand = \"x\"\n";
        assert!(matches!(rejected(src)?, ConfigError::Toml { .. }));
        let src = "[[hooks]]\non = [\"ended\"]\ncommand = [\"x\"]\nshell = true\n";
        assert!(matches!(rejected(src)?, ConfigError::Toml { .. }));
        Ok(())
    }

    #[test]
    fn hook_numbers_are_range_checked() -> TestResult {
        let hook = |key: &str| format!("[[hooks]]\non = [\"ended\"]\ncommand = [\"x\"]\n{key}\n");
        assert_eq!(
            out_of_range(&hook("timeout_ms = 0")),
            Some(("hooks[0].timeout_ms".to_owned(), 0))
        );
        assert_eq!(
            out_of_range(&hook("timeout_ms = 600001")),
            Some(("hooks[0].timeout_ms".to_owned(), 600_001))
        );
        assert_eq!(
            out_of_range(&hook("concurrency = 0")),
            Some(("hooks[0].concurrency".to_owned(), 0))
        );
        assert_eq!(
            out_of_range(&hook("concurrency = 33")),
            Some(("hooks[0].concurrency".to_owned(), 33))
        );
        assert_eq!(
            out_of_range(&hook("concurrency = 256")),
            Some(("hooks[0].concurrency".to_owned(), 256))
        );
        Config::from_toml(&hook("timeout_ms = 1\nconcurrency = 1"))?;
        Ok(())
    }
}
