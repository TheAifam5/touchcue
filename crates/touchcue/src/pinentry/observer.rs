//! Reads the Assuan stream between gpg-agent and pinentry for dialog
//! attribution.
//!
//! From gpg-agent it keeps only the verb of each line and the value of
//! `OPTION owner=<pid>/<uid>`; from pinentry only whether a line is an `OK`
//! or `ERR` reply. Other bytes are counted and dropped: descriptions,
//! prompts, PINs and data lines are never kept.

use touchcue_core::Outcome;

/// Longest gpg-agent line, terminator included. A longer line stops
/// observing for the rest of the session.
pub const MAX_LINE: usize = 1002;

/// Longest gpg-agent verb that matters: `CONFIRM`.
const CLIENT_VERB: usize = 7;
/// Longest `owner=<pid>/<uid>` value with two 32-bit decimal numbers.
const OWNER_VALUE: usize = 27;
/// Longest pinentry verb that matters: `ERR`.
const SERVER_VERB: usize = 3;

/// What the observed stream means for attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A dialog opened for the gpg-agent client `pid` of user `uid`.
    Open { pid: u32, uid: u32 },
    /// The open dialog ended.
    Close(Outcome),
}

/// Up to `N` bytes of a word; longer words are marked as overflowed and
/// not kept.
#[derive(Debug, Clone, Copy)]
struct Word<const N: usize> {
    bytes: [u8; N],
    len: usize,
    overflow: bool,
}

impl<const N: usize> Word<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
            overflow: false,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        if let Some(slot) = self.bytes.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        } else {
            self.bytes = [0; N];
            self.len = 0;
            self.overflow = true;
        }
    }

    /// The word, or `None` when it was longer than `N`.
    fn get(&self) -> Option<&[u8]> {
        if self.overflow {
            return None;
        }
        self.bytes.get(..self.len)
    }
}

/// A gpg-agent line the observer acts on at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Dialog,
    Other,
}

/// Position within the current gpg-agent line.
#[derive(Debug, Clone, Copy)]
enum ClientLine {
    Verb(Word<CLIENT_VERB>),
    OptionValue(Word<OWNER_VALUE>),
    Rest(Command),
}

/// Position within the current pinentry line.
#[derive(Debug, Clone, Copy)]
struct ServerLine {
    verb: Word<SERVER_VERB>,
    ended: bool,
}

impl ServerLine {
    const fn new() -> Self {
        Self {
            verb: Word::new(),
            ended: false,
        }
    }
}

/// Attribution state of one pinentry session.
#[derive(Debug)]
pub struct Observer {
    client: ClientLine,
    client_len: usize,
    server: ServerLine,
    owner: Option<(u32, u32)>,
    open: bool,
    disabled: bool,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            client: ClientLine::Verb(Word::new()),
            client_len: 0,
            server: ServerLine::new(),
            owner: None,
            open: false,
            disabled: false,
        }
    }

    /// Whether an overlong gpg-agent line stopped observing.
    #[must_use]
    pub const fn disabled(&self) -> bool {
        self.disabled
    }

    /// Feeds bytes gpg-agent sent to pinentry.
    pub fn client(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        for &byte in bytes {
            if self.disabled {
                return;
            }
            self.client_len += 1;
            // A line that has not ended by its last allowed byte is too long.
            if byte != b'\n' && self.client_len >= MAX_LINE {
                self.close(events);
                return;
            }
            match byte {
                b'\n' => {
                    if self.end_word() == Command::Dialog {
                        self.open(events);
                    }
                    self.client = ClientLine::Verb(Word::new());
                    self.client_len = 0;
                }
                b' ' | b'\r' => {
                    self.end_word();
                }
                _ => match &mut self.client {
                    ClientLine::Verb(word) => word.push(byte),
                    ClientLine::OptionValue(word) => word.push(byte),
                    ClientLine::Rest(_) => {}
                },
            }
        }
    }

    /// Feeds bytes pinentry sent to gpg-agent.
    pub fn server(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        for &byte in bytes {
            if self.disabled {
                return;
            }
            match byte {
                b'\n' => {
                    match self.server.verb.get() {
                        Some(b"OK") => self.end(Outcome::Touched, events),
                        Some(b"ERR") => self.end(Outcome::Cancelled, events),
                        _ => {}
                    }
                    self.server = ServerLine::new();
                }
                b' ' | b'\r' => self.server.ended = true,
                _ if !self.server.ended => self.server.verb.push(byte),
                _ => {}
            }
        }
    }

    /// Ends the session: an open dialog ended without a reply.
    pub fn close(&mut self, events: &mut Vec<Event>) {
        self.end(Outcome::Failed, events);
        self.disabled = true;
        self.owner = None;
        self.client = ClientLine::Rest(Command::Other);
        self.server = ServerLine::new();
    }

    /// Ends the current word of the gpg-agent line and returns what the
    /// line is so far.
    fn end_word(&mut self) -> Command {
        let command = match self.client {
            ClientLine::Verb(word) => match word.get() {
                Some(verb) if verb.eq_ignore_ascii_case(b"OPTION") => {
                    self.client = ClientLine::OptionValue(Word::new());
                    return Command::Other;
                }
                Some(verb)
                    if verb.eq_ignore_ascii_case(b"GETPIN")
                        || verb.eq_ignore_ascii_case(b"CONFIRM") =>
                {
                    Command::Dialog
                }
                _ => Command::Other,
            },
            ClientLine::OptionValue(word) => {
                if let Some(owner) = word.get().and_then(parse_owner) {
                    self.owner = Some(owner);
                }
                Command::Other
            }
            ClientLine::Rest(command) => command,
        };
        self.client = ClientLine::Rest(command);
        command
    }

    fn open(&mut self, events: &mut Vec<Event>) {
        if self.open {
            return;
        }
        if let Some((pid, uid)) = self.owner {
            self.open = true;
            events.push(Event::Open { pid, uid });
        }
    }

    fn end(&mut self, outcome: Outcome, events: &mut Vec<Event>) {
        if self.open {
            self.open = false;
            events.push(Event::Close(outcome));
        }
    }
}

/// Parses `owner=<pid>/<uid>`.
fn parse_owner(value: &[u8]) -> Option<(u32, u32)> {
    let value = value.strip_prefix(b"owner=")?;
    let slash = value.iter().position(|&byte| byte == b'/')?;
    let pid = decimal(value.get(..slash)?)?;
    let uid = decimal(value.get(slash + 1..)?)?;
    Some((pid, uid))
}

/// Parses a non-empty run of ASCII digits that fits in a `u32`.
fn decimal(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0_u32, |value, &digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Event = Event::Open {
        pid: 4242,
        uid: 1000,
    };

    fn client(observer: &mut Observer, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        observer.client(bytes, &mut events);
        events
    }

    fn server(observer: &mut Observer, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        observer.server(bytes, &mut events);
        events
    }

    fn with_owner() -> Observer {
        let mut observer = Observer::new();
        assert_eq!(client(&mut observer, b"OPTION owner=4242/1000 host\n"), []);
        observer
    }

    #[test]
    fn getpin_with_owner_opens_and_ok_touches() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"SETDESC secret%0Atext\n"), []);
        assert_eq!(server(&mut observer, b"OK\n"), []);
        assert_eq!(client(&mut observer, b"GETPIN\n"), [OPEN]);
        assert_eq!(server(&mut observer, b"D 123456\n"), []);
        assert_eq!(
            server(&mut observer, b"OK\n"),
            [Event::Close(Outcome::Touched)]
        );
        assert_eq!(server(&mut observer, b"OK\n"), []);
    }

    #[test]
    fn confirm_err_cancels() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"CONFIRM --one-button\n"), [OPEN]);
        assert_eq!(
            server(&mut observer, b"ERR 83886179 Operation cancelled\n"),
            [Event::Close(Outcome::Cancelled)]
        );
    }

    #[test]
    fn close_mid_dialog_fails_once() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"getpin\r\n"), [OPEN]);
        let mut events = Vec::new();
        observer.close(&mut events);
        assert_eq!(events, [Event::Close(Outcome::Failed)]);
        observer.close(&mut events);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn bytes_split_across_chunks_are_joined() {
        let mut observer = Observer::new();
        for byte in b"OPTION owner=7/8\nGET" {
            assert_eq!(client(&mut observer, &[*byte]), []);
        }
        assert_eq!(
            client(&mut observer, b"PIN\n"),
            [Event::Open { pid: 7, uid: 8 }]
        );
        assert_eq!(server(&mut observer, b"O"), []);
        assert_eq!(
            server(&mut observer, b"K\n"),
            [Event::Close(Outcome::Touched)]
        );
    }

    #[test]
    fn dialog_without_owner_is_not_reported() {
        let mut observer = Observer::new();
        assert_eq!(client(&mut observer, b"GETPIN\n"), []);
        assert_eq!(server(&mut observer, b"OK\n"), []);
    }

    #[test]
    fn other_lines_neither_open_nor_close() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"MESSAGE\n"), []);
        assert_eq!(client(&mut observer, b"GETPINX\n"), []);
        assert_eq!(client(&mut observer, b"SETDESC GETPIN\n"), []);
        assert_eq!(client(&mut observer, b"GETPIN\n"), [OPEN]);
        assert_eq!(
            server(
                &mut observer,
                b"S PROGRESS\n# OK\nD OK\nOKAY\nINQUIRE QUALITY x\n"
            ),
            []
        );
        assert_eq!(client(&mut observer, b"D 50\nEND\n"), []);
        assert_eq!(
            server(&mut observer, b"OK\n"),
            [Event::Close(Outcome::Touched)]
        );
    }

    #[test]
    fn malformed_owner_is_ignored() {
        for line in [
            &b"OPTION owner=4242\n"[..],
            b"OPTION owner=/1000\n",
            b"OPTION owner=4242/\n",
            b"OPTION owner=+4242/1000\n",
            b"OPTION owner=4242/1000x host\n",
            b"OPTION owner=4242/1/2\n",
            b"OPTION owner=4294967296/1000\n",
            b"OPTION owner=-1/1000\n",
            b"OPTION owner = 4242/1000\n",
            b"OPTION OWNER=4242/1000\n",
            b"OPTION ttyname=4242/1000\n",
            b"OPTIONS owner=4242/1000\n",
        ] {
            let mut observer = Observer::new();
            assert_eq!(client(&mut observer, line), []);
            assert_eq!(
                client(&mut observer, b"GETPIN\n"),
                [],
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn malformed_owner_keeps_the_previous_one() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"OPTION owner=x/y\n"), []);
        assert_eq!(client(&mut observer, b"GETPIN\n"), [OPEN]);
    }

    #[test]
    fn owner_with_a_long_nodename_parses() {
        let mut observer = Observer::new();
        let mut line = b"option owner=4294967295/0 ".to_vec();
        line.extend(std::iter::repeat_n(b'h', 900));
        line.push(b'\n');
        assert_eq!(client(&mut observer, &line), []);
        assert_eq!(
            client(&mut observer, b"GETPIN\n"),
            [Event::Open {
                pid: u32::MAX,
                uid: 0
            }]
        );
    }

    #[test]
    fn overlong_owner_value_is_ignored() {
        let mut observer = Observer::new();
        let mut line = b"OPTION owner=1/".to_vec();
        line.extend(std::iter::repeat_n(b'0', 100));
        line.extend(b"1\n");
        assert_eq!(client(&mut observer, &line), []);
        assert_eq!(client(&mut observer, b"GETPIN\n"), []);
    }

    #[test]
    fn overlong_line_disables_observing() {
        let mut observer = with_owner();
        assert_eq!(client(&mut observer, b"GETPIN\n"), [OPEN]);
        let long = vec![b'a'; MAX_LINE];
        assert_eq!(
            client(&mut observer, &long),
            [Event::Close(Outcome::Failed)]
        );
        assert!(observer.disabled());
        assert_eq!(client(&mut observer, b"\nOPTION owner=1/2\nGETPIN\n"), []);
        assert_eq!(server(&mut observer, b"OK\n"), []);
    }

    #[test]
    fn line_of_max_length_is_accepted() {
        let mut observer = with_owner();
        let mut line = vec![b'a'; MAX_LINE - 1];
        line.push(b'\n');
        assert_eq!(client(&mut observer, &line), []);
        assert!(!observer.disabled());
        assert_eq!(client(&mut observer, b"GETPIN\n"), [OPEN]);
    }

    #[test]
    fn line_content_is_not_kept() {
        let state = |description: &[u8], data: &[u8]| {
            let mut observer = with_owner();
            assert_eq!(client(&mut observer, description), []);
            assert_eq!(server(&mut observer, data), []);
            format!("{observer:?}")
        };
        assert_eq!(
            state(b"SETDESC SENTINEL-DESCRIPTION", b"D SENTINEL-PIN"),
            state(b"SETDESC xxxxxxxxxxxxxxxxxxxx", b"D xxxxxxxxxxxx"),
        );
        assert_eq!(
            state(b"SENTINEL", b"SENTINEL"),
            state(b"XXXXXXXX", b"XXXXXXXX"),
        );
    }
}
