//! Verifies DVB-teletext PES per ETSI EN 300 472 §4.2.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec, VideoCodec,
};

/// Drain the muxer once and reassemble the full PES bytes (header included)
/// from every TS packet on `pid`. We need the header to inspect
/// `PES_header_data_length` and `PES_packet_length` directly.
fn reassemble_pes_full(mux: &mut Muxer, pid: u16) -> Vec<u8> {
    let mut ts_buf = vec![0u8; 1316 * 4];
    let n = mux.pull(&mut ts_buf);
    let mut pes_full = Vec::new();
    for pkt in ts_buf[..n].chunks_exact(188) {
        let p = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        if p != pid {
            continue;
        }
        let af_present = (pkt[3] & 0x20) != 0;
        let mut idx = 4usize;
        if af_present {
            let af_len = pkt[idx] as usize;
            idx += 1 + af_len;
        }
        if idx < 188 {
            pes_full.extend_from_slice(&pkt[idx..188]);
        }
    }
    pes_full
}

#[test]
fn dvb_teletext_pes_uses_45_byte_header_with_stuffing() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 0, // magazine 8 (3-bit wrap)
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Synthetic teletext data: data_identifier(0x10) + data_unit_id(0x02) +
    // length(0x2C=44) + 44 bytes of line data. Total = 47 bytes.
    let mut payload = vec![0x10, 0x02, 0x2C];
    payload.extend(std::iter::repeat_n(0x00, 0x2C));
    assert_eq!(payload.len(), 47);

    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), &payload)
        .unwrap();

    let pes_full = reassemble_pes_full(&mut mux, 0x200);

    // PES_header_data_length is byte 8 of the PES packet (0-indexed).
    assert_eq!(
        pes_full[8], 0x24,
        "EN 300 472 §4.2 mandates PES_header_data_length=0x24 (36)"
    );
    // PES_packet_length lives at bytes 4..6 (after start_code(3) + stream_id(1)).
    let pes_packet_length = u16::from_be_bytes([pes_full[4], pes_full[5]]) as usize;
    // Total PES bytes = 6 + pes_packet_length (per H.222.0 PES_packet_length defn).
    let total_pes_len = 6 + pes_packet_length;
    assert_eq!(
        total_pes_len % 184,
        0,
        "EN 300 472 §4.2 mandates PES_packet_length=(N×184)−6, total PES = N×184"
    );
    // Header is 45 bytes; bytes 9..14 are the 5-byte PTS field; bytes 14..45
    // are the 31-byte stuffing run of 0xFF.
    for (i, &b) in pes_full[14..45].iter().enumerate() {
        assert_eq!(
            b,
            0xFF,
            "PES header stuffing byte {} must be 0xFF per EN 300 472 §4.2",
            i + 14
        );
    }
    // Caller payload starts at byte 45. No auto-prepend: payload[0]=0x10 is in
    // 0x10..=0x1F so no extra data_identifier byte is inserted.
    assert_eq!(&pes_full[45..45 + payload.len()], &payload[..]);
    // Bytes after caller payload, up to total_pes_len, must be spec-conformant
    // stuffing_data_units per EN 300 472 §4.4: each unit is 46 bytes —
    // [data_unit_id=0xFF, data_unit_length=0x2C, 0x00 × 44].
    // Input: 47 bytes; header: 45; total useful: 92; N×184 = 184; tail: 92 bytes.
    // 92 / 46 = 2 exactly → two whole stuffing_data_units.
    let tail = &pes_full[45 + payload.len()..total_pes_len];
    assert_eq!(
        tail.len(),
        92,
        "tail must be 92 bytes (2 × 46-byte stuffing units)"
    );
    for (unit_idx, unit) in tail.chunks_exact(46).enumerate() {
        assert_eq!(
            unit[0], 0xFF,
            "stuffing_data_unit[{}] data_unit_id must be 0xFF",
            unit_idx
        );
        assert_eq!(
            unit[1], 0x2C,
            "stuffing_data_unit[{}] data_unit_length must be 44 (0x2C)",
            unit_idx
        );
        assert!(
            unit[2..].iter().all(|&b| b == 0x00),
            "stuffing_data_unit[{}] padding bytes must be 0x00",
            unit_idx
        );
    }
}

#[test]
fn dvb_teletext_pes_grows_to_next_ts_packet_boundary_for_large_payload() {
    // Two-line payload = 1 + 2*(2+44) = 93 bytes; header 45 + payload 93 = 138 bytes total.
    // ceil(138 / 184) = 1 TS packet → total PES = 184 bytes; stuffing tail
    // = 184 − 138 = 46 bytes of 0xFF.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, VideoCodec::H264);
        prog.add_subtitle(
            0x200,
            SubtitleCodec::DvbTeletext {
                language: *b"eng",
                teletext_type: 0x02,
                magazine_number: 0,
                page_number: 0x88,
            },
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let mut payload = vec![0x10];
    for _ in 0..2 {
        payload.extend_from_slice(&[0x02, 0x2C]);
        payload.extend(std::iter::repeat_n(0x00, 0x2C));
    }
    assert_eq!(payload.len(), 1 + 2 * (2 + 0x2C));

    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), &payload)
        .unwrap();

    let pes_full = reassemble_pes_full(&mut mux, 0x200);
    let pes_packet_length = u16::from_be_bytes([pes_full[4], pes_full[5]]) as usize;
    let total = 6 + pes_packet_length;
    assert_eq!(
        total % 184,
        0,
        "PES must be N×184 bytes per EN 300 472 §4.2"
    );
}

fn teletext_mux() -> Muxer {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, VideoCodec::H264);
    prog.add_subtitle(
        0x200,
        SubtitleCodec::DvbTeletext {
            language: *b"eng",
            teletext_type: 0x02,
            magazine_number: 1,
            page_number: 0x00,
        },
    );
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    Muxer::new(b.build().unwrap()).unwrap()
}

/// The teletext PES the muxer writes for one push of `payload`.
fn teletext_pes(payload: &[u8]) -> Vec<u8> {
    let mut mux = teletext_mux();
    let h = mux.subtitle_handles()[0];
    mux.push_subtitle_to(h, Pts90khz::new(90_000), payload)
        .unwrap();
    reassemble_pes_full(&mut mux, 0x200)
}

/// An EBU teletext data unit: `02 2C` and 44 bytes of line data.
fn ebu_unit() -> Vec<u8> {
    let mut unit = vec![0x02, 0x2C];
    unit.extend(std::iter::repeat_n(0x00, 0x2C));
    unit
}

/// Walk `id, length, body` data units from `from` and return where the
/// walk ends; a unit that does not fit stops it where it starts.
fn walk_data_units(pes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 2 <= pes.len() && i + 2 + usize::from(pes[i + 1]) <= pes.len() {
        i += 2 + usize::from(pes[i + 1]);
    }
    i
}

#[test]
fn teletext_tail_is_whole_data_units_for_every_remainder() {
    // EN 300 472 §4.3: the PES data field after the data_identifier is a
    // sequence of whole data units. A caller unit of any length leaves a
    // tail of stuffing data units, never a lone byte.
    for extra in 2..=46usize {
        let mut payload = vec![0x10];
        payload.extend(ebu_unit());
        payload.extend([0xFF, (extra - 2) as u8]);
        payload.extend(std::iter::repeat_n(0x00, extra - 2));
        let pes = teletext_pes(&payload);
        assert_eq!(pes.len() % 184, 0, "extra {extra}");
        assert_eq!(pes[45], 0x10, "extra {extra}");
        assert_eq!(
            walk_data_units(&pes, 46),
            pes.len(),
            "extra {extra}: the data units do not reach the end of the PES"
        );
    }
}

#[test]
fn teletext_47_byte_tail_is_one_stuffing_unit() {
    // The identifier, one EBU unit and a 45-byte caller unit end at 137:
    // 47 bytes are left, which is one stuffing unit of length 45.
    let mut payload = vec![0x10];
    payload.extend(ebu_unit());
    payload.extend([0xFF, 0x2B]);
    payload.extend(std::iter::repeat_n(0x00, 0x2B));
    assert_eq!(45 + payload.len(), 137);
    let pes = teletext_pes(&payload);
    assert_eq!(pes.len(), 184);
    assert_eq!(&pes[137..139], &[0xFF, 0x2D]);
    assert!(pes[139..].iter().all(|&b| b == 0x00));
}

#[test]
fn teletext_user_defined_data_identifier_is_kept() {
    // EN 300 472 §4.4 Table 3: 0x80 to 0xFF are user-defined identifiers.
    for id in [0x80, 0x99, 0xFE] {
        let mut payload = vec![id];
        payload.extend(ebu_unit());
        let pes = teletext_pes(&payload);
        assert_eq!(pes[45], id, "identifier 0x{id:02X}");
        assert_eq!(pes[46], 0x02, "identifier 0x{id:02X}");
    }
}

#[test]
fn teletext_unit_ids_and_reserved_first_bytes_still_get_the_identifier() {
    // A unit id (0x02, 0x03, 0xFF) means the caller left the identifier
    // out; 0x20 and 0x7F are reserved for both. Each gets 0x10 in front.
    for first in [0x02, 0x03, 0xFF, 0x20, 0x7F] {
        let pes = teletext_pes(&[first, 0x01, 0x00]);
        assert_eq!(&pes[45..47], &[0x10, first], "first byte 0x{first:02X}");
    }
    let pes = teletext_pes(&[]);
    assert_eq!(pes[45], 0x10, "empty payload");
}
