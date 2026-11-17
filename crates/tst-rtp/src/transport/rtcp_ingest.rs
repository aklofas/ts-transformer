//! RTCP reports from an interleaved RTSP session into a receiver's
//! statistics.

use std::sync::Mutex;

use crate::rtcp::ingest::{ingest_rr, ingest_sr};
use crate::rtcp::stats::RtcpStats;
use crate::rtcp::{ReceiverReport, SenderReport};

/// Count one RTCP packet from the interleaved RTCP channel into `stats`:
/// a sender report (PT=200) or a receiver report (PT=201), or a parse
/// error for either. Other packet types (SDES/BYE/APP) do not affect the
/// projected `SocketStats` fields and are ignored. A poisoned `stats` is
/// skipped.
pub(crate) fn ingest_rtcp_packet(stats: &Mutex<RtcpStats>, our_ssrc: u32, bytes: &[u8]) {
    // RFC 3550 §6.4 — PT byte lives at offset 1 of every RTCP packet.
    // Packets shorter than 2 bytes are non-conformant; drop silently.
    let Some(&pt) = bytes.get(1) else {
        return;
    };
    match pt {
        200 => match SenderReport::decode(bytes) {
            Ok((sr, _)) => {
                if let Ok(mut g) = stats.lock() {
                    ingest_sr(&mut g, &sr);
                }
            }
            Err(_) => {
                if let Ok(mut g) = stats.lock() {
                    g.sr_parse_errors = g.sr_parse_errors.saturating_add(1);
                }
            }
        },
        201 => match ReceiverReport::decode(bytes) {
            Ok((rr, _)) => {
                if let Ok(mut g) = stats.lock() {
                    ingest_rr(&mut g, our_ssrc, &rr);
                }
            }
            Err(_) => {
                if let Ok(mut g) = stats.lock() {
                    g.rr_parse_errors = g.rr_parse_errors.saturating_add(1);
                }
            }
        },
        _ => {} // SDES/BYE/APP/etc. — ignored for stats.
    }
}
