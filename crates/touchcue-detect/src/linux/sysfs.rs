//! Discovery of FIDO hidraw nodes through sysfs.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use touchcue_core::{Device, DeviceId, DeviceKind};

use crate::descriptor::is_fido;
use crate::uevent;
use crate::vendor::vendor_name;

/// Largest HID report descriptor the kernel exposes (`HID_MAX_DESCRIPTOR_SIZE`).
const MAX_DESCRIPTOR_BYTES: u64 = 4096;
const MAX_UEVENT_BYTES: u64 = 4096;

/// Lists the FIDO devices under `<sys_root>/class/hidraw`, sorted by id.
///
/// Entries that cannot be read are skipped and logged at debug level.
#[must_use]
pub fn list_fido(sys_root: &Path) -> Vec<Device> {
    let class = sys_root.join("class/hidraw");
    let entries = match std::fs::read_dir(&class) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %class.display(), %error, "cannot list hidraw class");
            return Vec::new();
        }
    };
    let mut devices: Vec<Device> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => entry.file_name().to_str().map(str::to_owned),
            Err(error) => {
                tracing::debug!(%error, "cannot read hidraw class entry");
                None
            }
        })
        .filter_map(|name| fido_device(sys_root, &name))
        .collect();
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    devices
}

/// Returns whether `name` is `hidraw` followed by one or more ASCII digits.
#[must_use]
pub(crate) fn is_node_name(name: &str) -> bool {
    name.strip_prefix("hidraw")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Returns the device for hidraw node `name` when it is a FIDO device.
///
/// Returns `None` when `name` is not a hidraw node name, the node is not FIDO,
/// or its sysfs files cannot be read.
#[must_use]
pub(crate) fn fido_device(sys_root: &Path, name: &str) -> Option<Device> {
    if !is_node_name(name) {
        tracing::debug!(node = name, "not a hidraw node name");
        return None;
    }
    let dir = sys_root.join("class/hidraw").join(name).join("device");
    let desc = match read_limited(&dir.join("report_descriptor"), MAX_DESCRIPTOR_BYTES) {
        Ok(desc) => desc,
        Err(error) => {
            tracing::debug!(node = name, %error, "cannot read report descriptor");
            return None;
        }
    };
    if !is_fido(&desc) {
        return None;
    }
    let text = match read_limited(&dir.join("uevent"), MAX_UEVENT_BYTES) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            tracing::debug!(node = name, %error, "cannot read uevent");
            return None;
        }
    };
    let Some(ids) = uevent::parse(&text) else {
        tracing::debug!(node = name, "uevent has no valid HID_ID");
        return None;
    };
    Some(Device {
        id: DeviceId(format!("/dev/{name}")),
        kind: DeviceKind::Fido,
        transport: ids.transport(),
        vid: Some(ids.vid),
        pid: Some(ids.pid),
        vendor: vendor_name(ids.vid).map(str::to_owned),
        model: None,
        product: ids.name,
    })
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use touchcue_core::Transport;

    use super::*;

    type TestResult = std::io::Result<()>;

    const FIDO_DESC: [u8; 9] = [0x06, 0xd0, 0xf1, 0x09, 0x01, 0xa1, 0x01, 0xc0, 0x00];
    const KEYBOARD_DESC: [u8; 7] = [0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0];

    fn node(sys: &Path, name: &str, desc: &[u8], uevent: &str) -> io::Result<()> {
        let dir = sys.join("class/hidraw").join(name).join("device");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("report_descriptor"), desc)?;
        fs::write(dir.join("uevent"), uevent)
    }

    #[test]
    fn lists_only_fido_nodes() -> TestResult {
        let sys = tempfile::tempdir()?;
        node(
            sys.path(),
            "hidraw3",
            &FIDO_DESC,
            "DRIVER=hid-generic\nHID_ID=0003:00001050:00000407\nHID_NAME=Yubico YubiKey OTP+FIDO+CCID\n",
        )?;
        node(
            sys.path(),
            "hidraw1",
            &KEYBOARD_DESC,
            "HID_ID=0003:0000046D:0000C31C\nHID_NAME=Keyboard\n",
        )?;
        fs::create_dir_all(sys.path().join("class/hidraw/hidraw9"))?;

        assert_eq!(
            list_fido(sys.path()),
            vec![Device {
                id: DeviceId("/dev/hidraw3".to_owned()),
                kind: DeviceKind::Fido,
                transport: Transport::Usb,
                vid: Some(0x1050),
                pid: Some(0x0407),
                vendor: Some("Yubico".to_owned()),
                model: None,
                product: Some("Yubico YubiKey OTP+FIDO+CCID".to_owned()),
            }]
        );
        Ok(())
    }

    #[test]
    fn results_are_sorted_by_id() -> TestResult {
        let sys = tempfile::tempdir()?;
        for name in ["hidraw2", "hidraw0", "hidraw1"] {
            node(
                sys.path(),
                name,
                &FIDO_DESC,
                "HID_ID=0003:00001209:00000001\n",
            )?;
        }
        let ids: Vec<String> = list_fido(sys.path()).into_iter().map(|d| d.id.0).collect();
        assert_eq!(ids, ["/dev/hidraw0", "/dev/hidraw1", "/dev/hidraw2"]);
        Ok(())
    }

    #[test]
    fn node_names_are_validated() -> TestResult {
        assert!(is_node_name("hidraw0"));
        assert!(is_node_name("hidraw12"));
        for name in [
            "hidraw",
            "hidraw0a",
            "hidraw-1",
            "../hidraw0",
            "xhidraw0",
            "",
        ] {
            assert!(!is_node_name(name), "{name}");
        }
        let sys = tempfile::tempdir()?;
        node(
            sys.path(),
            "hidrawX",
            &FIDO_DESC,
            "HID_ID=0003:00001050:00000407\n",
        )?;
        assert_eq!(list_fido(sys.path()), Vec::new());
        Ok(())
    }

    #[test]
    fn oversized_descriptor_is_truncated() -> TestResult {
        let sys = tempfile::tempdir()?;
        let mut late = vec![0u8; 4100];
        late.extend_from_slice(&FIDO_DESC);
        node(
            sys.path(),
            "hidraw0",
            &late,
            "HID_ID=0003:00001050:00000407\n",
        )?;
        let mut early = FIDO_DESC.to_vec();
        early.resize(10_000, 0);
        node(
            sys.path(),
            "hidraw1",
            &early,
            "HID_ID=0003:00001050:00000407\n",
        )?;
        let ids: Vec<String> = list_fido(sys.path()).into_iter().map(|d| d.id.0).collect();
        assert_eq!(ids, ["/dev/hidraw1"]);
        Ok(())
    }

    #[test]
    fn non_utf8_uevent_is_sanitized() -> TestResult {
        let sys = tempfile::tempdir()?;
        let dir = sys.path().join("class/hidraw/hidraw0/device");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("report_descriptor"), FIDO_DESC)?;
        fs::write(
            dir.join("uevent"),
            b"HID_ID=0003:00001050:00000407\nHID_NAME=Key\xff\xfe\x1b[31m\n",
        )?;
        let devices = list_fido(sys.path());
        assert_eq!(devices.len(), 1);
        let product = devices.first().and_then(|d| d.product.clone());
        assert_eq!(product.as_deref(), Some("Key\u{fffd}\u{fffd} [31m"));
        Ok(())
    }

    #[test]
    fn missing_class_dir_is_empty() -> TestResult {
        let sys = tempfile::tempdir()?;
        assert_eq!(list_fido(sys.path()), Vec::new());
        Ok(())
    }
}
