//! Display names of security key vendors.

/// Returns the display name of a USB vendor ID, for labelling only.
///
/// Returns `None` for unknown IDs and for shared IDs such as pid.codes
/// (`0x1209`), whose products come from many vendors.
#[must_use]
pub fn vendor_name(vid: u16) -> Option<&'static str> {
    match vid {
        0x1050 => Some("Yubico"),
        0x20a0 => Some("Nitrokey"),
        0x18d1 => Some("Google"),
        0x096e => Some("Feitian"),
        0x349e => Some("Token2"),
        0x1d50 => Some("OnlyKey"),
        0x2c97 => Some("Ledger"),
        0x534c => Some("Trezor"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_shared_ids() {
        assert_eq!(vendor_name(0x1050), Some("Yubico"));
        assert_eq!(vendor_name(0x1209), None);
        assert_eq!(vendor_name(0x0000), None);
    }
}
