//! Responses of the Assuan line protocol spoken by gpg-agent, and the
//! `OpenPGP` card attributes read through it.
//!
//! Only the client side of a response is parsed: `OK`, `ERR`, `S` status,
//! `D` data, `#` comment and `INQUIRE` lines. Status arguments and data are
//! percent-escaped; in status arguments `+` also stands for a space.

use std::num::ParseIntError;
use std::str::Utf8Error;

use thiserror::Error;

use crate::model::Op;

/// Maximum length of one response line in bytes, without the newline.
pub const MAX_LINE: usize = 64 * 1024;

/// One response line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The command succeeded.
    Ok,
    /// The command failed with this gpg-error code.
    Err {
        code: u32,
    },
    /// A status line; `args` is still escaped, see [`decode_status`].
    Status {
        keyword: String,
        args: Vec<u8>,
    },
    /// Decoded data.
    Data(Vec<u8>),
    Comment,
    /// The server asks the client for data.
    Inquire,
}

/// Reasons a response line is rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LineError {
    #[error("Assuan line is longer than {MAX_LINE} bytes")]
    TooLong,
    #[error("malformed Assuan line")]
    Malformed,
    #[error("Assuan field is not UTF-8")]
    NotUtf8(#[from] Utf8Error),
    #[error("Assuan field is not a valid number")]
    BadNumber(#[from] ParseIntError),
}

/// Parses one response line, with or without its trailing newline.
///
/// # Errors
///
/// Returns [`LineError::TooLong`] for a line over [`MAX_LINE`] bytes,
/// [`LineError::Malformed`] for an unknown line type, an empty status
/// keyword or a bad escape in a data line, and [`LineError::NotUtf8`] or
/// [`LineError::BadNumber`] for an `ERR` line without a numeric code.
pub fn parse_line(line: &[u8]) -> Result<Response, LineError> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.len() > MAX_LINE {
        return Err(LineError::TooLong);
    }
    let (word, rest) = match line.iter().position(|&b| b == b' ') {
        Some(at) => (&line[..at], &line[at + 1..]),
        None => (line, &[][..]),
    };
    match word {
        b"OK" => Ok(Response::Ok),
        b"ERR" => {
            let code = rest.split(|&b| b == b' ').next().unwrap_or_default();
            let code = std::str::from_utf8(code)?.parse()?;
            Ok(Response::Err { code })
        }
        b"S" => {
            let (keyword, args) = match rest.iter().position(|&b| b == b' ') {
                Some(at) => (&rest[..at], &rest[at + 1..]),
                None => (rest, &[][..]),
            };
            let keyword = std::str::from_utf8(keyword)?;
            if keyword.is_empty() {
                return Err(LineError::Malformed);
            }
            Ok(Response::Status {
                keyword: keyword.to_owned(),
                args: args.to_vec(),
            })
        }
        b"D" => decode(rest, false).map(Response::Data),
        b"INQUIRE" => Ok(Response::Inquire),
        _ if line.first() == Some(&b'#') => Ok(Response::Comment),
        _ => Err(LineError::Malformed),
    }
}

/// Decodes escaped status arguments, turning `%XX` into its byte and `+` into a space.
///
/// # Errors
///
/// Returns [`LineError::Malformed`] for a `%` not followed by two hex digits.
pub fn decode_status(args: &[u8]) -> Result<Vec<u8>, LineError> {
    decode(args, true)
}

fn decode(escaped: &[u8], plus_is_space: bool) -> Result<Vec<u8>, LineError> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut input = escaped.iter();
    while let Some(&byte) = input.next() {
        match byte {
            b'%' => {
                let hi = input.next().and_then(|&b| hex(b));
                let lo = input.next().and_then(|&b| hex(b));
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(LineError::Malformed);
                };
                out.push(hi << 4 | lo);
            }
            b'+' if plus_is_space => out.push(b' '),
            _ => out.push(byte),
        }
    }
    Ok(out)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// User interaction flag of an `OpenPGP` card key slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Uif {
    Off,
    On,
    /// On, and cannot be turned off.
    Fixed,
    /// On, and one touch is valid for 15 seconds.
    Cached,
    CachedFixed,
    /// The card does not support the flag.
    Unsupported,
}

impl Uif {
    /// Parses the arguments of an `S UIF-n` status line.
    ///
    /// # Errors
    ///
    /// Returns [`LineError::Malformed`] for a bad escape, an empty value or
    /// an unknown first byte.
    pub fn parse(args: &[u8]) -> Result<Self, LineError> {
        let value = decode_status(args)?;
        match value.first() {
            Some(0x00) => Ok(Self::Off),
            Some(0x01) => Ok(Self::On),
            Some(0x02) => Ok(Self::Fixed),
            Some(0x03) => Ok(Self::Cached),
            Some(0x04) => Ok(Self::CachedFixed),
            Some(0xff) => Ok(Self::Unsupported),
            _ => Err(LineError::Malformed),
        }
    }

    /// Reports whether an operation with this slot waits for a touch.
    #[must_use]
    pub fn requires_touch(self) -> bool {
        matches!(
            self,
            Self::On | Self::Fixed | Self::Cached | Self::CachedFixed
        )
    }
}

/// Number of `OpenPGP` card key slots.
pub const SLOTS: usize = 3;

/// Returns the `OpenPGP` card key slot an operation uses: sign 1, decrypt 2, auth 3.
#[must_use]
pub fn slot(op: Op) -> Option<u8> {
    match op {
        Op::Sign => Some(1),
        Op::Decrypt => Some(2),
        Op::Auth => Some(3),
        Op::Assert | Op::Register | Op::Verify => None,
    }
}

/// A key on an `OpenPGP` card, from an `S KEYINFO` status line.
///
/// The keygrip and card serial number are not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardKey {
    /// Key slot, 1 to 3.
    pub slot: u8,
    /// Manufacturer ID from the card's application identifier.
    pub manufacturer: Option<u16>,
}

/// Parses the arguments of an `S KEYINFO` line of type `T`, a key on a
/// card, with an `OPENPGP.n` key reference.
///
/// Returns `Ok(None)` for other key types and references.
///
/// # Errors
///
/// Returns [`LineError`] for a line that is not UTF-8, misses fields, or
/// has a key slot or manufacturer ID that is not a number.
pub fn parse_keyinfo(args: &[u8]) -> Result<Option<CardKey>, LineError> {
    let args = std::str::from_utf8(args)?;
    let mut fields = args.split(' ');
    let (Some(_keygrip), Some(kind), Some(serial), Some(idstr)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(LineError::Malformed);
    };
    if kind != "T" {
        return Ok(None);
    }
    let Some(slot) = idstr.strip_prefix("OPENPGP.") else {
        return Ok(None);
    };
    let slot: u8 = slot.parse()?;
    if !(1..=3).contains(&slot) {
        return Ok(None);
    }
    Ok(Some(CardKey {
        slot,
        manufacturer: manufacturer(serial)?,
    }))
}

/// Returns the manufacturer ID of an `OpenPGP` application identifier, 16
/// bytes in hex: RID `D276000124`, application `01`, version, manufacturer,
/// serial number and two reserved bytes.
fn manufacturer(aid: &str) -> Result<Option<u16>, LineError> {
    if aid.len() != 32 || !aid.starts_with("D27600012401") {
        return Ok(None);
    }
    let field = aid.get(16..20).ok_or(LineError::Malformed)?;
    Ok(Some(u16::from_str_radix(field, 16)?))
}

/// Returns the name of an `OpenPGP` card manufacturer ID.
#[must_use]
pub fn manufacturer_name(id: u16) -> Option<&'static str> {
    match id {
        0x0006 => Some("Yubico"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRIP: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
    const SERIAL: &str = "D2760001240100000006000000010000";

    #[test]
    fn parses_response_lines() -> Result<(), LineError> {
        assert_eq!(parse_line(b"OK\n")?, Response::Ok);
        assert_eq!(parse_line(b"OK closing connection")?, Response::Ok);
        assert_eq!(
            parse_line(b"ERR 67108881 No such device <SCD>\n")?,
            Response::Err { code: 67_108_881 }
        );
        assert_eq!(
            parse_line(b"S UIF-1 %03+")?,
            Response::Status {
                keyword: "UIF-1".to_owned(),
                args: b"%03+".to_vec()
            }
        );
        assert_eq!(
            parse_line(b"D a%25b%0A+")?,
            Response::Data(b"a%b\n+".to_vec())
        );
        assert_eq!(parse_line(b"# comment")?, Response::Comment);
        assert_eq!(parse_line(b"INQUIRE PINENTRY_LAUNCHED")?, Response::Inquire);
        Ok(())
    }

    #[test]
    fn rejects_bad_lines() {
        let long = vec![b'#'; MAX_LINE + 1];
        assert_eq!(parse_line(&long), Err(LineError::TooLong));
        assert!(matches!(
            parse_line(b"ERR nope"),
            Err(LineError::BadNumber(_))
        ));
        assert!(matches!(parse_line(b"ERR"), Err(LineError::BadNumber(_))));
        assert!(matches!(
            parse_line(b"ERR \xff"),
            Err(LineError::NotUtf8(_))
        ));
        assert_eq!(parse_line(b"S"), Err(LineError::Malformed));
        assert_eq!(parse_line(b"D %G0"), Err(LineError::Malformed));
        assert_eq!(parse_line(b"D %0"), Err(LineError::Malformed));
        assert_eq!(parse_line(b"HELLO"), Err(LineError::Malformed));
        assert_eq!(parse_line(b""), Err(LineError::Malformed));
    }

    #[test]
    fn status_decoding_turns_plus_into_space() -> Result<(), LineError> {
        assert_eq!(decode_status(b"%03+")?, [0x03, b' ']);
        assert_eq!(decode_status(b"a+b%2B%25")?, b"a b+%");
        assert_eq!(decode_status(b"%zz"), Err(LineError::Malformed));
        Ok(())
    }

    #[test]
    fn uif_values() {
        let cases: [(&[u8], Result<Uif, LineError>); 9] = [
            (b"%00+", Ok(Uif::Off)),
            (b"%01+", Ok(Uif::On)),
            (b"%02+", Ok(Uif::Fixed)),
            (b"%03+", Ok(Uif::Cached)),
            (b"%04+", Ok(Uif::CachedFixed)),
            (b"%FF+", Ok(Uif::Unsupported)),
            (b"", Err(LineError::Malformed)),
            (b"%05+", Err(LineError::Malformed)),
            (b"%0", Err(LineError::Malformed)),
        ];
        for (args, expected) in cases {
            assert_eq!(Uif::parse(args), expected, "{args:?}");
        }
        assert!(!Uif::Off.requires_touch());
        assert!(!Uif::Unsupported.requires_touch());
        assert!(Uif::On.requires_touch());
        assert!(Uif::Cached.requires_touch());
        assert!(Uif::CachedFixed.requires_touch());
    }

    #[test]
    fn ops_map_to_slots() {
        assert_eq!(slot(Op::Sign), Some(1));
        assert_eq!(slot(Op::Decrypt), Some(2));
        assert_eq!(slot(Op::Auth), Some(3));
        assert_eq!(slot(Op::Assert), None);
    }

    #[test]
    fn keyinfo_card_keys() -> Result<(), LineError> {
        let line = format!("{GRIP} T {SERIAL} OPENPGP.2 - - - - -");
        assert_eq!(
            parse_keyinfo(line.as_bytes())?,
            Some(CardKey {
                slot: 2,
                manufacturer: Some(6)
            })
        );
        assert_eq!(manufacturer_name(6), Some("Yubico"));
        let other = format!("{GRIP} T 0123 OPENPGP.3 - - - - A");
        assert_eq!(
            parse_keyinfo(other.as_bytes())?,
            Some(CardKey {
                slot: 3,
                manufacturer: None
            })
        );
        let disk = format!("{GRIP} D - - - P - - -");
        assert_eq!(parse_keyinfo(disk.as_bytes())?, None);
        let piv = format!("{GRIP} T {SERIAL} PIV.9A - - - - -");
        assert_eq!(parse_keyinfo(piv.as_bytes())?, None);
        let bad_slot = format!("{GRIP} T {SERIAL} OPENPGP.4 - - - - -");
        assert_eq!(parse_keyinfo(bad_slot.as_bytes())?, None);
        let not_a_slot = format!("{GRIP} T {SERIAL} OPENPGP.x - - - - -");
        assert!(matches!(
            parse_keyinfo(not_a_slot.as_bytes()),
            Err(LineError::BadNumber(_))
        ));
        let bad_serial = format!("{GRIP} T D27600012401000000ZZ000000010000 OPENPGP.1 - - - - -");
        assert!(matches!(
            parse_keyinfo(bad_serial.as_bytes()),
            Err(LineError::BadNumber(_))
        ));
        assert_eq!(parse_keyinfo(GRIP.as_bytes()), Err(LineError::Malformed));
        Ok(())
    }
}
