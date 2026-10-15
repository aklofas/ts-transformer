//! Helpers shared by the muxing test files (declared once in `muxing.rs`).

use tst_core::mpegts::mux::Muxer;
use tstrans::muxer::{TstMuxer, tst_muxer_pull};

/// Drain all buffered TS bytes from a C muxer handle into one `Vec`.
pub(crate) fn drain_mux(mux: *mut TstMuxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 188];
    loop {
        let n = unsafe { tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len()) };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Drain all buffered TS bytes from a Rust `tst_core` muxer.
pub(crate) fn drain_core_muxer(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 188 * 256];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Locate the first PES start in `ts_bytes` on the given PID and return
/// the index into `ts_bytes` of the first byte of the PES header.
///
/// Returns `None` if no TS packet on `pid` has `payload_unit_start_indicator`
/// set.
pub(crate) fn find_pes_start(ts_bytes: &[u8], pid: u16) -> Option<usize> {
    assert_eq!(
        ts_bytes.len() % 188,
        0,
        "TS must be aligned to 188-byte packets"
    );
    for pkt in ts_bytes.chunks(188) {
        // TS header: sync(1) + error/PUSI/priority+PID_hi(1) + PID_lo(1) + AFC+CC(1)
        if pkt[0] != 0x47 {
            continue;
        }
        let pusi = (pkt[1] & 0x40) != 0;
        let pkt_pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        if pkt_pid != pid || !pusi {
            continue;
        }
        // Locate payload start within the packet.
        let afc = (pkt[3] >> 4) & 0x03;
        let payload_offset = match afc {
            // 0b01 = payload only
            1 => 4,
            // 0b11 = adaptation field + payload
            3 => 4 + 1 + pkt[4] as usize,
            _ => continue,
        };
        if payload_offset >= 188 {
            continue;
        }
        // The TS packet payload starts with the pointer_field when PUSI is set
        // for sections, but for PES streams the payload IS the PES header
        // directly (no pointer field). Return the index of the PES start code.
        let pkt_start = unsafe {
            // SAFETY: ts_bytes is a contiguous slice; pkt is a sub-slice of it.
            pkt.as_ptr().offset_from(ts_bytes.as_ptr()) as usize
        };
        return Some(pkt_start + payload_offset);
    }
    None
}

/// Build a minimal AV1 access unit (TD, Sequence Header, Frame Header,
/// Tile Group OBUs). Each OBU has `obu_has_size_field = 1` so the
/// demuxer can recover them from raw OBU framing without ambiguity
/// (matches the existing `crates/tst-core/tests/av1_carriage_roundtrip.rs`
/// corpus).
pub(crate) fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        // AV1 spec §5.3.2 OBU header byte:
        //   obu_forbidden_bit  f(1) = 0
        //   obu_type           f(4)
        //   obu_extension_flag f(1) = 0
        //   obu_has_size_field f(1) = 1
        //   obu_reserved_1bit  f(1) = 0
        // = (obu_type << 3) | 0b010
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        // Single-byte LEB128 size (bodies < 128 bytes here).
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter
    au.extend(obu(1, &[0x00, 0x00])); // Sequence Header (placeholder)
    au.extend(obu(3, &[0x00])); // Frame Header (placeholder)
    au.extend(obu(4, &[0x00, 0x01, 0x02])); // Tile Group (placeholder)
    au
}
