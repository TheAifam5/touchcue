//! Line protocol between the daemon and its in-process-tree reporters.
//!
//! A reporter is a short-lived touchcue process started by another program:
//! `touchcue scdaemon` (gpg-agent's `scdaemon-program`) or `touchcue askpass`
//! (OpenSSH's `SSH_ASKPASS`). It sends one line per event over the daemon's
//! helper socket. The daemon never replies.
//!
//! Wire format, one message per line, at most [`MAX_LINE`] bytes including
//! the newline:
//!
//! ```text
//! v1 start <origin> <seq> <op> [<detail>]
//! v1 end <origin> <seq> <outcome>
//! ```
//!
//! `seq` is a `u32` chosen by the reporter and unique within its connection.
//! `detail` is percent-encoded: every byte outside printable ASCII, plus `%`
//! and space, is written as `%XX`.

use thiserror::Error;

use crate::model::{Op, Outcome};

/// Protocol version tag that starts every line.
pub const VERSION: &str = "v1";

/// Maximum length of one line in bytes, including the newline.
pub const MAX_LINE: usize = 1024;

/// Maximum length of a decoded detail string in bytes.
pub const MAX_DETAIL: usize = 256;

const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
];

/// Program that sent a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// `touchcue scdaemon`, between gpg-agent and scdaemon.
    Scdaemon,
    /// `touchcue askpass`, started by OpenSSH for a user-presence notice.
    Askpass,
}

impl Origin {
    /// Returns the wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scdaemon => "scdaemon",
            Self::Askpass => "askpass",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "scdaemon" => Some(Self::Scdaemon),
            "askpass" => Some(Self::Askpass),
            _ => None,
        }
    }
}

/// One reporter event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A card or key operation started and may wait for a touch.
    Start {
        origin: Origin,
        seq: u32,
        op: Op,
        /// Untrusted free text, such as an ssh key fingerprint and
        /// destination. Callers sanitize it before display.
        detail: Option<String>,
    },
    /// The operation with this `seq` finished.
    End {
        origin: Origin,
        seq: u32,
        outcome: Outcome,
    },
}

/// Reasons a line is rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("line is longer than {MAX_LINE} bytes")]
    TooLong,
    #[error("unsupported protocol version")]
    Version,
    #[error("malformed message")]
    Malformed,
    #[error("invalid sequence number")]
    Seq(#[source] std::num::ParseIntError),
    #[error("detail is not UTF-8")]
    Utf8(#[source] std::string::FromUtf8Error),
}

impl Message {
    /// Encodes the message as one line ending in `\n`.
    ///
    /// A detail longer than [`MAX_DETAIL`] bytes is truncated at a character
    /// boundary, so the line always fits in [`MAX_LINE`].
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Start {
                origin,
                seq,
                op,
                detail,
            } => {
                let mut line = format!("{VERSION} start {} {seq} {}", origin.as_str(), op.as_str());
                if let Some(detail) = detail {
                    line.push(' ');
                    encode_detail(detail, &mut line);
                }
                line.push('\n');
                line
            }
            Self::End {
                origin,
                seq,
                outcome,
            } => format!(
                "{VERSION} end {} {seq} {}\n",
                origin.as_str(),
                outcome_name(*outcome)
            ),
        }
    }

    /// Parses one line, with or without its trailing newline.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] for an oversized line, another version, or any
    /// field that does not match the format.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        if line.len() > MAX_LINE {
            return Err(ParseError::TooLong);
        }
        let line = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = line.split(' ');
        if fields.next() != Some(VERSION) {
            return Err(ParseError::Version);
        }
        let kind = fields.next().ok_or(ParseError::Malformed)?;
        let origin = fields
            .next()
            .and_then(Origin::parse)
            .ok_or(ParseError::Malformed)?;
        let seq = fields
            .next()
            .ok_or(ParseError::Malformed)?
            .parse::<u32>()
            .map_err(ParseError::Seq)?;
        let message = match kind {
            "start" => {
                let op = fields
                    .next()
                    .and_then(parse_op)
                    .ok_or(ParseError::Malformed)?;
                let detail = fields.next().map(decode_detail).transpose()?;
                Self::Start {
                    origin,
                    seq,
                    op,
                    detail,
                }
            }
            "end" => {
                let outcome = fields
                    .next()
                    .and_then(parse_outcome)
                    .ok_or(ParseError::Malformed)?;
                Self::End {
                    origin,
                    seq,
                    outcome,
                }
            }
            _ => return Err(ParseError::Malformed),
        };
        if fields.next().is_some() {
            return Err(ParseError::Malformed);
        }
        Ok(message)
    }
}

fn parse_op(s: &str) -> Option<Op> {
    match s {
        "sign" => Some(Op::Sign),
        "decrypt" => Some(Op::Decrypt),
        "auth" => Some(Op::Auth),
        _ => None,
    }
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Touched => "touched",
        Outcome::Cancelled => "cancelled",
        Outcome::Failed => "failed",
        Outcome::TimedOut => "timed_out",
    }
}

fn parse_outcome(s: &str) -> Option<Outcome> {
    match s {
        "touched" => Some(Outcome::Touched),
        "cancelled" => Some(Outcome::Cancelled),
        "failed" => Some(Outcome::Failed),
        "timed_out" => Some(Outcome::TimedOut),
        _ => None,
    }
}

fn encode_detail(detail: &str, out: &mut String) {
    let mut end = detail.len().min(MAX_DETAIL);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    for byte in detail.as_bytes().iter().take(end) {
        if byte.is_ascii_graphic() && *byte != b'%' {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push(HEX_DIGITS[usize::from(byte >> 4)]);
            out.push(HEX_DIGITS[usize::from(byte & 0x0f)]);
        }
    }
}

fn decode_detail(encoded: &str) -> Result<String, ParseError> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut input = encoded.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = input.next().and_then(hex).ok_or(ParseError::Malformed)?;
            let lo = input.next().and_then(hex).ok_or(ParseError::Malformed)?;
            bytes.push(hi << 4 | lo);
        } else {
            bytes.push(byte);
        }
        if bytes.len() > MAX_DETAIL {
            return Err(ParseError::Malformed);
        }
    }
    String::from_utf8(bytes).map_err(ParseError::Utf8)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() -> Result<(), ParseError> {
        let messages = [
            Message::Start {
                origin: Origin::Scdaemon,
                seq: 7,
                op: Op::Sign,
                detail: None,
            },
            Message::Start {
                origin: Origin::Askpass,
                seq: 1,
                op: Op::Auth,
                detail: Some("ED25519-SK SHA256:abc to git@github.com %x\nñ".to_owned()),
            },
            Message::End {
                origin: Origin::Scdaemon,
                seq: 7,
                outcome: Outcome::TimedOut,
            },
        ];
        for message in messages {
            let line = message.encode();
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1);
            assert_eq!(Message::parse(&line)?, message);
        }
        Ok(())
    }

    #[test]
    fn long_detail_is_truncated_on_a_char_boundary() -> Result<(), ParseError> {
        let detail = "é".repeat(MAX_DETAIL);
        let line = Message::Start {
            origin: Origin::Askpass,
            seq: 1,
            op: Op::Auth,
            detail: Some(detail),
        }
        .encode();
        assert!(line.len() <= MAX_LINE);
        let Message::Start {
            detail: Some(decoded),
            ..
        } = Message::parse(&line)?
        else {
            return Err(ParseError::Malformed);
        };
        assert!(decoded.len() <= MAX_DETAIL);
        assert!(decoded.chars().all(|c| c == 'é'));
        Ok(())
    }

    #[test]
    fn rejects_a_bad_sequence_number() {
        let parsed = Message::parse("v1 start scdaemon -1 sign");
        assert!(matches!(parsed, Err(ParseError::Seq(_))), "{parsed:?}");
        let parsed = Message::parse("v1 start askpass 1 auth %FF");
        assert!(matches!(parsed, Err(ParseError::Utf8(_))), "{parsed:?}");
    }

    #[test]
    fn rejects_bad_lines() {
        let too_long = "x".repeat(MAX_LINE + 1);
        let cases = [
            (too_long.as_str(), ParseError::TooLong),
            ("v2 start scdaemon 1 sign", ParseError::Version),
            ("", ParseError::Version),
            ("v1 start scdaemon 1 dance", ParseError::Malformed),
            ("v1 start gpg 1 sign", ParseError::Malformed),
            ("v1 end scdaemon 1", ParseError::Malformed),
            ("v1 end scdaemon 1 touched extra", ParseError::Malformed),
            ("v1 start askpass 1 auth %G0", ParseError::Malformed),
            ("v1 stop scdaemon 1 touched", ParseError::Malformed),
        ];
        for (line, expected) in cases {
            assert_eq!(Message::parse(line), Err(expected), "{line:?}");
        }
    }
}
