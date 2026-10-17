//! The `rtsp-rtcp-ingest` thread: RTCP reports from an interleaved RTSP
//! session into a receiver's statistics.

use std::sync::{Arc, Mutex};

use crate::rtcp::ingest::{ingest_rr, ingest_sr};
use crate::rtcp::stats::RtcpStats;
use crate::rtcp::{ReceiverReport, SenderReport};

/// Spawn the `rtsp-rtcp-ingest` background thread.
///
/// Drains `rtcp_rx` (fed by the RTSP client's interleaved pump on the
/// RFC 7826 §14 RTCP channel), parses each frame, and dispatches to
/// [`ingest_sr`] (PT=200) or [`ingest_rr`] (PT=201). Unknown PTs
/// (SDES/BYE/APP) are ignored — they don't affect the projected
/// `SocketStats` fields. Parse errors increment
/// `rtcp_stats.sr_parse_errors` / `rr_parse_errors`.
///
/// The thread exits cleanly when the pump's `Sender<Bytes>` drops
/// (`rtcp_rx.recv()` returns `Err(mpsc::RecvError)`). It is detached
/// (no join) — the transport's lifecycle holds the consumer side, and
/// once the transport drops + the pump dies the channel closes.
pub(super) fn spawn_rtcp_ingest(
    rtcp_rx: std::sync::mpsc::Receiver<bytes::Bytes>,
    stats: Arc<Mutex<RtcpStats>>,
    our_ssrc: u32,
) {
    let spawn_result = std::thread::Builder::new()
        .name("rtsp-rtcp-ingest".to_string())
        .spawn(move || {
            while let Ok(bytes) = rtcp_rx.recv() {
                // RFC 3550 §6.4 — PT byte lives at offset 1 of every RTCP packet.
                // Packets shorter than 2 bytes are non-conformant; drop silently.
                if bytes.len() < 2 {
                    continue;
                }
                let pt = bytes[1];
                match pt {
                    200 => match SenderReport::decode(&bytes) {
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
                    201 => match ReceiverReport::decode(&bytes) {
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
                    _ => continue, // SDES/BYE/APP/etc. — ignored for stats.
                }
            }
        });
    // Degrade gracefully instead of panicking: a thread-spawn failure (OS
    // resource exhaustion) must NOT panic — this runs on the RTSP connect path,
    // and the JVM/C bindings do not catch unwinds across the FFI boundary, so a
    // panic here would abort the host process. If ingest can't start, RTCP stats
    // simply stay unpopulated (reporting is best-effort / experimental anyway).
    if let Err(e) = spawn_result {
        tracing::warn!(
            target: "tst_rtp",
            error = %e,
            "failed to spawn rtsp-rtcp-ingest thread; RTCP stats will not be collected"
        );
    }
}
