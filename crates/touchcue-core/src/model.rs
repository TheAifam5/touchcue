//! Devices and the signals detectors report about them.

/// Physical link between the host and an authenticator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Transport {
    Usb,
    Bluetooth,
    Nfc,
    Other,
}

impl Transport {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
            Self::Nfc => "nfc",
            Self::Other => "other",
        }
    }
}

/// Function of the device interface a signal was observed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceKind {
    Fido,
    OpenPgp,
    Piv,
    Otp,
    Fingerprint,
    Wallet,
}

impl DeviceKind {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fido => "fido",
            Self::OpenPgp => "openpgp",
            Self::Piv => "piv",
            Self::Otp => "otp",
            Self::Fingerprint => "fingerprint",
            Self::Wallet => "wallet",
        }
    }
}

/// Stable identifier of a device, unique among the devices present at one time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub String);

/// An authenticator as seen by a detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub transport: Transport,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub product: Option<String>,
}

/// Detector that produced a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Fido,
    Gpg,
    Ssh,
    Ccid,
    Hmac,
    Fprintd,
    Trezor,
    Ledger,
}

impl Source {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fido => "fido",
            Self::Gpg => "gpg",
            Self::Ssh => "ssh",
            Self::Ccid => "ccid",
            Self::Hmac => "hmac",
            Self::Fprintd => "fprintd",
            Self::Trezor => "trezor",
            Self::Ledger => "ledger",
        }
    }
}

/// How directly a signal shows that the device waits for a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SignalClass {
    /// The authenticator itself reports that it waits for user presence.
    Asserted,
    /// Client activity was observed and the wait is inferred after a delay.
    Activity,
    /// The wait is inferred from the device node disappearing.
    Disappearance,
}

impl SignalClass {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Asserted => "asserted",
            Self::Activity => "activity",
            Self::Disappearance => "disappearance",
        }
    }
}

/// Protocol of the operation that waits for a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    Fido2,
    U2f,
    OpenPgp,
    Ssh,
    Piv,
    Oath,
    Hmac,
    Fingerprint,
    Wallet,
}

impl Method {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fido2 => "fido2",
            Self::U2f => "u2f",
            Self::OpenPgp => "openpgp",
            Self::Ssh => "ssh",
            Self::Piv => "piv",
            Self::Oath => "oath",
            Self::Hmac => "hmac",
            Self::Fingerprint => "fingerprint",
            Self::Wallet => "wallet",
        }
    }
}

/// Operation that waits for a touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Op {
    Sign,
    Decrypt,
    Auth,
    Assert,
    Register,
    Verify,
}

impl Op {
    /// Returns the lowercase name used in templates.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sign => "sign",
            Self::Decrypt => "decrypt",
            Self::Auth => "auth",
            Self::Assert => "assert",
            Self::Register => "register",
            Self::Verify => "verify",
        }
    }
}

/// How an operation that waited for a touch finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Outcome {
    Touched,
    Cancelled,
    Failed,
    TimedOut,
}

/// What a signal reports about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalKind {
    /// The device waits for a touch.
    Pending { method: Method, op: Option<Op> },
    /// The device is busy with the request but does not wait for a touch.
    Progress,
    /// The operation finished.
    Resolved(Outcome),
}

/// One observation from a detector.
///
/// `channel`, such as a CTAPHID channel ID, does not separate concurrent
/// requests: a device and source have at most one request, and `channel`
/// only guards which progress and resolved signals apply to it. A source must
/// use channels consistently, so `Some` on pending signals means `Some` on
/// resolved signals. `pids` lists the client processes known to take part and
/// may be empty when they are not yet attributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub device: Device,
    pub source: Source,
    pub class: SignalClass,
    pub kind: SignalKind,
    pub channel: Option<u32>,
    pub pids: Vec<u32>,
}
