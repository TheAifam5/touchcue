//! Assuan line tracker for the scdaemon wrapper.
//!
//! A pure state machine fed with the bytes gpg-agent (the client) and
//! scdaemon (the server) exchange. It never sees time: it tells the caller
//! when to arm or disarm the show-delay timer, and the caller reports the
//! timer's expiry with [`Tracker::expire`].
//!
//! Line classification follows libassuan 3.0:
//! - A client command verb is the text before the first space or tab,
//!   compared case-insensitively. `PKSIGN`, `PKDECRYPT` and `PKAUTH` start an
//!   operation.
//! - An inquiry answer ends with a client `END` or `CAN` line, both
//!   case-insensitive.
//! - Server responses are case-sensitive: `OK`, `ERR`, `INQUIRE` and `S`
//!   followed by a space or the end of the line, and `D` followed by a space.
//!
//! Rules while an operation runs:
//! - The timer is armed at the command and re-armed on every server `S` or
//!   `D` line, so it measures how long scdaemon has been silent.
//! - A server `INQUIRE` disarms it until the client's `END` or `CAN`, so PIN
//!   entry never counts. An `INQUIRE` after the timer fired ends the shown
//!   operation as [`Outcome::Cancelled`].
//! - `OK` or `ERR` ends the operation. [`Event::End`] is emitted only when
//!   the timer fired before, so an operation that finished within the delay
//!   produces no report.
//!
//! Only the first [`PREFIX`] bytes of each line are kept, and none of a `D`
//! line past its tag, so data and PIN bytes are not retained. A line longer
//! than [`MAX_LINE`] is not classified at all.

use touchcue_core::{Op, Outcome};

/// Longest line, without its newline, that is classified.
pub const MAX_LINE: usize = 64 * 1024;

/// Bytes kept from the start of a line; enough for every verb and an
/// `ERR` code.
const PREFIX: usize = 24;

/// `GPG_ERR_CODE_MASK`: the code part of a `gpg_error_t`.
const CODE_MASK: u32 = 0xFFFF;
/// `GPG_ERR_SYSTEM_ERROR`: the flag of codes mapped from `errno`.
const SYSTEM_ERROR: u32 = 1 << 15;
/// Codes reported as [`Outcome::Cancelled`]: `GPG_ERR_CANCELED`,
/// `GPG_ERR_FULLY_CANCELED` and `GPG_ERR_ECANCELED`.
const CANCELLED_CODES: [u32; 3] = [99, 198, SYSTEM_ERROR | 0x14];
/// Codes reported as [`Outcome::TimedOut`]: `GPG_ERR_TIMEOUT` and
/// `GPG_ERR_ETIMEDOUT`.
const TIMED_OUT_CODES: [u32; 2] = [62, SYSTEM_ERROR | 0x84];

/// What the caller does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Start the show-delay timer, replacing a running one.
    Arm,
    /// Stop the show-delay timer.
    Disarm,
    /// The operation reported by the last [`Tracker::expire`] ended.
    End(Outcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// An operation runs and the timer is armed.
    Armed(Op),
    /// scdaemon waits for an inquiry answer; the timer is disarmed.
    Inquiring(Op),
    /// The timer fired and the operation was reported.
    Shown(Op),
}

/// Start of the current line in one direction.
#[derive(Debug, Default)]
struct Line {
    prefix: Vec<u8>,
    len: usize,
}

impl Line {
    /// Calls `on_line` with the prefix of every line `bytes` completes.
    fn feed(&mut self, bytes: &[u8], mut on_line: impl FnMut(&[u8])) {
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            let (body, complete) = match chunk.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (chunk, false),
            };
            self.len = self.len.saturating_add(body.len());
            for byte in body {
                if self.prefix.len() >= PREFIX || is_data(&self.prefix) {
                    break;
                }
                self.prefix.push(*byte);
            }
            if complete {
                if self.len <= MAX_LINE {
                    on_line(&self.prefix);
                }
                self.prefix.clear();
                self.len = 0;
            }
        }
    }
}

/// Tracks card operations in one gpg-agent to scdaemon connection.
#[derive(Debug)]
pub struct Tracker {
    client: Line,
    server: Line,
    state: State,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: Line::default(),
            server: Line::default(),
            state: State::Idle,
        }
    }

    /// Feeds bytes gpg-agent sent to scdaemon and appends resulting events
    /// to `events`.
    pub fn client(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        let state = &mut self.state;
        self.client
            .feed(bytes, |line| on_client_line(state, line, events));
    }

    /// Feeds bytes scdaemon sent to gpg-agent and appends resulting events
    /// to `events`.
    pub fn server(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        let state = &mut self.state;
        self.server
            .feed(bytes, |line| on_server_line(state, line, events));
    }

    /// Records that the armed timer fired and returns the operation to
    /// report, or `None` when no timer was armed.
    pub fn expire(&mut self) -> Option<Op> {
        match self.state {
            State::Armed(op) => {
                self.state = State::Shown(op);
                Some(op)
            }
            State::Idle | State::Inquiring(_) | State::Shown(_) => None,
        }
    }

    /// Ends tracking when the connection closes and returns the outcome to
    /// report for an operation that was reported but not answered.
    pub fn close(&mut self) -> Option<Outcome> {
        let state = std::mem::replace(&mut self.state, State::Idle);
        match state {
            State::Shown(_) => Some(Outcome::Cancelled),
            State::Idle | State::Armed(_) | State::Inquiring(_) => None,
        }
    }
}

fn on_client_line(state: &mut State, line: &[u8], events: &mut Vec<Event>) {
    match *state {
        State::Idle => {
            if let Some(op) = operation(line) {
                *state = State::Armed(op);
                events.push(Event::Arm);
            }
        }
        State::Inquiring(op) => {
            if ends_inquiry(line) {
                *state = State::Armed(op);
                events.push(Event::Arm);
            }
        }
        State::Armed(_) | State::Shown(_) => {}
    }
}

fn on_server_line(state: &mut State, line: &[u8], events: &mut Vec<Event>) {
    let response = Response::parse(line);
    match (*state, response) {
        (State::Armed(_), Response::Status | Response::Data) => events.push(Event::Arm),
        (State::Armed(op), Response::Inquire) => {
            *state = State::Inquiring(op);
            events.push(Event::Disarm);
        }
        (State::Armed(_), Response::Done(_)) => {
            *state = State::Idle;
            events.push(Event::Disarm);
        }
        (State::Inquiring(_), Response::Done(_)) => *state = State::Idle,
        (State::Shown(op), Response::Inquire) => {
            *state = State::Inquiring(op);
            events.push(Event::End(Outcome::Cancelled));
        }
        (State::Shown(_), Response::Done(outcome)) => {
            *state = State::Idle;
            events.push(Event::End(outcome));
        }
        _ => {}
    }
}

/// A server line, as far as the tracker cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Response {
    Status,
    Data,
    Inquire,
    /// `OK` or `ERR`, with the outcome it reports.
    Done(Outcome),
    Other,
}

impl Response {
    fn parse(line: &[u8]) -> Self {
        if line.starts_with(b"D ") {
            Self::Data
        } else if keyword(line, b"S").is_some() {
            Self::Status
        } else if keyword(line, b"OK").is_some() {
            Self::Done(Outcome::Touched)
        } else if let Some(rest) = keyword(line, b"ERR") {
            Self::Done(error_outcome(rest))
        } else if keyword(line, b"INQUIRE").is_some() {
            Self::Inquire
        } else {
            Self::Other
        }
    }
}

/// Returns the rest of `line` when it is `word` followed by a space or the
/// end of the line.
fn keyword<'a>(line: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(word)?;
    match rest.first() {
        None => Some(rest),
        Some(b' ') => rest.get(1..),
        Some(_) => None,
    }
}

/// Maps the decimal `gpg_error_t` after `ERR` to an outcome.
fn error_outcome(rest: &[u8]) -> Outcome {
    match error_code(rest).map(|error| error & CODE_MASK) {
        Some(code) if CANCELLED_CODES.contains(&code) => Outcome::Cancelled,
        Some(code) if TIMED_OUT_CODES.contains(&code) => Outcome::TimedOut,
        _ => Outcome::Failed,
    }
}

/// Returns the decimal number after leading spaces, or `None` when there is
/// none or it does not fit in a `u32`.
fn error_code(rest: &[u8]) -> Option<u32> {
    let mut digits = rest
        .iter()
        .skip_while(|byte| **byte == b' ')
        .take_while(|byte| byte.is_ascii_digit())
        .peekable();
    digits.peek()?;
    digits.try_fold(0_u32, |value, &digit| {
        value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
    })
}

/// Returns the operation a client command starts.
fn operation(line: &[u8]) -> Option<Op> {
    let end = line
        .iter()
        .position(|byte| *byte == b' ' || *byte == b'\t')
        .unwrap_or(line.len());
    let verb = line.get(..end)?;
    if verb.eq_ignore_ascii_case(b"PKSIGN") {
        Some(Op::Sign)
    } else if verb.eq_ignore_ascii_case(b"PKDECRYPT") {
        Some(Op::Decrypt)
    } else if verb.eq_ignore_ascii_case(b"PKAUTH") {
        Some(Op::Auth)
    } else {
        None
    }
}

/// Whether a client line ends an inquiry answer, as libassuan's
/// `assuan_inquire` reads it.
fn ends_inquiry(line: &[u8]) -> bool {
    let end = line
        .get(..3)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"END"))
        && matches!(line.get(3), None | Some(b' '));
    let cancel = line
        .get(..3)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"CAN"));
    end || cancel
}

/// Whether a line prefix is a data line, in either direction.
fn is_data(prefix: &[u8]) -> bool {
    matches!(prefix, [b'D' | b'd', b' ', ..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One step of a transcript.
    enum Step {
        Client(&'static str),
        Server(&'static str),
        Expire,
    }
    use Step::{Client, Expire, Server};

    /// Runs `steps` and returns every event, with `Start(op)` standing for
    /// an expiry that reported an operation.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Event(Event),
        Start(Op),
    }

    fn run(steps: &[Step]) -> Vec<Seen> {
        let mut tracker = Tracker::new();
        let mut seen = Vec::new();
        for step in steps {
            let mut events = Vec::new();
            match step {
                Client(bytes) => tracker.client(bytes.as_bytes(), &mut events),
                Server(bytes) => tracker.server(bytes.as_bytes(), &mut events),
                Expire => seen.extend(tracker.expire().map(Seen::Start)),
            }
            seen.extend(events.into_iter().map(Seen::Event));
        }
        seen
    }

    fn event(event: Event) -> Seen {
        Seen::Event(event)
    }

    #[test]
    fn sign_waiting_for_touch_is_reported() {
        let seen = run(&[
            Client("PKSIGN --hash=sha256 OPENPGP.1\n"),
            Expire,
            Server("D 0123456789ABCDEF\n"),
            Server("OK\n"),
        ]);
        assert_eq!(
            seen,
            [
                event(Event::Arm),
                Seen::Start(Op::Sign),
                event(Event::End(Outcome::Touched)),
            ]
        );
    }

    #[test]
    fn quick_operation_is_not_reported() {
        let seen = run(&[
            Client("pkauth OPENPGP.3\n"),
            Server("D 00\n"),
            Server("OK\n"),
            Expire,
        ]);
        assert_eq!(
            seen,
            [event(Event::Arm), event(Event::Arm), event(Event::Disarm)]
        );
    }

    #[test]
    fn pin_inquiry_pauses_the_timer() {
        let seen = run(&[
            Client("PKDECRYPT OPENPGP.2\n"),
            Server("INQUIRE NEEDPIN |A|Please enter the PIN\n"),
            Expire,
            Client("D 313233343536\n"),
            Client("END\n"),
            Server("S PINCACHE_PUT 12345678 OPENPGP.2\n"),
            Expire,
            Server("D 00\nOK\n"),
        ]);
        assert_eq!(
            seen,
            [
                event(Event::Arm),
                event(Event::Disarm),
                event(Event::Arm),
                event(Event::Arm),
                Seen::Start(Op::Decrypt),
                event(Event::End(Outcome::Touched)),
            ]
        );
    }

    #[test]
    fn inquiry_after_show_ends_the_report() {
        let seen = run(&[
            Client("PKSIGN OPENPGP.1\n"),
            Expire,
            Server("INQUIRE NEEDPIN ||Please enter the PIN\n"),
            Client("can\n"),
            Server("ERR 100663395 Operation cancelled <SCD>\n"),
        ]);
        assert_eq!(
            seen,
            [
                event(Event::Arm),
                Seen::Start(Op::Sign),
                event(Event::End(Outcome::Cancelled)),
                event(Event::Arm),
                event(Event::Disarm),
            ]
        );
    }

    #[test]
    fn error_codes_map_to_outcomes() {
        // `gpg_error_t` = source << 24 | code; source 6 is SCD, 4 is the agent.
        let cases = [
            (
                "ERR 100663395 Operation cancelled <SCD>",
                Outcome::Cancelled,
            ),
            ("ERR 67109062 Operation fully cancelled", Outcome::Cancelled),
            ("ERR 100696084 Operation canceled", Outcome::Cancelled),
            ("ERR 100663358 Timeout <SCD>", Outcome::TimedOut),
            ("ERR 100696196 Connection timed out", Outcome::TimedOut),
            ("ERR 100663383 Bad PIN <SCD>", Outcome::Failed),
            ("ERR", Outcome::Failed),
            ("ERR x", Outcome::Failed),
            ("ERR 99999999999 overflow", Outcome::Failed),
        ];
        for (line, outcome) in cases {
            let mut tracker = Tracker::new();
            let mut events = Vec::new();
            tracker.client(b"PKSIGN OPENPGP.1\n", &mut events);
            assert_eq!(tracker.expire(), Some(Op::Sign));
            events.clear();
            tracker.server(format!("{line}\n").as_bytes(), &mut events);
            assert_eq!(events, [Event::End(outcome)], "{line}");
        }
    }

    #[test]
    fn lines_split_across_reads_are_joined() {
        let seen = run(&[
            Client("PKS"),
            Client("IGN OPENPGP.1"),
            Client("\nGETINFO version\n"),
            Expire,
            Server("E"),
            Server("RR 100663358 Timeout\n"),
        ]);
        assert_eq!(
            seen,
            [
                event(Event::Arm),
                Seen::Start(Op::Sign),
                event(Event::End(Outcome::TimedOut)),
            ]
        );
    }

    #[test]
    fn oversized_line_is_ignored_and_the_next_one_is_tracked() {
        let long = format!("PKSIGN {}\n", "A".repeat(MAX_LINE));
        let mut tracker = Tracker::new();
        let mut events = Vec::new();
        for chunk in long.as_bytes().chunks(4096) {
            tracker.client(chunk, &mut events);
        }
        assert_eq!(events, []);
        assert_eq!(tracker.expire(), None);
        tracker.client(b"PKAUTH OPENPGP.3\n", &mut events);
        assert_eq!(events, [Event::Arm]);
        assert_eq!(tracker.expire(), Some(Op::Auth));
    }

    #[test]
    fn other_commands_and_lookalikes_are_ignored() {
        let seen = run(&[
            Client("SERIALNO\n"),
            Server("S SERIALNO D2760001240100000000000000000000\nOK\n"),
            Client("PKSIGNX OPENPGP.1\n"),
            Client("LEARN --force\n"),
            Expire,
        ]);
        assert_eq!(seen, []);
    }

    #[test]
    fn server_keywords_are_case_sensitive() {
        let seen = run(&[
            Client("PKSIGN OPENPGP.1\n"),
            Expire,
            Server("ok\n"),
            Server("# comment\n"),
            Server("OKAY\n"),
        ]);
        assert_eq!(seen, [event(Event::Arm), Seen::Start(Op::Sign)]);
    }

    #[test]
    fn close_ends_only_a_reported_operation() {
        let mut tracker = Tracker::new();
        let mut events = Vec::new();
        tracker.client(b"PKAUTH OPENPGP.3\n", &mut events);
        assert_eq!(tracker.close(), None);
        tracker.client(b"PKAUTH OPENPGP.3\n", &mut events);
        assert_eq!(tracker.expire(), Some(Op::Auth));
        assert_eq!(tracker.close(), Some(Outcome::Cancelled));
        assert_eq!(tracker.close(), None);
    }

    #[test]
    fn data_lines_are_not_retained() {
        let mut line = Line::default();
        line.feed(b"D 3132333435363738393031323334353637", |_| {});
        assert_eq!(line.prefix, b"D ");
    }
}
