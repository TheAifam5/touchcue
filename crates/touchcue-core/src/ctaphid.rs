//! Classification of CTAPHID input reports from FIDO HID devices.
//!
//! Reports are the 64-byte packets of FIDO CTAP 2.1, section 11.2, as read from
//! Linux hidraw without a report ID prefix.

use crate::model::Outcome;

/// U2F/CTAP1 message (`CTAPHID_MSG`) with the initialization bit set.
pub const CTAPHID_MSG: u8 = 0x83;
/// CTAP2 CBOR message (`CTAPHID_CBOR`) with the initialization bit set.
pub const CTAPHID_CBOR: u8 = 0x90;
/// Error response (`CTAPHID_ERROR`) with the initialization bit set.
pub const CTAPHID_ERROR: u8 = 0xBF;
/// Keepalive (`CTAPHID_KEEPALIVE`) with the initialization bit set.
pub const CTAPHID_KEEPALIVE: u8 = 0xBB;
/// Keepalive status: the authenticator is still processing.
pub const PROCESSING: u8 = 0x01;
/// Keepalive status: the authenticator waits for user presence.
pub const UPNEEDED: u8 = 0x02;
/// U2F status word of a successful response.
pub const SW_NO_ERROR: u16 = 0x9000;
/// U2F status word asking the client to retry after a user touch.
pub const SW_CONDITIONS_NOT_SATISFIED: u16 = 0x6985;
/// CTAP2 status byte of a successful CBOR response.
pub const CTAP2_OK: u8 = 0x00;
/// CTAP2 status byte of a denied operation, sent when the user declines.
pub const CTAP2_ERR_OPERATION_DENIED: u8 = 0x27;
/// CTAP2 status byte of a request cancelled while it waited for the user.
pub const CTAP2_ERR_KEEPALIVE_CANCEL: u8 = 0x2D;
/// CTAP2 status byte of a timed-out user action.
pub const CTAP2_ERR_USER_ACTION_TIMEOUT: u8 = 0x2F;
/// CTAP2 status byte of a timed-out operation.
pub const CTAP2_ERR_ACTION_TIMEOUT: u8 = 0x3A;

const INIT_BIT: u8 = 0x80;
const CMD_OFFSET: usize = 4;
const DATA_OFFSET: usize = 7;

/// Meaning of one initialization packet for touch detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frame {
    /// Keepalive with status `UPNEEDED`.
    KeepaliveUpNeeded,
    /// Keepalive with status `PROCESSING`.
    KeepaliveProcessing,
    /// U2F response with status word `0x6985`.
    U2fConditionsNotSatisfied,
    /// Final response to a request.
    ///
    /// A CBOR response maps its status byte as [`cbor_outcome`] does. A U2F
    /// response is [`Outcome::Touched`] for status word `0x9000` and
    /// [`Outcome::Failed`] otherwise. `CTAPHID_ERROR` and unreadable statuses
    /// are [`Outcome::Failed`].
    Done(Outcome),
    /// Any other initialization packet.
    Other,
}

/// Returns the channel ID and frame of an initialization packet.
///
/// Returns `None` for reports shorter than 7 bytes and for continuation
/// packets. The channel ID is bytes 0..4 read big-endian.
#[must_use]
pub fn parse(report: &[u8]) -> Option<(u32, Frame)> {
    let cid = u32::from_be_bytes(*report.first_chunk::<CMD_OFFSET>()?);
    let cmd = *report.get(CMD_OFFSET)?;
    let bcnt = usize::from(u16::from_be_bytes(
        *report.get(CMD_OFFSET + 1..)?.first_chunk::<2>()?,
    ));
    if cmd & INIT_BIT == 0 {
        return None;
    }
    let frame = match cmd {
        CTAPHID_KEEPALIVE => match report.get(DATA_OFFSET) {
            Some(&UPNEEDED) => Frame::KeepaliveUpNeeded,
            Some(&PROCESSING) => Frame::KeepaliveProcessing,
            _ => Frame::Other,
        },
        CTAPHID_MSG => match status_word(report, bcnt) {
            Some(SW_CONDITIONS_NOT_SATISFIED) => Frame::U2fConditionsNotSatisfied,
            Some(SW_NO_ERROR) => Frame::Done(Outcome::Touched),
            _ => Frame::Done(Outcome::Failed),
        },
        CTAPHID_CBOR => Frame::Done(match report.get(DATA_OFFSET) {
            Some(&status) if bcnt >= 1 => cbor_outcome(status),
            _ => Outcome::Failed,
        }),
        CTAPHID_ERROR => Frame::Done(Outcome::Failed),
        _ => Frame::Other,
    };
    Some((cid, frame))
}

/// Maps the status byte of a CTAP2 CBOR response to its outcome.
///
/// Status codes are those of FIDO CTAP 2.1, section 8.2. Statuses other than
/// `CTAP2_OK`, cancellation, denial and the two timeouts are [`Outcome::Failed`].
#[must_use]
pub fn cbor_outcome(status: u8) -> Outcome {
    match status {
        CTAP2_OK => Outcome::Touched,
        CTAP2_ERR_KEEPALIVE_CANCEL | CTAP2_ERR_OPERATION_DENIED => Outcome::Cancelled,
        CTAP2_ERR_USER_ACTION_TIMEOUT | CTAP2_ERR_ACTION_TIMEOUT => Outcome::TimedOut,
        _ => Outcome::Failed,
    }
}

/// Reads the trailing status word of a message that fits in one packet.
fn status_word(report: &[u8], bcnt: usize) -> Option<u16> {
    let start = DATA_OFFSET.checked_add(bcnt.checked_sub(2)?)?;
    Some(u16::from_be_bytes(
        *report.get(start..)?.first_chunk::<2>()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(cid: u32, cmd: u8, data: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[..4].copy_from_slice(&cid.to_be_bytes());
        r[4] = cmd;
        // The low two bytes of the length, big-endian.
        let len = data.len().to_be_bytes();
        r[5..7].copy_from_slice(&len[len.len() - 2..]);
        r[7..7 + data.len()].copy_from_slice(data);
        r
    }

    #[test]
    fn keepalive_statuses() {
        assert_eq!(
            parse(&report(1, CTAPHID_KEEPALIVE, &[UPNEEDED])),
            Some((1, Frame::KeepaliveUpNeeded))
        );
        assert_eq!(
            parse(&report(1, CTAPHID_KEEPALIVE, &[PROCESSING])),
            Some((1, Frame::KeepaliveProcessing))
        );
        assert_eq!(
            parse(&report(1, CTAPHID_KEEPALIVE, &[0x7f])),
            Some((1, Frame::Other))
        );
    }

    #[test]
    fn u2f_conditions_not_satisfied() {
        assert_eq!(
            parse(&report(7, CTAPHID_MSG, &[0x69, 0x85])),
            Some((7, Frame::U2fConditionsNotSatisfied))
        );
        assert_eq!(
            parse(&report(7, CTAPHID_MSG, &[0x01, 0x02, 0x69, 0x85])),
            Some((7, Frame::U2fConditionsNotSatisfied))
        );
    }

    const DONE_OK: Frame = Frame::Done(Outcome::Touched);
    const DONE_ERR: Frame = Frame::Done(Outcome::Failed);

    #[test]
    fn cbor_responses() {
        assert_eq!(
            parse(&report(2, CTAPHID_CBOR, &[CTAP2_OK, 0xa1])),
            Some((2, DONE_OK))
        );
        for (status, outcome) in [
            (CTAP2_ERR_KEEPALIVE_CANCEL, Outcome::Cancelled),
            (CTAP2_ERR_OPERATION_DENIED, Outcome::Cancelled),
            (CTAP2_ERR_USER_ACTION_TIMEOUT, Outcome::TimedOut),
            (CTAP2_ERR_ACTION_TIMEOUT, Outcome::TimedOut),
            // CTAP2_ERR_PIN_INVALID
            (0x31, Outcome::Failed),
            // CTAP1_ERR_OTHER
            (0x7f, Outcome::Failed),
        ] {
            assert_eq!(
                parse(&report(2, CTAPHID_CBOR, &[status])),
                Some((2, Frame::Done(outcome))),
                "{status:#04x}"
            );
        }
        // BCNT 0 with a zero byte after the header is not a status.
        assert_eq!(parse(&report(2, CTAPHID_CBOR, &[])), Some((2, DONE_ERR)));
        assert_eq!(
            parse(&[0, 0, 0, 2, CTAPHID_CBOR, 0, 1]),
            Some((2, DONE_ERR))
        );
    }

    #[test]
    fn msg_responses() {
        assert_eq!(
            parse(&report(2, CTAPHID_MSG, &[0x01, 0x90, 0x00])),
            Some((2, DONE_OK))
        );
        assert_eq!(
            parse(&report(2, CTAPHID_MSG, &[0x6a, 0x80])),
            Some((2, DONE_ERR))
        );
    }

    #[test]
    fn error_response_is_failure() {
        assert_eq!(
            parse(&report(2, CTAPHID_ERROR, &[0x06])),
            Some((2, DONE_ERR))
        );
    }

    #[test]
    fn msg_without_readable_status_word_is_failure() {
        assert_eq!(parse(&report(2, CTAPHID_MSG, &[0x90])), Some((2, DONE_ERR)));
        let mut spilled = report(2, CTAPHID_MSG, &[0x90, 0x00]);
        spilled[5..7].copy_from_slice(&200u16.to_be_bytes());
        assert_eq!(parse(&spilled), Some((2, DONE_ERR)));
    }

    #[test]
    fn host_cancel_is_other() {
        assert_eq!(parse(&report(2, 0x91, &[])), Some((2, Frame::Other)));
    }

    #[test]
    fn other_init_frames() {
        assert_eq!(parse(&report(3, 0x86, &[0; 17])), Some((3, Frame::Other)));
        assert_eq!(parse(&report(3, 0x81, &[1, 2])), Some((3, Frame::Other)));
    }

    #[test]
    fn continuation_packet_is_ignored() {
        assert_eq!(parse(&report(1, 0x00, &[0x69, 0x85])), None);
        assert_eq!(parse(&report(1, 0x7f, &[])), None);
    }

    #[test]
    fn short_report_is_ignored() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0, 0, 0, 1, CTAPHID_KEEPALIVE, 0]), None);
    }

    #[test]
    fn cid_is_big_endian() {
        assert_eq!(
            parse(&report(0x0102_0304, CTAPHID_CBOR, &[])).map(|(cid, _)| cid),
            Some(0x0102_0304)
        );
        assert_eq!(
            parse(&[0xff, 0xff, 0xff, 0xff, CTAPHID_CBOR, 0, 0]),
            Some((0xffff_ffff, DONE_ERR))
        );
    }
}
