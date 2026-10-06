//! Recognition of FIDO authenticators from HID report descriptors.
//!
//! Follows systemd's `fido_id_desc.c` and HID 1.11, section 6.2.2.

const LONG_ITEM: u8 = 0xfe;
const TYPE_GLOBAL: u8 = 1;
const TYPE_LOCAL: u8 = 2;
const TAG_USAGE_PAGE: u8 = 0;
const TAG_USAGE: u8 = 0;
/// FIDO Alliance usage page `0xF1D0`, usage `CTAPHID` (`0x01`).
const FIDO_USAGE_CTAPHID: u32 = 0xF1D0_0001;

/// Returns whether a report descriptor declares the FIDO CTAPHID usage.
///
/// Returns `false` for malformed or truncated descriptors.
#[must_use]
pub fn is_fido(desc: &[u8]) -> bool {
    let mut usage: u32 = 0;
    let mut pos: usize = 0;
    while let Some(&prefix) = desc.get(pos) {
        if prefix == LONG_ITEM {
            let Some(&len) = desc.get(pos.saturating_add(1)) else {
                return false;
            };
            pos = pos.saturating_add(usize::from(len)).saturating_add(3);
            continue;
        }
        let tag = prefix >> 4;
        let kind = (prefix >> 2) & 3;
        let size = match prefix & 3 {
            3 => 4,
            code => usize::from(code),
        };
        pos = pos.saturating_add(1);
        let Some(bytes) = desc.get(pos..pos.saturating_add(size)) else {
            return false;
        };
        let value = bytes
            .iter()
            .rev()
            .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
        pos = pos.saturating_add(size);

        if kind == TYPE_GLOBAL && tag == TAG_USAGE_PAGE {
            if size > 2 {
                return false;
            }
            usage = (value & 0xffff) << 16;
        }
        if kind == TYPE_LOCAL && tag == TAG_USAGE {
            usage = if size == 4 {
                value
            } else {
                (usage & 0xffff_0000) | (value & 0xffff)
            };
            if usage == FIDO_USAGE_CTAPHID {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const YUBIKEY_FIDO: [u8; 34] = [
        0x06, 0xd0, 0xf1, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x20, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
        0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x21, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95,
        0x40, 0x91, 0x02, 0xc0,
    ];

    #[test]
    fn yubikey_fido_descriptor_matches() {
        assert!(is_fido(&YUBIKEY_FIDO));
    }

    #[test]
    fn keyboard_descriptor_does_not_match() {
        assert!(!is_fido(&[
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0xc0
        ]));
    }

    #[test]
    fn long_item_is_skipped() {
        let mut desc = vec![LONG_ITEM, 2, 0x10, 0xaa, 0xbb];
        desc.extend_from_slice(&YUBIKEY_FIDO);
        assert!(is_fido(&desc));
    }

    #[test]
    fn four_byte_usage_matches_without_page() {
        assert!(is_fido(&[0x0b, 0x01, 0x00, 0xd0, 0xf1]));
    }

    #[test]
    fn truncated_or_empty_input_does_not_match() {
        assert!(!is_fido(&[]));
        assert!(!is_fido(&YUBIKEY_FIDO[..2]));
        assert!(!is_fido(&YUBIKEY_FIDO[..4]));
        assert!(!is_fido(&[LONG_ITEM]));
    }

    #[test]
    fn four_byte_usage_page_does_not_match() {
        assert!(!is_fido(&[0x07, 0x00, 0x00, 0xd0, 0xf1, 0x09, 0x01]));
    }

    #[test]
    fn usage_before_its_page_does_not_match() {
        assert!(!is_fido(&[0x09, 0x01, 0x06, 0xd0, 0xf1, 0xa1, 0x01, 0xc0]));
    }
}
