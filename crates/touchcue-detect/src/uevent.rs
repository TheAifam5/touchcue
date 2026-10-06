//! Parsing of HID device `uevent` files.

use touchcue_core::Transport;
use touchcue_core::text::sanitize;
use tracing::trace;

const BUS_USB: u16 = 0x0003;
const BUS_BLUETOOTH: u16 = 0x0005;
/// Longest `HID_NAME` kept, in chars.
const MAX_NAME_CHARS: usize = 128;

/// Identity of a HID device from its `uevent` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidIds {
    pub bus: u16,
    pub vid: u16,
    pub pid: u16,
    pub name: Option<String>,
}

impl HidIds {
    /// Returns the transport for the kernel bus type.
    #[must_use]
    pub fn transport(&self) -> Transport {
        match self.bus {
            BUS_USB => Transport::Usb,
            BUS_BLUETOOTH => Transport::Bluetooth,
            _ => Transport::Other,
        }
    }
}

/// Parses the `HID_ID` and `HID_NAME` lines of a HID `uevent` file.
///
/// `HID_ID` has the kernel format `%04X:%08X:%08X` (bus, vendor, product);
/// vendor and product keep their low 16 bits. Only the first line of each key
/// counts. Returns `None` when `HID_ID` is missing or malformed.
///
/// The name is sanitized for display and cut to 128 chars; a name with nothing
/// visible is `None`.
#[must_use]
pub fn parse(text: &str) -> Option<HidIds> {
    let mut id_line = None;
    let mut name_line = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("HID_ID=") {
            id_line.get_or_insert(value);
        } else if let Some(value) = line.strip_prefix("HID_NAME=") {
            name_line.get_or_insert(value);
        }
    }
    let (bus, vid, pid) = parse_id(id_line?)?;
    Some(HidIds {
        bus,
        vid,
        pid,
        name: name_line.and_then(|name| sanitize(name, MAX_NAME_CHARS)),
    })
}

fn parse_id(value: &str) -> Option<(u16, u16, u16)> {
    let mut parts = value.split(':');
    let bus = hex(parts.next()?, 4)?;
    let vid = hex(parts.next()?, 8)?;
    let pid = hex(parts.next()?, 8)?;
    if parts.next().is_some() {
        return None;
    }
    Some((low16(bus), low16(vid), low16(pid)))
}

fn hex(digits: &str, len: usize) -> Option<u32> {
    if digits.len() != len || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match u32::from_str_radix(digits, 16) {
        Ok(number) => Some(number),
        Err(error) => {
            trace!(
                error = &error as &dyn std::error::Error,
                "uevent hex field unparsable"
            );
            None
        }
    }
}

fn low16(value: u32) -> u16 {
    let [low, high, _, _] = value.to_le_bytes();
    u16::from_le_bytes([low, high])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yubikey_uevent() {
        let ids = parse("HID_ID=0003:00001050:00000407\nHID_NAME=Yubico YubiKey OTP+FIDO+CCID\n");
        assert_eq!(
            ids,
            Some(HidIds {
                bus: 0x0003,
                vid: 0x1050,
                pid: 0x0407,
                name: Some("Yubico YubiKey OTP+FIDO+CCID".to_owned()),
            })
        );
        assert_eq!(ids.map(|i| i.transport()), Some(Transport::Usb));
    }

    #[test]
    fn takes_low_16_bits_of_ids() {
        let ids = parse("HID_ID=0005:0001ABCD:FFFF1234\n");
        assert_eq!(
            ids.as_ref().map(|i| (i.vid, i.pid, i.transport())),
            Some((0xabcd, 0x1234, Transport::Bluetooth))
        );
        assert_eq!(ids.and_then(|i| i.name), None);
    }

    #[test]
    fn other_bus_is_other_transport() {
        assert_eq!(
            parse("HID_ID=0018:00001050:00000407").map(|i| i.transport()),
            Some(Transport::Other)
        );
    }

    #[test]
    fn first_id_wins() {
        let injected = parse("HID_ID=0003:00001050:00000407\nHID_ID=0005:00001234:00005678\n");
        assert_eq!(
            injected.map(|i| (i.bus, i.vid, i.pid)),
            Some((0x0003, 0x1050, 0x0407))
        );
        let malformed_later = parse("HID_ID=0003:00001050:00000407\nHID_ID=bogus\n");
        assert_eq!(
            malformed_later.map(|i| (i.vid, i.pid)),
            Some((0x1050, 0x0407))
        );
    }

    #[test]
    fn name_is_sanitized() {
        let ids =
            parse("HID_ID=0003:00001050:00000407\nHID_NAME=\x1b[2JKey\tOne \nHID_NAME=Other\n");
        assert_eq!(ids.and_then(|i| i.name).as_deref(), Some("[2JKey One"));
        let long = format!(
            "HID_ID=0003:00001050:00000407\nHID_NAME={}\n",
            "é".repeat(200)
        );
        assert_eq!(
            parse(&long).and_then(|i| i.name).map(|n| n.chars().count()),
            Some(128)
        );
        assert_eq!(
            parse("HID_ID=0003:00001050:00000407\nHID_NAME=\x07 \n").and_then(|i| i.name),
            None
        );
    }

    #[test]
    fn missing_or_malformed_id_is_none() {
        assert_eq!(parse("HID_NAME=x\n"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("HID_ID=03:00001050:00000407"), None);
        assert_eq!(parse("HID_ID=0003:00001050"), None);
        assert_eq!(parse("HID_ID=0003:00001050:00000407:0"), None);
        assert_eq!(parse("HID_ID=0003:+0001050:00000407"), None);
    }
}
