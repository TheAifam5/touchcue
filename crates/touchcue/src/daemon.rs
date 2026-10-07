//! Turns detection signals into prompts and published events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, TryAcquireError};
use touchcue_appinfo::linux::Resolver;
use touchcue_core::placeholders::{self, AppInfo, Confidence, ProcessInfo, Requester};
use touchcue_core::text::sanitize;
use touchcue_core::{Config, DeviceKind, Event, Machine, RateLimit, Request, RequestId, Signal};
use touchcue_ipc::agent::AgentPaths;
use touchcue_ipc::helper::Notice;
use touchcue_ipc::sockdiag;
use touchcue_ui::{Command, Prompt};
use tracing::Instrument;

/// Longest prompt title, in chars.
const TITLE_MAX: usize = 120;
/// Longest prompt body, in chars.
const BODY_MAX: usize = 400;
/// Longest wait for attributing a request; a scan of about 1400 processes
/// takes 50 to 200 ms. A request not attributed in time has no holders.
pub const ATTRIBUTION_TIMEOUT: Duration = Duration::from_secs(1);
/// Shortest interval between two logged skipped attributions.
const BUSY_WARN_INTERVAL: Duration = Duration::from_secs(10);
/// Longest wait for finding the file of a rule icon. A lookup not done in
/// time yields the application's icon.
pub const ICON_TIMEOUT: Duration = Duration::from_millis(500);
/// Shortest interval between two logged unresolved rule icons.
const ICON_WARN_INTERVAL: Duration = Duration::from_secs(60);
/// Longest rule icon text recorded in logs, in chars.
const LOGGED_ICON_MAX: usize = 256;
/// Process names never attributed as a gpg-agent client: the agent, its
/// card daemon and touchcue's own `scdaemon` wrapper.
const AGENT_SIDE: &[&str] = &["gpg-agent", "scdaemon", "touchcue"];
/// Process names of tools that ask gpg-agent for card operations; ranked
/// before other clients, which may hold an idle connection.
///
/// Unlike the requester skip list, which names ancestors passed over, this
/// lists only programs that connect to gpg-agent's sockets. It is matched
/// against `comm`, which any process of the same user can set.
const AGENT_TOOLS: &[&str] = &[
    "gpg",
    "gpg2",
    "gpgsm",
    "ssh",
    "scp",
    "sftp",
    "ssh-add",
    "ssh-keygen",
];
/// Process name of maximbaz/yubikey-touch-detector, truncated to 15 bytes
/// as in `comm`; it proxies the agent sockets, so its clients are the
/// requesters and it is attributed only when no client is found.
const PROXY: &str = "yubikey-touch-d";
/// Time an askpass notice stays usable without its end.
const NOTICE_TTL: Duration = Duration::from_secs(30);
/// Most askpass notices kept; the oldest is dropped for a new one.
const MAX_NOTICES: usize = 16;

/// Destination of prompts and published events.
pub trait Sink {
    /// Queues a UI command and returns whether it was queued; a failure is
    /// logged by the sink.
    async fn ui(&mut self, command: Command) -> bool;
    /// Publishes `event` with placeholder `values` and the rendered
    /// `prompt`, which is `None` when a rule suppresses it or the request
    /// ended.
    fn publish(
        &mut self,
        event: &Event,
        values: &BTreeMap<String, String>,
        prompt: Option<&Prompt>,
    );
    /// Replaces the published set of active requests; each entry is a
    /// [`Event::Started`] with the values of its last published event.
    fn resync(&mut self, active: &[(Event, BTreeMap<String, String>)]);
}

/// Source of the client processes and application of a request, and of
/// the files of rule icons.
pub trait Attribute {
    async fn attribute(&mut self, request: &Request) -> Attribution;
    /// Returns the file of a rule's `icon`, an absolute path, a `~/` path or
    /// an icon name; `None` when none is found.
    async fn icon(&mut self, icon: &str) -> Option<String>;
}

/// Client processes of a request and what they resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// Every candidate client process; empty when none was found.
    pub pids: Vec<u32>,
    pub process: Option<ProcessInfo>,
    pub app: Option<AppInfo>,
    pub requester: Option<Requester>,
    /// Value of `process.chain`.
    pub chain: Option<String>,
    pub confidence: Confidence,
    /// For a device node, its holders and their ancestors, sorted; matched
    /// against askpass notices.
    pub lineage: Vec<u32>,
}

impl Default for Attribution {
    fn default() -> Self {
        Self {
            pids: Vec::new(),
            process: None,
            app: None,
            requester: None,
            chain: None,
            confidence: Confidence::Low,
            lineage: Vec::new(),
        }
    }
}

/// Where the clients of a request are looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Processes holding this device node open; high confidence for one
    /// holder, medium for several.
    Node(PathBuf),
    /// Processes connected to these gpg-agent sockets; medium confidence.
    Agent(Vec<PathBuf>),
    /// No way to find the clients.
    None,
}

/// Scans for the clients of a target and resolves the first of them.
type Scan = fn(&Resolver, &Target) -> Attribution;

/// Attributes requests to the processes holding their device node, scanning
/// procfs on the blocking pool, one scan at a time.
///
/// A scan that exceeds [`ATTRIBUTION_TIMEOUT`] is abandoned and keeps its
/// blocking thread until it finishes; until then, further requests are not
/// attributed and have no holders. Rule icons are looked up the same way,
/// one at a time within [`ICON_TIMEOUT`].
pub struct SystemAttribution {
    resolver: Arc<Resolver>,
    /// gpg-agent's sockets, for `OpenPGP` requests.
    agent: Option<AgentPaths>,
    scan: Scan,
    timeout: Duration,
    /// Held by the one running scan.
    busy: Arc<Semaphore>,
    busy_limit: RateLimit,
    /// Held by the one running rule icon lookup.
    icon_busy: Arc<Semaphore>,
    icon_limit: RateLimit,
}

impl SystemAttribution {
    /// Creates the attribution; without `agent`, `OpenPGP` requests are not attributed.
    pub fn new(resolver: Resolver, agent: Option<AgentPaths>) -> Self {
        Self::with_scan(resolver, agent, attribute, ATTRIBUTION_TIMEOUT)
    }

    fn with_scan(
        resolver: Resolver,
        agent: Option<AgentPaths>,
        scan: Scan,
        timeout: Duration,
    ) -> Self {
        Self {
            resolver: Arc::new(resolver),
            agent,
            scan,
            timeout,
            busy: Arc::new(Semaphore::new(1)),
            busy_limit: RateLimit::new(BUSY_WARN_INTERVAL),
            icon_busy: Arc::new(Semaphore::new(1)),
            icon_limit: RateLimit::new(ICON_WARN_INTERVAL),
        }
    }

    /// Returns where the clients of `request` are looked for: the agent
    /// sockets for an `OpenPGP` card, else the device node.
    fn target(&self, request: &Request) -> Target {
        if request.device.kind == DeviceKind::OpenPgp {
            return self.agent.as_ref().map_or(Target::None, |agent| {
                Target::Agent(vec![agent.agent.clone(), agent.ssh.clone()])
            });
        }
        let node = PathBuf::from(&request.device.id.0);
        if node.is_absolute() {
            Target::Node(node)
        } else {
            Target::None
        }
    }

    /// Logs a skipped attribution at most once per [`BUSY_WARN_INTERVAL`].
    fn warn_busy(&mut self) {
        let now = tokio::time::Instant::now().into_std();
        self.busy_limit.log(now, |suppressed| {
            tracing::warn!(suppressed, "attribution busy; skipping it");
        });
    }

    /// Logs a rule icon without a file at most once per [`ICON_WARN_INTERVAL`],
    /// naming an icon name but recording a path at debug level only.
    fn warn_icon(&mut self, icon: &str, reason: &'static str) {
        let path = icon.starts_with('/') || icon.starts_with("~/");
        if path {
            tracing::debug!(
                path = sanitize(icon, LOGGED_ICON_MAX).as_deref(),
                reason,
                "rule icon file not usable"
            );
        }
        let now = tokio::time::Instant::now().into_std();
        self.icon_limit.log(now, |suppressed| {
            if path {
                tracing::warn!(
                    reason,
                    suppressed,
                    "rule icon file not usable; using the application icon"
                );
            } else {
                tracing::warn!(
                    icon = sanitize(icon, LOGGED_ICON_MAX).as_deref(),
                    reason,
                    suppressed,
                    "rule icon not found; using the application icon"
                );
            }
        });
    }
}

impl Attribute for SystemAttribution {
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(id = %request.id, device = %request.device.id.0)
    )]
    async fn attribute(&mut self, request: &Request) -> Attribution {
        let permit = match Arc::clone(&self.busy).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                self.warn_busy();
                return Attribution::default();
            }
            Err(error @ TryAcquireError::Closed) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "attribution permits closed"
                );
                return Attribution::default();
            }
        };
        let resolver = Arc::clone(&self.resolver);
        let attribute = self.scan;
        let target = self.target(request);
        let span = tracing::Span::current();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            span.in_scope(|| attribute(&resolver, &target))
        });
        match tokio::time::timeout(self.timeout, task).await {
            Ok(Ok(attribution)) => attribution,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "attribution failed"
                );
                Attribution::default()
            }
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    timeout_ms = millis(self.timeout),
                    "attribution timed out"
                );
                Attribution::default()
            }
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn icon(&mut self, icon: &str) -> Option<String> {
        let permit = match Arc::clone(&self.icon_busy).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                self.warn_icon(icon, "busy");
                return None;
            }
            Err(error @ TryAcquireError::Closed) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "icon lookup permits closed"
                );
                return None;
            }
        };
        let resolver = Arc::clone(&self.resolver);
        let owned = icon.to_owned();
        let span = tracing::Span::current();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            span.in_scope(|| resolver.icons().resolve_config(&owned))
        });
        let reason = match tokio::time::timeout(ICON_TIMEOUT, task).await {
            Ok(Ok(Some(file))) => return Some(file),
            Ok(Ok(None)) => "not found",
            Ok(Err(error)) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "icon lookup failed"
                );
                "failed"
            }
            Err(error) => {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    timeout_ms = millis(ICON_TIMEOUT),
                    "icon lookup timed out"
                );
                "timed out"
            }
        };
        self.warn_icon(icon, reason);
        None
    }
}

/// Attributes a request to the clients of `target`; the first client
/// supplies the process, application, requester and chain.
fn attribute(resolver: &Resolver, target: &Target) -> Attribution {
    let started = Instant::now();
    let (clients, confidence) = match target {
        Target::Node(node) => {
            let holders = resolver.holders(node);
            // Several holders leave the client choice ambiguous.
            let confidence = if holders.len() > 1 {
                Confidence::Medium
            } else {
                Confidence::High
            };
            (holders, confidence)
        }
        Target::Agent(sockets) => agent_clients(resolver, sockets),
        Target::None => (Vec::new(), Confidence::Low),
    };
    let Some(&(pid, start)) = clients.first() else {
        tracing::debug!(elapsed_ms = millis(started.elapsed()), "no client found");
        return Attribution::default();
    };
    let mut lineage: Vec<u32> = match target {
        Target::Node(_) => clients
            .iter()
            .flat_map(|&(pid, _)| resolver.lineage(pid))
            .collect(),
        Target::Agent(_) | Target::None => Vec::new(),
    };
    lineage.sort_unstable();
    lineage.dedup();
    let origin = resolver.origin(pid, start);
    let (app, requester, chain) = match origin {
        Some(origin) => (origin.app, origin.requester, origin.chain),
        None => (None, None, None),
    };
    let attribution = Attribution {
        pids: clients.iter().map(|&(pid, _)| pid).collect(),
        process: resolver.process(pid, start),
        app,
        requester,
        chain,
        confidence,
        lineage,
    };
    let app = attribution.app.as_ref();
    tracing::debug!(
        pid,
        clients = attribution.pids.len(),
        confidence = confidence.as_str(),
        process = attribution.process.as_ref().and_then(|p| p.name.as_deref()),
        requester = attribution
            .requester
            .as_ref()
            .and_then(|r| r.name.as_deref()),
        app_id = app.and_then(|a| a.id.as_deref()),
        app_name = app.and_then(|a| a.name.as_deref()),
        elapsed_ms = millis(started.elapsed()),
        "attributed request"
    );
    attribution
}

/// Returns `duration` in whole milliseconds, saturating at `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(duration.subsec_millis()))
}

/// Returns the processes connected to `sockets`, without gpg-agent,
/// scdaemon and touchcue, ranked by [`rank_clients`].
///
/// Clients of the proxy of maximbaz/yubikey-touch-detector are connected to
/// sockets bound at the same paths, so they are found directly. The proxy
/// itself is returned, at low confidence, only when no other client is.
fn agent_clients(resolver: &Resolver, sockets: &[PathBuf]) -> (Vec<(u32, u64)>, Confidence) {
    let paths: Vec<&Path> = sockets.iter().map(PathBuf::as_path).collect();
    let connections = match sockdiag::connections(&paths) {
        Ok(connections) => connections,
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "cannot list gpg-agent clients"
            );
            return (Vec::new(), Confidence::Low);
        }
    };
    let peers: Vec<u32> = connections.iter().map(|c| c.peer).collect();
    let mut clients = Vec::new();
    let mut proxies = Vec::new();
    for (pid, start) in resolver.socket_holders(&peers) {
        let name = resolver
            .process(pid, start)
            .and_then(|process| process.name);
        match name.as_deref() {
            Some(name) if AGENT_SIDE.contains(&name) => {}
            Some(PROXY) => proxies.push((pid, start, false)),
            name => clients.push((pid, start, name.is_some_and(|n| AGENT_TOOLS.contains(&n)))),
        }
    }
    tracing::debug!(
        connections = connections.len(),
        clients = clients.len(),
        proxies = proxies.len(),
        "gpg-agent clients"
    );
    if clients.is_empty() {
        (rank_clients(proxies), Confidence::Low)
    } else {
        (rank_clients(clients), Confidence::Medium)
    }
}

/// Returns the pids and start times of `clients`, each with whether it is
/// one of [`AGENT_TOOLS`]: tools first, then newest first.
///
/// The requester is most likely the client started last, but a program such
/// as an editor can hold an idle connection open and start after the tool
/// that asks for the operation.
fn rank_clients(mut clients: Vec<(u32, u64, bool)>) -> Vec<(u32, u64)> {
    clients.sort_by_key(|&(_, start, tool)| (std::cmp::Reverse(tool), std::cmp::Reverse(start)));
    clients
        .into_iter()
        .map(|(pid, start, _)| (pid, start))
        .collect()
}

#[derive(Debug)]
struct Cached {
    attribution: Attribution,
    /// `Request::count` at the last attribution.
    count: u32,
    /// Whether the UI holds a prompt for the request: set only once a show
    /// was queued, so a show lost to a full queue is retried on the next update.
    shown: bool,
    /// Request and values of the last published event.
    published: Option<(Request, BTreeMap<String, String>)>,
}

/// Drives the request state machine and forwards every change to a [`Sink`].
///
/// A request is attributed when it starts and again when a client retry
/// revives it. A prompt is shown while the configuration renders the request
/// and hidden when a rule suppresses it or the request ends. Every event is
/// published, suppressed or not. Events are handled one at a time, so a slow
/// attribution delays later events by up to [`ATTRIBUTION_TIMEOUT`].
///
/// An askpass notice only enriches requests: a request whose attributed
/// lineage contains the notice's pid gets the notice's detail, whether the
/// notice arrives before or after the request is attributed.
pub struct Daemon<A, S> {
    machine: Machine,
    config: Config,
    attribute: A,
    sink: S,
    cache: BTreeMap<RequestId, Cached>,
    /// Askpass notices by pid: detail and arrival time.
    notices: BTreeMap<u32, (String, Instant)>,
}

impl<A: Attribute, S: Sink> Daemon<A, S> {
    pub fn new(machine: Machine, config: Config, attribute: A, sink: S) -> Self {
        Self {
            machine,
            config,
            attribute,
            sink,
            cache: BTreeMap::new(),
            notices: BTreeMap::new(),
        }
    }

    /// Applies an askpass notice received at `now`.
    pub async fn notice(&mut self, notice: Notice, now: Instant) {
        match notice {
            Notice::Started { pid, detail } => {
                self.notices
                    .retain(|_, (_, at)| now.saturating_duration_since(*at) < NOTICE_TTL);
                if self.notices.len() >= MAX_NOTICES && !self.notices.contains_key(&pid) {
                    let oldest = self
                        .notices
                        .iter()
                        .min_by_key(|(_, (_, at))| *at)
                        .map(|(p, _)| *p);
                    if let Some(oldest) = oldest {
                        tracing::debug!(pid = oldest, "askpass notice dropped for a newer one");
                        self.notices.remove(&oldest);
                    }
                }
                self.notices.insert(pid, (detail, now));
                let ids: Vec<RequestId> = self
                    .cache
                    .iter()
                    .filter(|(_, cached)| cached.attribution.lineage.contains(&pid))
                    .map(|(id, _)| *id)
                    .collect();
                for id in ids {
                    let span = tracing::debug_span!("request", id = %id);
                    async {
                        tracing::debug!(pid, "askpass notice matches request");
                        if let Some(event) = self.apply_detail(id, &[pid], now) {
                            self.present(&event, now).await;
                        }
                    }
                    .instrument(span)
                    .await;
                }
            }
            Notice::Ended { pid } => {
                self.notices.remove(&pid);
            }
        }
    }

    /// Sets the detail of the notice matching `lineage` on request `id` and
    /// returns the update, if any.
    fn apply_detail(&mut self, id: RequestId, lineage: &[u32], now: Instant) -> Option<Event> {
        let detail = lineage.iter().find_map(|pid| {
            let (detail, at) = self.notices.get(pid)?;
            (now.saturating_duration_since(*at) < NOTICE_TTL).then(|| detail.clone())
        })?;
        self.machine.set_detail(id, Some(detail))
    }

    /// Returns `request` with the detail of a matching askpass notice, if any.
    fn with_detail(
        &mut self,
        request: Request,
        attribution: &Attribution,
        now: Instant,
    ) -> Request {
        match self.apply_detail(request.id, &attribution.lineage, now) {
            Some(Event::Updated(updated)) => updated,
            _ => request,
        }
    }

    /// Applies a signal observed at `now`.
    pub async fn signal(&mut self, signal: Signal, now: Instant) {
        let events = self.machine.handle(signal, now);
        self.process(events, now).await;
    }

    /// Applies the deadlines that passed at `now`.
    pub async fn tick(&mut self, now: Instant) {
        let events = self.machine.tick(now);
        self.process(events, now).await;
    }

    /// Returns the earliest instant at which [`Self::tick`] has work.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.machine.next_deadline()
    }

    /// Sends the sink every active request with the values of its last
    /// published event, so that an unchanged request compares equal.
    pub fn resync(&mut self) {
        let active: Vec<(Event, BTreeMap<String, String>)> = self
            .machine
            .active()
            .filter_map(|request| self.cache.get(&request.id)?.published.clone())
            .map(|(request, values)| (Event::Started(request), values))
            .collect();
        tracing::debug!(active = active.len(), "resync");
        self.sink.resync(&active);
    }

    /// Returns the sink, ending the daemon.
    pub fn into_sink(self) -> S {
        self.sink
    }

    async fn process(&mut self, events: Vec<Event>, now: Instant) {
        for event in events {
            let (Event::Started(request) | Event::Updated(request) | Event::Ended { request, .. }) =
                &event;
            let span = tracing::debug_span!("request", id = %request.id);
            self.handle(event, now).instrument(span).await;
        }
    }

    async fn handle(&mut self, event: Event, now: Instant) {
        match event {
            Event::Started(request) => {
                let attribution = self.attribute.attribute(&request).await;
                let request = self.assign(request, &attribution);
                let request = self.with_detail(request, &attribution, now);
                self.cache.insert(
                    request.id,
                    Cached {
                        attribution,
                        count: request.count,
                        shown: false,
                        published: None,
                    },
                );
                self.present(&Event::Started(request), now).await;
            }
            Event::Updated(request) => {
                let stale = self
                    .cache
                    .get(&request.id)
                    .is_none_or(|cached| request.count > cached.count);
                let request = if stale {
                    let attribution = self.attribute.attribute(&request).await;
                    let request = self.assign(request, &attribution);
                    let request = self.with_detail(request, &attribution, now);
                    let cached = self.cache.entry(request.id).or_insert(Cached {
                        attribution: Attribution::default(),
                        count: 0,
                        shown: false,
                        published: None,
                    });
                    cached.attribution = attribution;
                    cached.count = request.count;
                    request
                } else {
                    request
                };
                self.present(&Event::Updated(request), now).await;
            }
            Event::Ended { request, reason } => {
                let cached = self.cache.remove(&request.id);
                let attribution = cached.as_ref().map(|c| &c.attribution);
                let values = values(&request, attribution, now);
                if cached.is_some_and(|c| c.shown) {
                    self.sink.ui(Command::Hide(request.id)).await;
                }
                self.sink
                    .publish(&Event::Ended { request, reason }, &values, None);
            }
        }
    }

    /// Replaces the request's pids with the attributed holders, when any were
    /// found, and returns the request carrying them.
    fn assign(&mut self, request: Request, attribution: &Attribution) -> Request {
        if attribution.pids.is_empty() {
            return request;
        }
        match self.machine.set_pids(request.id, attribution.pids.clone()) {
            Some(Event::Updated(updated)) => updated,
            _ => request,
        }
    }

    /// Shows, updates or hides the prompt of a started or updated request and publishes the event.
    async fn present(&mut self, event: &Event, now: Instant) {
        let (Event::Started(request) | Event::Updated(request)) = event else {
            return;
        };
        let Some(cached) = self.cache.get_mut(&request.id) else {
            return;
        };
        let (values, prompt) = render(
            &self.config,
            request,
            &cached.attribution,
            now,
            &mut self.attribute,
        )
        .await;
        let published = prompt.clone();
        match (prompt, cached.shown) {
            (Some(prompt), false) => {
                cached.shown = self.sink.ui(Command::Show(prompt)).await;
            }
            (Some(prompt), true) => {
                self.sink.ui(Command::Update(prompt)).await;
            }
            (None, true) => {
                cached.shown = !self.sink.ui(Command::Hide(request.id)).await;
            }
            (None, false) => {}
        }
        self.sink.publish(event, &values, published.as_ref());
        cached.published = Some((request.clone(), values));
    }
}

fn values(
    request: &Request,
    attribution: Option<&Attribution>,
    now: Instant,
) -> BTreeMap<String, String> {
    placeholders::values(
        request,
        attribution.and_then(|a| a.app.as_ref()),
        attribution.and_then(|a| a.process.as_ref()),
        attribution.and_then(|a| a.requester.as_ref()),
        attribution.and_then(|a| a.chain.as_deref()),
        attribution.map_or(Confidence::Low, |a| a.confidence),
        now,
    )
}

/// Returns the placeholder values of `request` and its prompt, or `None` for
/// the prompt when a rule suppresses it.
///
/// Title and body are sanitized and capped at 120 and 400 chars. The icon is
/// the file `icons` finds for the matching rule's icon, else the
/// application's icon.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(id = %request.id, state = request.state.as_str())
)]
pub async fn render(
    config: &Config,
    request: &Request,
    attribution: &Attribution,
    now: Instant,
    icons: &mut impl Attribute,
) -> (BTreeMap<String, String>, Option<Prompt>) {
    let values = values(request, Some(attribution), now);
    let Some(rendered) = config.rendered(&values) else {
        tracing::debug!(suppressed = true, "rendered prompt");
        return (values, None);
    };
    let rule_icon = match rendered.icon.as_deref() {
        Some(icon) => icons.icon(icon).await,
        None => None,
    };
    let prompt = Prompt {
        id: request.id,
        title: sanitize(&rendered.title, TITLE_MAX).unwrap_or_default(),
        body: sanitize(&rendered.body, BODY_MAX).unwrap_or_default(),
        icon: rule_icon
            .or_else(|| attribution.app.as_ref().and_then(|app| app.icon.clone()))
            .map(PathBuf::from),
        state: request.state,
    };
    tracing::debug!(suppressed = false, "rendered prompt");
    (values, Some(prompt))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use touchcue_core::{
        Device, DeviceId, DeviceKind, MachineConfig, Method, Outcome, RequestState, SignalClass,
        SignalKind, Source, Transport,
    };

    use touchcue_appinfo::linux::icon::IconLookup;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error(transparent)]
        Config(#[from] touchcue_core::ConfigError),
        #[error("expected a prompt")]
        NoPrompt,
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Ipc(#[from] touchcue_ipc::IpcError),
        #[error(transparent)]
        Join(#[from] tokio::task::JoinError),
        #[error("the daemon did not reach the expected output in time")]
        TimedOut,
        #[error(transparent)]
        Elapsed(#[from] tokio::time::error::Elapsed),
    }

    type TestResult = Result<(), TestError>;

    const MS: Duration = Duration::from_millis(1);

    fn signal(channel: u32, kind: SignalKind) -> Signal {
        Signal {
            device: Device {
                id: DeviceId("/dev/hidraw3".to_owned()),
                kind: DeviceKind::Fido,
                transport: Transport::Usb,
                vid: Some(0x1050),
                pid: Some(0x0407),
                vendor: Some("Yubico".to_owned()),
                model: None,
                product: None,
            },
            source: Source::Fido,
            class: SignalClass::Asserted,
            kind,
            channel: Some(channel),
            pids: Vec::new(),
        }
    }

    fn pending(channel: u32) -> Signal {
        signal(
            channel,
            SignalKind::Pending {
                method: Method::Fido2,
                op: None,
            },
        )
    }

    fn request(state: RequestState) -> Request {
        let s = pending(1);
        Request {
            id: RequestId(1),
            device: s.device,
            source: s.source,
            class: s.class,
            method: Method::Fido2,
            op: None,
            channel: Some(1),
            pids: Vec::new(),
            started: Instant::now(),
            count: 1,
            detail: None,
            state,
        }
    }

    fn firefox() -> Attribution {
        Attribution {
            pids: vec![42, 43],
            process: Some(ProcessInfo {
                name: Some("firefox".to_owned()),
                exe: None,
                pid: 42,
                cmdline: None,
                uid: Some(1000),
            }),
            app: Some(AppInfo {
                name: Some("Firefox\u{202E}\n".to_owned()),
                icon: Some("/icons/firefox.png".to_owned()),
                ..AppInfo::default()
            }),
            requester: None,
            chain: Some("firefox".to_owned()),
            confidence: Confidence::High,
            lineage: vec![42, 43, 77],
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Out {
        Show(RequestId, String),
        Update(RequestId, RequestState),
        Hide(RequestId),
        Publish(&'static str, RequestId, Option<String>),
        Resync(Vec<(RequestId, BTreeMap<String, String>)>),
    }

    /// Records sink calls; `values` holds the values of every published event
    /// in order. The first `reject_shows` shows are not recorded and report
    /// a full queue.
    #[derive(Default)]
    struct Recorder {
        out: Vec<Out>,
        values: Vec<BTreeMap<String, String>>,
        /// Body of the prompt of every published event, in order.
        prompts: Vec<Option<String>>,
        reject_shows: u32,
    }

    impl Sink for Recorder {
        fn ui(&mut self, command: Command) -> impl Future<Output = bool> {
            if matches!(command, Command::Show(_)) && self.reject_shows > 0 {
                self.reject_shows -= 1;
                return std::future::ready(false);
            }
            self.out.push(match command {
                Command::Show(p) => Out::Show(p.id, p.body),
                Command::Update(p) => Out::Update(p.id, p.state),
                Command::Hide(id) => Out::Hide(id),
            });
            std::future::ready(true)
        }

        fn publish(
            &mut self,
            event: &Event,
            values: &BTreeMap<String, String>,
            prompt: Option<&Prompt>,
        ) {
            self.prompts.push(prompt.map(|p| p.body.clone()));
            let (kind, request) = match event {
                Event::Started(r) => ("started", r),
                Event::Updated(r) => ("updated", r),
                Event::Ended { request, .. } => ("ended", request),
            };
            let confidence = values.get("request.confidence").cloned();
            self.out.push(Out::Publish(kind, request.id, confidence));
            self.values.push(values.clone());
        }

        fn resync(&mut self, active: &[(Event, BTreeMap<String, String>)]) {
            let ids = active
                .iter()
                .filter_map(|(event, values)| match event {
                    Event::Started(r) => Some((r.id, values.clone())),
                    Event::Updated(_) | Event::Ended { .. } => None,
                })
                .collect();
            self.out.push(Out::Resync(ids));
        }
    }

    /// Returns `firefox()` and counts the calls.
    #[derive(Default)]
    struct Fixed(u32);

    impl Attribute for Fixed {
        fn attribute(&mut self, _: &Request) -> impl Future<Output = Attribution> {
            self.0 = self.0.saturating_add(1);
            std::future::ready(firefox())
        }

        /// Finds only the icon named `fixture-icon`.
        fn icon(&mut self, icon: &str) -> impl Future<Output = Option<String>> {
            std::future::ready((icon == "fixture-icon").then(|| "/icons/fixture.svg".to_owned()))
        }
    }

    fn daemon(toml: &str) -> Result<Daemon<Fixed, Recorder>, TestError> {
        Ok(Daemon::new(
            Machine::new(MachineConfig::default()),
            Config::from_toml(toml)?,
            Fixed::default(),
            Recorder::default(),
        ))
    }

    #[tokio::test]
    async fn render_sanitizes_and_prefers_app_icon() -> TestResult {
        let (values, prompt) = render(
            &Config::default(),
            &request(RequestState::Waiting),
            &firefox(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            values.get("request.confidence").map(String::as_str),
            Some("high")
        );
        let prompt = prompt.ok_or(TestError::NoPrompt)?;
        assert_eq!(prompt.title, "Touch Yubico");
        assert_eq!(prompt.body, "Firefox is waiting for fido2");
        assert_eq!(prompt.icon, Some(PathBuf::from("/icons/firefox.png")));
        Ok(())
    }

    #[tokio::test]
    async fn render_caps_title_and_body() -> TestResult {
        let long = "x".repeat(500);
        let config = Config::from_toml(&format!(
            "[templates]\ntitle = \"{long}\"\nbody = \"{long}\"\n"
        ))?;
        let (_, prompt) = render(
            &config,
            &request(RequestState::Waiting),
            &firefox(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        let prompt = prompt.ok_or(TestError::NoPrompt)?;
        assert_eq!(prompt.title.chars().count(), TITLE_MAX);
        assert_eq!(prompt.body.chars().count(), BODY_MAX);
        Ok(())
    }

    #[tokio::test]
    async fn rule_icon_precedes_app_icon_and_suppress_hides() -> TestResult {
        let config = Config::from_toml(
            "[[rules]]\nmatch = { \"request.state\" = \"waiting\" }\nicon = \"fixture-icon\"\n\
             [[rules]]\nmatch = { \"request.state\" = \"touched\" }\nicon = \"missing\"\n\
             [[rules]]\nmatch = { \"request.state\" = \"cancelled\" }\nsuppress = true\n",
        )?;
        let (_, waiting) = render(
            &config,
            &request(RequestState::Waiting),
            &firefox(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            waiting.ok_or(TestError::NoPrompt)?.icon,
            Some(PathBuf::from("/icons/fixture.svg"))
        );
        let (_, touched) = render(
            &config,
            &request(RequestState::Lingering(touchcue_core::EndReason::Touched)),
            &firefox(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            touched.ok_or(TestError::NoPrompt)?.icon,
            Some(PathBuf::from("/icons/firefox.png"))
        );
        let cancelled = RequestState::Lingering(touchcue_core::EndReason::Cancelled);
        let (values, suppressed) = render(
            &config,
            &request(cancelled),
            &firefox(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(suppressed, None);
        assert!(values.contains_key("app.name"));
        Ok(())
    }

    #[tokio::test]
    async fn render_without_holder_has_low_confidence() -> TestResult {
        let (values, prompt) = render(
            &Config::default(),
            &request(RequestState::Waiting),
            &Attribution::default(),
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            values.get("request.confidence").map(String::as_str),
            Some("low")
        );
        let prompt = prompt.ok_or(TestError::NoPrompt)?;
        assert_eq!(prompt.body, "An application is waiting for fido2");
        assert_eq!(prompt.icon, None);
        Ok(())
    }

    #[tokio::test]
    async fn render_names_the_requester_in_the_app() -> TestResult {
        let attribution = Attribution {
            requester: Some(Requester {
                name: Some("claude".to_owned()),
                exe: None,
                pid: 50,
            }),
            chain: Some("gpg ← git ← bash ← claude ← nu ← kitty".to_owned()),
            ..firefox()
        };
        let (values, prompt) = render(
            &Config::default(),
            &request(RequestState::Waiting),
            &attribution,
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            prompt.ok_or(TestError::NoPrompt)?.body,
            "claude in Firefox is waiting for fido2"
        );
        assert_eq!(
            values.get("process.chain").map(String::as_str),
            Some("gpg ← git ← bash ← claude ← nu ← kitty")
        );
        Ok(())
    }

    #[test]
    fn tools_rank_before_newer_idle_clients() {
        let editor = (10, 900, false);
        let gpg = (11, 500, true);
        let older_gpg = (12, 400, true);
        let shell = (13, 100, false);
        assert_eq!(
            rank_clients(vec![shell, older_gpg, editor, gpg]),
            [(11, 500), (12, 400), (10, 900), (13, 100)]
        );
    }

    /// Writes a fake procfs process `pid` started at tick `pid` that holds `node` open.
    fn holder(proc_root: &Path, pid: u32, name: &str, node: &Path) -> std::io::Result<()> {
        let dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(dir.join("fd"))?;
        std::os::unix::fs::symlink(node, dir.join("fd/3"))?;
        std::os::unix::fs::symlink(format!("/usr/bin/{name}"), dir.join("exe"))?;
        std::fs::write(dir.join("comm"), format!("{name}\n"))?;
        std::fs::write(
            dir.join("status"),
            "PPid:\t1\nUid:\t1000\t1000\t1000\t1000\n",
        )?;
        std::fs::write(dir.join("cgroup"), "0::/user.slice/session-2.scope\n")?;
        let middle = vec!["0"; 18].join(" ");
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (x) S {middle} {pid} 0 0\n"),
        )
    }

    #[test]
    fn several_device_holders_lower_confidence() -> TestResult {
        let dir = tempfile::tempdir()?;
        let node = Path::new("/dev/touchcue-test-hidraw");
        let target = Target::Node(node.to_owned());
        holder(dir.path(), 300, "ssh-sk-helper", node)?;
        let resolver = Resolver::new(dir.path().to_owned(), Vec::new(), 64);
        let single = attribute(&resolver, &target);
        assert_eq!(single.confidence, Confidence::High);
        assert_eq!(single.chain.as_deref(), Some("ssh-sk-helper"));
        assert_eq!(
            single.requester.and_then(|r| r.name).as_deref(),
            Some("ssh-sk-helper")
        );
        holder(dir.path(), 301, "pcscd", node)?;
        let several = attribute(&resolver, &target);
        assert_eq!(several.pids, [300, 301]);
        assert_eq!(several.confidence, Confidence::Medium);
        Ok(())
    }

    #[tokio::test]
    async fn client_in_an_application_unit_is_named_in_the_application() -> TestResult {
        let dir = tempfile::tempdir()?;
        let proc_root = dir.path().join("proc");
        let node = Path::new("/dev/touchcue-test-hidraw");
        let unit = "0::/user.slice/user@1000.service/app.slice/app-kitty-12.scope\n";
        for (pid, name, parent) in [(402, "ykman", 401), (401, "zsh", 400), (400, "kitty", 1)] {
            holder(&proc_root, pid, name, node)?;
            std::fs::write(
                proc_root.join(pid.to_string()).join("status"),
                format!("PPid:\t{parent}\nUid:\t1000\t1000\t1000\t1000\n"),
            )?;
            std::fs::write(proc_root.join(pid.to_string()).join("cgroup"), unit)?;
        }
        for pid in [400, 401] {
            std::fs::remove_file(proc_root.join(pid.to_string()).join("fd/3"))?;
        }
        let share = dir.path().join("share");
        std::fs::create_dir_all(share.join("applications"))?;
        std::fs::write(
            share.join("applications/kitty.desktop"),
            "[Desktop Entry]\nType=Application\nName=Kitty\nExec=kitty\n",
        )?;
        let resolver = Resolver::new(proc_root, vec![share], 64);
        let attribution = attribute(&resolver, &Target::Node(node.to_owned()));
        let (values, _) = render(
            &Config::default(),
            &request(RequestState::Waiting),
            &attribution,
            Instant::now(),
            &mut Fixed::default(),
        )
        .await;
        assert_eq!(
            values.get("requester.label").map(String::as_str),
            Some("ykman in Kitty")
        );
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_shows_updates_and_hides_in_order() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        let id = RequestId(1);
        d.signal(pending(1), base).await;
        d.signal(
            signal(1, SignalKind::Resolved(Outcome::Cancelled)),
            base + MS * 100,
        )
        .await;
        d.signal(pending(2), base + MS * 500).await;
        d.signal(
            signal(2, SignalKind::Resolved(Outcome::Touched)),
            base + MS * 900,
        )
        .await;
        let high = Some("high".to_owned());
        assert_eq!(
            d.into_sink().out,
            [
                Out::Show(id, "Firefox is waiting for fido2".to_owned()),
                Out::Publish("started", id, high.clone()),
                Out::Update(
                    id,
                    RequestState::Lingering(touchcue_core::EndReason::Cancelled)
                ),
                Out::Publish("updated", id, high.clone()),
                Out::Update(id, RequestState::Waiting),
                Out::Publish("updated", id, high.clone()),
                Out::Hide(id),
                Out::Publish("ended", id, high),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn revival_reattributes_and_started_carries_pids() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        d.signal(pending(1), base).await;
        let pids: Vec<Vec<u32>> = d.machine.active().map(|r| r.pids.clone()).collect();
        assert_eq!(pids, [vec![42, 43]]);
        d.signal(
            signal(1, SignalKind::Resolved(Outcome::Failed)),
            base + MS * 100,
        )
        .await;
        assert_eq!(d.attribute.0, 1);
        d.signal(pending(2), base + MS * 200).await;
        assert_eq!(d.attribute.0, 2);
        Ok(())
    }

    #[tokio::test]
    async fn suppressed_request_is_published_but_never_shown() -> TestResult {
        let mut d =
            daemon("[[rules]]\nmatch = { \"device.vendor\" = \"Yubico\" }\nsuppress = true\n")?;
        let base = Instant::now();
        d.signal(pending(1), base).await;
        d.tick(base + MS * 1500).await;
        d.tick(base + MS * 2500).await;
        let id = RequestId(1);
        let high = Some("high".to_owned());
        let sink = d.into_sink();
        assert_eq!(
            sink.out,
            [
                Out::Publish("started", id, high.clone()),
                Out::Publish("updated", id, high.clone()),
                Out::Publish("ended", id, high),
            ]
        );
        assert_eq!(sink.prompts, [None, None, None]);
        Ok(())
    }

    #[tokio::test]
    async fn published_events_carry_the_shown_prompt() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        d.signal(pending(1), base).await;
        d.tick(base + MS * 1500).await;
        d.tick(base + MS * 2500).await;
        let sink = d.into_sink();
        let [Some(started), Some(updated), None] = sink.prompts.as_slice() else {
            return Err(TestError::NoPrompt);
        };
        assert!(started.contains("is waiting for"), "{started}");
        assert_eq!(started, updated);
        Ok(())
    }

    #[tokio::test]
    async fn resync_repeats_last_published_values() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        let id = RequestId(1);
        d.resync();
        d.signal(pending(1), base).await;
        // Progress keeps the request waiting past 2 s without a new event.
        d.signal(signal(1, SignalKind::Progress), base + MS * 1000)
            .await;
        d.tick(base + MS * 2400).await;
        d.resync();
        d.signal(
            signal(1, SignalKind::Resolved(Outcome::Touched)),
            base + MS * 2600,
        )
        .await;
        d.resync();
        let sink = d.into_sink();
        let started = sink.values.first().cloned().ok_or(TestError::NoPrompt)?;
        assert_eq!(
            started.get("request.elapsed").map(String::as_str),
            Some("0")
        );
        let resyncs: Vec<Out> = sink
            .out
            .into_iter()
            .filter(|out| matches!(out, Out::Resync(_)))
            .collect();
        assert_eq!(
            resyncs,
            [
                Out::Resync(Vec::new()),
                Out::Resync(vec![(id, started)]),
                Out::Resync(Vec::new()),
            ]
        );
        Ok(())
    }

    /// Calls of [`slow_scan`].
    static SLOW_SCANS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Blocks past the attribution timeout, then returns `firefox()`.
    fn slow_scan(_: &Resolver, _: &Target) -> Attribution {
        SLOW_SCANS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(MS * 300);
        firefox()
    }

    /// Waits until no scan holds the permit of `attribution`.
    async fn idle(attribution: &SystemAttribution) -> bool {
        let wait = async {
            while attribution.busy.available_permits() == 0 {
                tokio::time::sleep(MS * 10).await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), wait).await {
            Ok(()) => true,
            Err(_elapsed) => false,
        }
    }

    #[tokio::test]
    async fn rule_icon_names_are_found_in_the_icon_theme() -> TestResult {
        let dir = tempfile::tempdir()?;
        let share = dir.path().join("share");
        let theme = share.join("icons/Fixture");
        std::fs::create_dir_all(theme.join("apps"))?;
        std::fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=Fixture\nDirectories=apps\n[apps]\nSize=64\nType=Fixed\n",
        )?;
        let file = theme.join("apps/touchcue-fixture.svg");
        std::fs::write(&file, b"<svg/>")?;
        let icons = IconLookup::new(vec![share.join("icons")], None, "Fixture".to_owned(), 64);
        let resolver =
            Resolver::new(PathBuf::from("/nonexistent"), Vec::new(), 64).with_icons(icons);
        let mut attribution = SystemAttribution::new(resolver, None);
        let config = Config::from_toml(
            "[[rules]]\nmatch = { \"request.state\" = \"waiting\" }\nicon = \"touchcue-fixture\"\n",
        )?;
        let (_, prompt) = render(
            &config,
            &request(RequestState::Waiting),
            &firefox(),
            Instant::now(),
            &mut attribution,
        )
        .await;
        assert_eq!(prompt.ok_or(TestError::NoPrompt)?.icon, Some(file));
        assert_eq!(attribution.icon("touchcue-missing").await, None);
        assert_eq!(attribution.icon("/nonexistent/touchcue.png").await, None);
        Ok(())
    }

    #[tokio::test]
    async fn busy_attribution_is_skipped_until_the_scan_ends() {
        let resolver = Resolver::new(PathBuf::from("/nonexistent"), Vec::new(), 64);
        let mut attribution = SystemAttribution::with_scan(resolver, None, slow_scan, MS * 20);
        let request = request(RequestState::Waiting);
        assert_eq!(
            attribution.attribute(&request).await,
            Attribution::default()
        );
        assert_eq!(
            attribution.attribute(&request).await,
            Attribution::default()
        );
        assert!(idle(&attribution).await, "the abandoned scan never ended");
        assert_eq!(SLOW_SCANS.load(std::sync::atomic::Ordering::SeqCst), 1);
        attribution.attribute(&request).await;
        assert!(idle(&attribution).await, "the second scan never ended");
        assert_eq!(SLOW_SCANS.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn show_lost_to_a_full_queue_is_retried() -> TestResult {
        let mut d = daemon("")?;
        d.sink.reject_shows = 1;
        let base = Instant::now();
        let id = RequestId(1);
        d.signal(pending(1), base).await;
        d.signal(
            signal(1, SignalKind::Resolved(Outcome::Cancelled)),
            base + MS * 100,
        )
        .await;
        let high = Some("high".to_owned());
        assert_eq!(
            d.into_sink().out,
            [
                Out::Publish("started", id, high.clone()),
                Out::Show(id, "Firefox is waiting for fido2".to_owned()),
                Out::Publish("updated", id, high),
            ]
        );
        Ok(())
    }

    /// Serves one gpg-agent connection at `path` that reports UIF off for
    /// the signing slot and on for the others, on a blocking thread, until the client says `BYE`.
    fn uif_off_agent(path: &Path) -> std::io::Result<tokio::task::JoinHandle<std::io::Result<()>>> {
        use std::io::{BufRead as _, BufReader, Write as _};

        let listener = std::os::unix::net::UnixListener::bind(path)?;
        Ok(tokio::task::spawn_blocking(move || {
            let (stream, _) = listener.accept()?;
            let mut write = stream.try_clone()?;
            write.write_all(b"OK\n")?;
            for line in BufReader::new(stream).lines() {
                let line = line?;
                let reply = match line.strip_prefix("SCD GETATTR ") {
                    Some("UIF-1") => "S UIF-1 %00+\nOK\n".to_owned(),
                    Some(keyword) => format!("S {keyword} %01+\nOK\n"),
                    None => "OK\n".to_owned(),
                };
                write.write_all(reply.as_bytes())?;
                if line == "BYE" {
                    break;
                }
            }
            Ok(())
        }))
    }

    /// Feeds signals from `rx` to `d` until the sink holds `count` entries.
    async fn drive(
        d: &mut Daemon<Fixed, Recorder>,
        rx: &mut tokio::sync::mpsc::Receiver<Signal>,
        count: usize,
    ) -> TestResult {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while d.sink.out.len() < count {
            let signal = tokio::time::timeout_at(deadline, rx.recv())
                .await?
                .ok_or(TestError::TimedOut)?;
            d.signal(signal, Instant::now()).await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn helper_reports_become_requests_unless_uif_is_off() -> TestResult {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        use touchcue_ipc::agent::AgentPaths;
        use touchcue_ipc::helper::{Helper, HelperConfig, HelperOutputs};

        let dir = tempfile::tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        let agent_socket = dir.path().join("S.gpg-agent");
        let agent = uif_off_agent(&agent_socket)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (notices, _notices_rx) = tokio::sync::mpsc::channel(16);
        let helper = Helper::spawn(
            HelperConfig {
                runtime_dir: dir.path().to_owned(),
                agent: Some(AgentPaths {
                    agent: agent_socket,
                    ssh: dir.path().join("S.gpg-agent.ssh"),
                    homedir: dir.path().to_owned(),
                }),
                keepalive: MS * 500,
            },
            HelperOutputs {
                signals: tx,
                notices,
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        let mut reporter =
            std::os::unix::net::UnixStream::connect(dir.path().join("touchcue/helper.sock"))?;
        let mut d = daemon("")?;

        // The first operation is shown: UIF is read only once it ends.
        reporter.write_all(b"v1 start scdaemon 1 sign\nv1 end scdaemon 1 touched\n")?;
        drive(&mut d, &mut rx, 4).await?;
        tokio::time::timeout(Duration::from_secs(10), agent).await???;
        tokio::time::sleep(MS * 100).await;
        // UIF is off now, so this operation never becomes a request.
        reporter.write_all(b"v1 start scdaemon 2 sign\nv1 end scdaemon 2 touched\n")?;
        reporter.write_all(b"v1 start scdaemon 3 decrypt\nv1 end scdaemon 3 touched\n")?;
        drive(&mut d, &mut rx, 8).await?;
        helper.stop().await?;

        let first = RequestId(1);
        let second = RequestId(2);
        let high = Some("high".to_owned());
        assert_eq!(
            d.into_sink().out,
            [
                Out::Show(first, "Firefox is waiting for openpgp".to_owned()),
                Out::Publish("started", first, high.clone()),
                Out::Hide(first),
                Out::Publish("ended", first, high.clone()),
                Out::Show(second, "Firefox is waiting for openpgp".to_owned()),
                Out::Publish("started", second, high.clone()),
                Out::Hide(second),
                Out::Publish("ended", second, high),
            ]
        );
        Ok(())
    }

    fn notice(pid: u32) -> Notice {
        Notice::Started {
            pid,
            detail: "SHA256:abc to git@host".to_owned(),
        }
    }

    fn detail(values: Option<&BTreeMap<String, String>>) -> Option<&str> {
        values?.get("request.detail").map(String::as_str)
    }

    #[tokio::test]
    async fn notice_after_the_start_updates_the_request() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        d.signal(pending(1), base).await;
        d.notice(notice(5), base).await;
        assert_eq!(d.sink.out.len(), 2, "an unrelated pid changes nothing");
        d.notice(notice(77), base + MS * 10).await;
        let id = RequestId(1);
        assert_eq!(
            d.sink.out.get(2..),
            Some(
                &[
                    Out::Update(id, RequestState::Waiting),
                    Out::Publish("updated", id, Some("high".to_owned())),
                ][..]
            )
        );
        assert_eq!(detail(d.sink.values.last()), Some("SHA256:abc to git@host"));
        Ok(())
    }

    #[tokio::test]
    async fn notice_before_the_start_is_applied_until_it_ends() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        d.notice(notice(43), base).await;
        d.signal(pending(1), base + MS * 10).await;
        assert_eq!(detail(d.sink.values.last()), Some("SHA256:abc to git@host"));
        d.signal(
            signal(1, SignalKind::Resolved(Outcome::Touched)),
            base + MS * 20,
        )
        .await;
        d.notice(Notice::Ended { pid: 43 }, base + MS * 30).await;
        d.signal(pending(2), base + MS * 40).await;
        assert_eq!(detail(d.sink.values.last()), None);
        Ok(())
    }

    #[tokio::test]
    async fn expired_notice_is_ignored() -> TestResult {
        let mut d = daemon("")?;
        let base = Instant::now();
        d.notice(notice(42), base).await;
        d.signal(pending(1), base + NOTICE_TTL).await;
        assert_eq!(detail(d.sink.values.last()), None);
        Ok(())
    }
}
