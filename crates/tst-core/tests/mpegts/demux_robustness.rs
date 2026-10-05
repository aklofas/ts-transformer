//! Robustness tests for `mpegts::demux::Demuxer` against non-conformant TS input.
//!
//! Lenient-mode coverage (the default `StrictMode::Off`):
//!
//! * Garbage prefix before the first 0x47 — demuxer slides past and recovers.
//! * Sustained garbage past `SYNC_SEARCH_WINDOW` — surfaces as
//!   `DemuxError::Unrecoverable`.
//! * Corrupt PAT section payload — checksum mismatch surfaces a
//!   `NonConformant` event (lenient mode keeps the loop alive).
//! * Continuity counter jump on a video PID — surfaces a `Discontinuity`
//!   event with `DiscontinuityKind::ContinuityJump`.
//!
//! Strict-mode rejection of these same issues is exercised in
//! `mpegts_demux_strict.rs`; here we only assert the lenient-mode contract.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, DiscontinuityKind, NonConformantIssue, SamplePayload,
    StrictMode,
};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

fn drain(m: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = m.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Build a known-good clean MPEG-TS byte stream: PAT + PMT + two H.264 AUs
/// large enough to span multiple TS packets each. The size matters: the
/// `cc_jump_emits_discontinuity` test needs at least a couple consecutive
/// packets on the same video PID so the demuxer's continuity-counter
/// tracker has a baseline against which to detect a CC bump. Async-KLV PID
/// is configured so the PMT carries a KLVA registration descriptor, but
/// no KLV payload is pushed — the goal is just a valid stream to corrupt.
fn build_clean_stream() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    // ~400-byte AU with AUD prefix + filler — produces 3 video TS packets.
    let mut au1 = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    au1.extend(std::iter::repeat(0xAB).take(400));
    m.push_video(&au1, Pts90khz::new(0), true).unwrap();
    let mut au2 = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    au2.extend(std::iter::repeat(0xCD).take(400));
    m.push_video(&au2, Pts90khz::new(90_000), false).unwrap();
    drain(&mut m)
}

#[test]
fn recovers_from_garbage_prefix() {
    // 500 bytes of 0xAA before the first 0x47 — well under the demuxer's
    // sync-search window, so it should slide past and parse the rest.
    let prefix = vec![0xAA; 500];
    let mut bytes = prefix;
    bytes.extend_from_slice(&build_clean_stream());

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    // The clean stream has unbounded video PES; flush the partial so any
    // trailing Sample is emitted. ProgramMap arrives at PMT-parse time so
    // it's already queued, but flush keeps the round-trip pattern.
    d.flush();

    let mut saw_program_map = false;
    while let Some(e) = d.next_event() {
        if matches!(e, DemuxEvent::ProgramMap(_)) {
            saw_program_map = true;
        }
    }
    assert!(
        saw_program_map,
        "demuxer failed to recover from 500-byte garbage prefix"
    );
}

#[test]
fn unrecoverable_after_too_much_garbage() {
    // 188 * 1000 bytes of 0xAA — well past `SYNC_SEARCH_WINDOW`. Demuxer
    // never finds a 0x47 and must surface `DemuxError::Unrecoverable`.
    let huge_garbage = vec![0xAA; 188 * 1000];
    let mut d = Demuxer::new();
    let res = d.feed(&huge_garbage);
    assert!(
        res.is_err(),
        "expected Unrecoverable error on sustained garbage, got Ok"
    );
}

#[test]
fn corrupted_pat_surfaces_psi_checksum_event() {
    let mut bytes = build_clean_stream();
    // Find the first 0x47 PID-0 (PAT) packet and flip a byte inside the
    // PAT section (NOT the post-CRC32 stuffing). Layout for a single-program
    // PAT in a 188-byte TS packet with no adaptation field:
    //
    //   bytes 0..4   TS header (sync + flags + PID + cc)
    //   byte  4      pointer_field (0)
    //   bytes 5..    PAT section: table_id, section_length, transport_stream_id,
    //                version/cur/next, sec_num, last_sec_num, [program_num, PID]+,
    //                CRC32. Total ~21 bytes.
    //   bytes 26..   stuffing (0xFF), post-CRC32 — flipping these would NOT
    //                affect the checksum and the test would silently pass
    //                without exercising the path under test.
    //
    // Offset 10 lands in the `version/current_next_indicator` byte
    // (PAT section offset 5: TS header 0..4 + pointer_field at 4 + section
    // bytes table_id/section_length/transport_stream_id occupy 5..10),
    // inside the CRC-protected region. XOR with 0xFF flips the value but
    // leaves section_length intact so the parser still reaches the
    // checksum step.
    let mut patched = false;
    for i in (0..bytes.len()).step_by(188) {
        if bytes[i] == 0x47 && bytes[i + 1] & 0x1F == 0 && bytes[i + 2] == 0 {
            bytes[i + 10] ^= 0xFF;
            patched = true;
            break;
        }
    }
    assert!(patched, "no PAT packet found in clean stream");

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();

    let mut saw_checksum_issue = false;
    while let Some(e) = d.next_event() {
        if let DemuxEvent::NonConformant { issue, .. } = e {
            if format!("{issue:?}").contains("PsiChecksumMismatch") {
                saw_checksum_issue = true;
            }
        }
    }
    assert!(
        saw_checksum_issue,
        "expected PsiChecksumMismatch NonConformant event from corrupt PAT"
    );
}

/// Sync re-acquisition must verify N-of-M (5 of 7) packet
/// boundaries before declaring sync, not blindly accept any 0x47. Without
/// this check, a random 0x47 byte inside a payload (TS packets routinely
/// carry 0x47 in PES payload, descriptors, etc.) was treated as the start
/// of a new packet, causing false sync and downstream parse errors.
///
/// Discriminator: feed a long buffer of garbage that contains stray 0x47
/// bytes at non-188-stride offsets — without enough 0x47s spaced at 188
/// to confirm a real packet boundary. With the bug, each stray 0x47
/// resets `bytes_since_sync` to 0 and parses 188 bytes as if it were a
/// valid TS packet (parse_ts_packet succeeds because it only requires
/// pkt[0] == 0x47). The parser remains "alive" indefinitely on garbage.
///
/// With the N-of-M fix, none of the stray 0x47s satisfy the
/// "5 of 7 spaced 188 bytes" check, so no false sync is acquired; the
/// parser correctly exhausts SYNC_SEARCH_WINDOW and surfaces
/// `DemuxError::Unrecoverable`.
#[test]
fn sync_reacquisition_strays_at_non_188_stride_do_not_acquire() {
    // 18800 bytes (= 100 stride positions). Every 250 bytes, place a stray
    // 0x47. After accounting for the SYNC_SEARCH_WINDOW = 188 * 32 = 6016
    // bytes, we have enough strays (>= 24) to keep the bug alive.
    //
    // 250 is chosen as the stray stride because lcm(250, 188) = 23500 is
    // larger than our buffer (18800), so within the buffer no two stray
    // 0x47s are 188-aligned with each other — every N-of-M probe from a
    // stray candidate lands on a 0xAA filler byte. Concretely for the
    // candidate at byte 250: checks 250+188=438 (0xAA), 250+376=626 (0xAA),
    // 250+564=814 (0xAA), 250+752=1002 (0xAA), 250+940=1190 (0xAA),
    // 250+1128=1378 (0xAA), 250+1316=1566 (0xAA). All seven slots are
    // 0xAA — 0 of 7 → no sync.
    let mut bytes = vec![0xAAu8; 188 * 100];
    for off in (250..bytes.len()).step_by(250) {
        bytes[off] = 0x47;
    }

    let mut d = Demuxer::new();
    let res = d.feed(&bytes);
    assert!(
        res.is_err(),
        "stray 0x47 bytes at non-188-aligned offsets must NOT acquire sync; \
         expected DemuxError::Unrecoverable past SYNC_SEARCH_WINDOW, got {res:?}"
    );
}

/// Companion N-of-M check — the demuxer MUST still accept legitimate sync when
/// N-of-M aligns. Feed pure garbage prefix then the clean stream; the
/// real stream's 188-aligned 0x47s satisfy the N-of-M check at the very
/// first candidate (7-of-7), so sync is acquired and the stream parses.
#[test]
fn sync_reacquisition_accepts_n_of_m_aligned_stream() {
    // 500 bytes of 0xAA (no 0x47 anywhere — clean prefix), then the real
    // stream. The first 0x47 the demuxer finds is at offset 500 (start of
    // a real TS packet). Checking 0x47 at +188, +376, ... finds 0x47s at
    // every stride within the real stream → ≥5 of 7 → sync acquired.
    let prefix = vec![0xAA; 500];
    let mut bytes = prefix;
    bytes.extend_from_slice(&build_clean_stream());

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    d.flush();

    let mut saw_program_map = false;
    while let Some(e) = d.next_event() {
        if matches!(e, DemuxEvent::ProgramMap(_)) {
            saw_program_map = true;
        }
    }
    assert!(
        saw_program_map,
        "real 188-aligned 0x47 stride must pass N-of-M and acquire sync"
    );
}

/// PUSI handler must NOT discard the section continuation
/// bytes preceding `pointer_field`. Per H.222.0 §2.4.4.1, when PUSI is set
/// and `pointer_field > 0`, bytes `payload[1..1+pointer_field]` complete
/// the prior partial section that started in an earlier packet, and
/// `payload[1+pointer_field..]` begins a NEW section. The bug discarded
/// the continuation bytes, losing the prior section entirely.
///
/// We construct a LARGE PAT (~200 bytes — 47 programs) that genuinely
/// requires two packets to deliver. The first packet (PUSI=1,
/// pointer_field=0) carries the section header + most programs but not
/// the full CRC. The second packet (PUSI=1, pointer_field=N) carries the
/// remaining `N` bytes of the prior PAT as continuation, then starts a
/// NEW (incomplete) section past the pointer_field.
///
/// With the bug: pkt2's pointer_field>0 + PUSI logic discards
/// `payload[1..1+pointer_field]` (the PAT tail) and starts fresh at
/// `payload[1+pointer_field..]`. The first PAT is lost. No ProgramMap.
/// With the fix: pkt2's continuation bytes complete the first PAT,
/// programs are registered, subsequent PMTs route correctly, ProgramMap
/// fires.
#[test]
fn pusi_pointer_field_preserves_prior_section_continuation() {
    use tst_core::mpegts::common::crc32::crc32_mpeg2;
    use tst_core::mpegts::demux::DemuxEvent;

    // Build a LARGE PAT with 47 programs. Each program-loop entry is 4
    // bytes (`program_number` + `reserved+pid`). Header tail: 5 bytes.
    // Total section size: 3 (fixed) + 5 (header tail) + 47*4 + 4 (CRC)
    // = 200 bytes; section_length = 197 = 0x0C5.
    // section_length is 12 bits split across (byte1[3:0], byte2). 197 =
    // 0x0C5 → byte1 low nibble = 0x0, byte2 = 0xC5.
    let mut pat = Vec::with_capacity(200);
    pat.push(0x00); // table_id = PAT
    pat.push(0xB0); // syntax=1, '0', reserved=11, section_length hi nibble = 0
    pat.push(0xC5); // section_length lo = 0xC5
    pat.push(0x00); // transport_stream_id hi
    pat.push(0x01); // transport_stream_id lo = 1
    pat.push(0xC1); // reserved=11, version=0, current_next=1
    pat.push(0x00); // section_number = 0
    pat.push(0x00); // last_section_number = 0
    // 47 program entries:
    //   program 1 → PMT PID 0x100
    //   program 2..47 → arbitrary distinct PIDs 0x101..0x12E (kept under
    //   the 13-bit PID limit and disjoint from 0x100).
    for i in 0..47u16 {
        let prog_num: u16 = i + 1;
        let pmt_pid: u16 = 0x100 + i; // 0x100, 0x101, ..., 0x12E
        pat.push((prog_num >> 8) as u8);
        pat.push(prog_num as u8);
        pat.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
        pat.push(pmt_pid as u8);
    }
    let crc = crc32_mpeg2(&pat);
    pat.extend_from_slice(&crc.to_be_bytes());
    assert_eq!(pat.len(), 200);

    // ── Packet 1: PUSI=1, pointer_field=0. Payload = pointer_field +
    // first 183 bytes of PAT (4 TS header + 1 pointer_field = 5; 188-5
    // = 183 payload bytes for the section). PAT is 200 bytes so 17
    // bytes remain unsent — they must come from packet 2.
    let mut pkt1 = [0u8; 188];
    pkt1[0] = 0x47;
    pkt1[1] = 0x40; // PUSI=1, PID hi=0
    pkt1[2] = 0x00; // PID lo=0
    pkt1[3] = 0x10; // adaptation=01, CC=0
    pkt1[4] = 0x00; // pointer_field=0
    pkt1[5..188].copy_from_slice(&pat[..183]);

    // ── Packet 2: PUSI=1, pointer_field=17, carries:
    //   - bytes [1..18] = last 17 bytes of prior PAT (completing it)
    //   - bytes [18..]  = start of NEW section (PAT v1, partial)
    // Available payload size: 188 - 4 (TS header) - 1 (pointer_field) -
    // 17 (continuation) = 166 bytes for the new section. To keep it
    // incomplete, declare a section_length of 0xC5 (197) again — needs
    // 200 bytes total, so 166 < 200.
    let mut pkt2 = [0xFFu8; 188];
    pkt2[0] = 0x47;
    pkt2[1] = 0x40; // PUSI=1
    pkt2[2] = 0x00;
    pkt2[3] = 0x11; // adaptation=01, CC=1
    pkt2[4] = 17; // pointer_field=17
    pkt2[5..22].copy_from_slice(&pat[183..200]); // 17 PAT continuation bytes
    // New section header at offset 22 — a benign partial PAT-v1 header.
    pkt2[22] = 0x00; // table_id
    pkt2[23] = 0xB0; // syntax=1
    pkt2[24] = 0xC5; // section_length = 197
    // Remaining bytes are stuffing (0xFF) — assembler accumulates them
    // but never reaches 200 total. Section stays partial; no events.

    let mut d = Demuxer::new();
    d.feed(&pkt1).unwrap();
    d.feed(&pkt2).unwrap();
    while d.next_event().is_some() {}

    // ── Packet 3: complete PMT on PID 0x100 (program 1 from the PAT).
    let mut pmt = vec![
        0x02, // table_id = PMT
        0xB0, 0x12, // syntax=1, section_length=0x012
        0x00, 0x01, // program_number=1
        0xC1, // version=0, current_next=1
        0x00, 0x00, // section/last
        0xE1, 0x01, // pcr_pid = 0x101
        0xF0, 0x00, // program_info_length = 0
        0x1B, // stream_type = H.264
        0xE1, 0x01, // elementary_pid = 0x101
        0xF0, 0x00, // es_info_length = 0
    ];
    let pmt_crc = crc32_mpeg2(&pmt);
    pmt.extend_from_slice(&pmt_crc.to_be_bytes());

    let mut pkt3 = [0xFFu8; 188];
    pkt3[0] = 0x47;
    pkt3[1] = 0x41; // PUSI=1, PID hi=1 → PID 0x100
    pkt3[2] = 0x00;
    pkt3[3] = 0x10; // CC=0
    pkt3[4] = 0x00; // pointer_field=0
    pkt3[5..5 + pmt.len()].copy_from_slice(&pmt);
    d.feed(&pkt3).unwrap();

    let mut saw_program_map = false;
    while let Some(e) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = e {
            if pm.pcr_pid == 0x101 {
                saw_program_map = true;
            }
        }
    }
    assert!(
        saw_program_map,
        "PUSI with pointer_field > 0 must process the prior-section continuation \
         bytes — without the fix the prior PAT is lost, PID 0x100 is not \
         registered, and the PMT on PID 0x100 produces no ProgramMap."
    );
}

/// Mid-stream-join scenario. When the demuxer attaches to an in-progress stream
/// and the first PAT packet has `PUSI=1` with `pointer_field > 0` (i.e. carries
/// the tail of some prior PAT section we never saw the start of, then a new
/// section), the `append_continuation` call on the leading bytes silently drops
/// them (no prior PUSI state → §2.4.4.4 mandates discard) and returns
/// `Ok(None)`. Conflating this with "bail" discards the new section at
/// `payload[1+pointer_field..]` too — losing the fresh PAT that the demuxer's
/// PSI dispatch is actively waiting for.
///
/// The fix splits the helper return into `Completed` / `Incomplete` /
/// `Overflowed`. Only `Overflowed` (4 KiB DoS cap) bails. `Incomplete`
/// after step 1 must allow step 2 (start the new section) to run.
///
/// Construction: a single PUSI=1 PAT packet with `pointer_field=20`,
/// where the first 20 bytes are arbitrary garbage (the "prior section
/// tail" we never saw — the assembler will drop them silently because
/// there's no prior PUSI state), followed by a complete in-spec PAT
/// section. With the bug, the PAT is silently lost. With the fix, the
/// PAT is parsed, programs are registered, the follow-up PMT routes
/// correctly, and a `ProgramMap` event fires.
#[test]
fn pusi_pointer_field_mid_stream_join_starts_new_section() {
    use tst_core::mpegts::common::crc32::crc32_mpeg2;
    use tst_core::mpegts::demux::DemuxEvent;

    // Build a minimal in-spec PAT with one program → PMT PID 0x100.
    let mut pat = Vec::with_capacity(20);
    pat.push(0x00); // table_id = PAT
    pat.push(0xB0); // syntax=1, '0', reserved=11, section_length hi nibble = 0
    pat.push(0x0D); // section_length lo = 13 (5 header tail + 4 program entry + 4 CRC)
    pat.push(0x00); // transport_stream_id hi
    pat.push(0x01); // transport_stream_id lo = 1
    pat.push(0xC1); // reserved=11, version=0, current_next=1
    pat.push(0x00); // section_number = 0
    pat.push(0x00); // last_section_number = 0
    pat.push(0x00); // program_number hi
    pat.push(0x01); // program_number lo = 1
    pat.push(0xE1); // reserved=111, PMT PID hi = 0x100 >> 8 = 0x01
    pat.push(0x00); // PMT PID lo = 0x00
    let crc = crc32_mpeg2(&pat);
    pat.extend_from_slice(&crc.to_be_bytes());
    assert_eq!(pat.len(), 16); // 3 fixed + 13 section_length

    // Single TS packet, PUSI=1, pointer_field=20. Bytes 5..25 are 20 bytes
    // of garbage (the "prior partial section tail"); bytes 25..(25+pat.len())
    // are the real PAT. Remainder is 0xFF stuffing.
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40; // PUSI=1, PID hi=0
    pkt[2] = 0x00; // PID lo=0 → PAT
    pkt[3] = 0x10; // adaptation=01, CC=0
    pkt[4] = 20; // pointer_field=20
    // Bytes [5..25] = garbage continuation that the assembler will drop
    // silently (no prior PUSI on PID 0). Use 0xAA to avoid colliding with
    // legitimate table IDs.
    for b in &mut pkt[5..25] {
        *b = 0xAA;
    }
    // Bytes [25..25+16] = the real PAT section.
    pkt[25..25 + pat.len()].copy_from_slice(&pat);

    let mut d = Demuxer::new();
    d.feed(&pkt).unwrap();

    // Now send a complete PMT on PID 0x100 — only routes correctly if
    // the PAT above was actually parsed and registered PMT PID 0x100.
    let mut pmt = vec![
        0x02, // table_id = PMT
        0xB0, 0x12, // syntax=1, section_length=0x012
        0x00, 0x01, // program_number=1
        0xC1, // version=0, current_next=1
        0x00, 0x00, // section/last
        0xE1, 0x01, // pcr_pid = 0x101
        0xF0, 0x00, // program_info_length = 0
        0x1B, // stream_type = H.264
        0xE1, 0x01, // elementary_pid = 0x101
        0xF0, 0x00, // es_info_length = 0
    ];
    let pmt_crc = crc32_mpeg2(&pmt);
    pmt.extend_from_slice(&pmt_crc.to_be_bytes());

    let mut pmt_pkt = [0xFFu8; 188];
    pmt_pkt[0] = 0x47;
    pmt_pkt[1] = 0x41; // PUSI=1, PID hi=1 → PID 0x100
    pmt_pkt[2] = 0x00;
    pmt_pkt[3] = 0x10; // adaptation=01, CC=0
    pmt_pkt[4] = 0x00; // pointer_field=0
    pmt_pkt[5..5 + pmt.len()].copy_from_slice(&pmt);
    d.feed(&pmt_pkt).unwrap();

    let mut saw_program_map = false;
    while let Some(e) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = e {
            if pm.pcr_pid == 0x101 {
                saw_program_map = true;
            }
        }
    }
    assert!(
        saw_program_map,
        "mid-stream join: PUSI with pointer_field > 0 and no prior PUSI on this PID \
         must still START the new section at payload[1+pointer_field..]. Pre-fix \
         code bailed after the silent-drop continuation and discarded the new \
         section header, leaving the PMT on PID 0x100 unroutable."
    );
}

#[test]
fn cc_jump_emits_discontinuity() {
    let mut bytes = build_clean_stream();
    // Find the SECOND packet on a non-PSI PID and bump its CC by 5. The
    // demuxer's `check_continuity` only fires `ContinuityJump` if there's
    // a prior CC value cached for the PID — bumping the first packet on a
    // PID is silently ignored. We also need the PMT to have been parsed
    // before the bumped packet arrives (so `lookup_stream` resolves);
    // the PMT precedes any video packet in our clean stream, so finding
    // the second video packet by PID-seen tracking covers both
    // preconditions in one walk.
    //
    // CC lives in the low nibble of TS header byte 3.
    use std::collections::HashSet;
    let mut seen_pids: HashSet<u16> = HashSet::new();
    let mut patched = false;
    for i in (0..bytes.len()).step_by(188) {
        if bytes[i] != 0x47 {
            continue;
        }
        let pid = ((u16::from(bytes[i + 1] & 0x1F)) << 8) | u16::from(bytes[i + 2]);
        if pid == 0x0000 || pid == 0x1000 {
            // PAT or PMT — skip.
            continue;
        }
        if seen_pids.contains(&pid) {
            bytes[i + 3] = (bytes[i + 3] & 0xF0) | (((bytes[i + 3] & 0x0F) + 5) & 0x0F);
            patched = true;
            break;
        }
        seen_pids.insert(pid);
    }
    assert!(patched, "no second non-PSI packet found in clean stream");

    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();

    let mut saw_jump = false;
    while let Some(e) = d.next_event() {
        if matches!(
            e,
            DemuxEvent::Discontinuity {
                kind: DiscontinuityKind::ContinuityJump { .. },
                ..
            }
        ) {
            saw_jump = true;
        }
    }
    assert!(
        saw_jump,
        "expected ContinuityJump discontinuity from CC bump"
    );
}

// ---- H.222.0 §2.4.3.3 spec-legal duplicate regression tests ----

/// Collect per-AU raw bytes from all video Sample events in the demuxer's
/// event queue. The demuxer surfaces video as raw-first `SamplePayload::Video
/// { raw, .. }`. Used to byte-compare two demux runs.
fn collect_video_raw_bytes(d: &mut Demuxer) -> Vec<Vec<u8>> {
    let mut aus = Vec::new();
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = ev
        {
            aus.push(raw.as_slice().to_vec());
        }
    }
    aus
}

/// Find the Nth video TS packet (PID 0x100) in `stream` and insert `count`
/// additional copies directly after it. Returns the patched stream.
///
/// The video PID 0x100 is hard-coded to match `build_clean_stream`'s config.
fn insert_video_packet_duplicates(stream: &[u8], nth: usize, count: usize) -> Vec<u8> {
    let mut seen = 0usize;
    let mut out = Vec::with_capacity(stream.len() + count * 188);
    let mut i = 0;
    while i + 188 <= stream.len() {
        let pkt = &stream[i..i + 188];
        out.extend_from_slice(pkt);
        if pkt[0] == 0x47 {
            let pid = ((u16::from(pkt[1] & 0x1F)) << 8) | u16::from(pkt[2]);
            if pid == 0x0100 {
                seen += 1;
                if seen == nth {
                    for _ in 0..count {
                        out.extend_from_slice(pkt);
                    }
                }
            }
        }
        i += 188;
    }
    out.extend_from_slice(&stream[i..]);
    out
}

/// Find the first PMT TS packet (PID 0x1000) in `stream` and insert one copy
/// directly after it. Returns the patched stream.
fn insert_pmt_packet_duplicate(stream: &[u8]) -> Vec<u8> {
    let mut inserted = false;
    let mut out = Vec::with_capacity(stream.len() + 188);
    let mut i = 0;
    while i + 188 <= stream.len() {
        let pkt = &stream[i..i + 188];
        out.extend_from_slice(pkt);
        if !inserted && pkt[0] == 0x47 {
            let pid = ((u16::from(pkt[1] & 0x1F)) << 8) | u16::from(pkt[2]);
            if pid == 0x1000 {
                out.extend_from_slice(pkt);
                inserted = true;
            }
        }
        i += 188;
    }
    out.extend_from_slice(&stream[i..]);
    out
}

/// (a): a spec-legal duplicate TS packet (same CC, no
/// `discontinuity_indicator`) is suppressed — the reassembled ES byte
/// content is identical to a clean stream without the duplicate.
#[test]
fn cc_duplicate_suppressed_es_bytes_identical() {
    let clean = build_clean_stream();

    // Collect ES content from the clean stream as the expected reference.
    let mut d_clean = Demuxer::new();
    d_clean.feed(&clean).unwrap();
    d_clean.flush();
    let expected_aus = collect_video_raw_bytes(&mut d_clean);
    assert!(
        !expected_aus.is_empty(),
        "clean stream must yield video AUs"
    );

    // Patch: insert one duplicate of the 2nd video packet (2nd so cc_by_pid
    // already has a prior value, matching the live-stream duplicate case).
    let patched = insert_video_packet_duplicates(&clean, 2, 1);
    assert!(
        patched.len() > clean.len(),
        "patched stream must be longer by one packet"
    );

    let mut d_dup = Demuxer::new();
    d_dup.feed(&patched).unwrap();
    d_dup.flush();

    // No ContinuityJump — the duplicate must be silently suppressed.
    let mut saw_jump = false;
    let mut dup_aus = Vec::new();
    while let Some(ev) = d_dup.next_event() {
        if matches!(
            ev,
            DemuxEvent::Discontinuity {
                kind: DiscontinuityKind::ContinuityJump { .. },
                ..
            }
        ) {
            saw_jump = true;
        }
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = ev
        {
            dup_aus.push(raw.as_slice().to_vec());
        }
    }

    assert!(
        !saw_jump,
        "spec-legal first duplicate must not emit ContinuityJump"
    );
    assert_eq!(
        expected_aus, dup_aus,
        "spec-legal CC duplicate changed the reassembled ES output"
    );
}

/// (b): a spec-legal duplicate PMT packet is suppressed — the PSI
/// reassembler receives the section exactly once and delivers a `ProgramMap`.
/// No `PsiCcDiscontinuity` event is emitted.
#[test]
fn cc_duplicate_on_pmt_psi_survives() {
    let clean = build_clean_stream();
    let patched = insert_pmt_packet_duplicate(&clean);

    let mut d = Demuxer::new();
    d.feed(&patched).unwrap();
    d.flush();

    let mut saw_program_map = false;
    let mut saw_psi_cc_discontinuity = false;
    while let Some(ev) = d.next_event() {
        if matches!(ev, DemuxEvent::ProgramMap(_)) {
            saw_program_map = true;
        }
        // A PSI CC jump surfaces as NonConformant::PsiCcDiscontinuity, not as
        // a Discontinuity event (PSI PIDs are not registered in stream_kind_by_pid
        // so lookup_stream returns None for them, routing through psi_topology
        // instead of record_discontinuity).
        if matches!(
            ev,
            DemuxEvent::NonConformant {
                issue: NonConformantIssue::PsiCcDiscontinuity { .. },
                ..
            }
        ) {
            saw_psi_cc_discontinuity = true;
        }
    }

    assert!(
        saw_program_map,
        "PMT duplicate must not prevent ProgramMap delivery"
    );
    assert!(
        !saw_psi_cc_discontinuity,
        "PMT spec-legal duplicate must not emit PsiCcDiscontinuity"
    );
}

/// (c): `StrictMode::Full` does not reject a stream that contains
/// exactly one spec-legal duplicate. Duplicates are suppressed before any
/// strict evaluation path.
#[test]
fn cc_single_duplicate_accepted_by_strict_full() {
    let clean = build_clean_stream();
    let patched = insert_video_packet_duplicates(&clean, 2, 1);

    let mut d = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    // With one conformant duplicate, no StrictRejection should be returned.
    let res = d.feed(&patched);
    assert!(
        res.is_ok(),
        "StrictMode::Full must accept a stream with one spec-legal CC duplicate, got: {res:?}"
    );
}

/// Count `ContinuityJump` discontinuity events in the demuxer's queue.
fn count_continuity_jumps(d: &mut Demuxer) -> usize {
    let mut jumps = 0;
    while let Some(ev) = d.next_event() {
        if matches!(
            ev,
            DemuxEvent::Discontinuity {
                kind: DiscontinuityKind::ContinuityJump { .. },
                ..
            }
        ) {
            jumps += 1;
        }
    }
    jumps
}

/// (d): a THIRD packet with the same CC (the "only-two" rule
/// violation per H.222.0 §2.4.3.3) is treated as a real discontinuity and
/// surfaces EXACTLY ONE `ContinuityJump` event, not one from the only-two
/// branch and another from the generic CC-jump check.
#[test]
fn cc_third_same_cc_fires_discontinuity_exactly_once() {
    let clean = build_clean_stream();
    // Insert TWO extra copies of the 2nd video packet → three total with the
    // original. First extra = allowed duplicate (suppressed). Second extra =
    // third occurrence of the same CC → exactly one real discontinuity.
    let patched = insert_video_packet_duplicates(&clean, 2, 2);

    let mut d = Demuxer::new();
    d.feed(&patched).unwrap();
    d.flush();

    assert_eq!(
        count_continuity_jumps(&mut d),
        1,
        "third packet with same CC must emit exactly one ContinuityJump"
    );
}

/// (f): the only-two enforcement does not reset mid-run — in a
/// FOUR-in-a-row identical same-CC run, the 3rd and 4th packets each fire
/// one `ContinuityJump` (the 4th must NOT be silently re-suppressed as a
/// fresh "first duplicate").
#[test]
fn cc_fourth_same_cc_keeps_firing_discontinuities() {
    let clean = build_clean_stream();
    // THREE extra copies → four total: original routed, 1st extra suppressed,
    // 2nd + 3rd extras each surface one discontinuity.
    let patched = insert_video_packet_duplicates(&clean, 2, 3);

    let mut d = Demuxer::new();
    d.feed(&patched).unwrap();
    d.flush();

    assert_eq!(
        count_continuity_jumps(&mut d),
        2,
        "3rd and 4th identical same-CC packets must each fire one ContinuityJump"
    );
}

/// Find the Nth video TS packet (PID 0x100) in `stream` and insert one copy
/// with the SAME header (incl. CC) but a corrupted final payload byte.
/// Models the non-conformant same-CC-different-bytes case that must NOT be
/// classified as a duplicate.
fn insert_video_packet_same_cc_different_payload(stream: &[u8], nth: usize) -> Vec<u8> {
    let mut seen = 0usize;
    let mut out = Vec::with_capacity(stream.len() + 188);
    let mut i = 0;
    while i + 188 <= stream.len() {
        let pkt = &stream[i..i + 188];
        out.extend_from_slice(pkt);
        if pkt[0] == 0x47 {
            let pid = ((u16::from(pkt[1] & 0x1F)) << 8) | u16::from(pkt[2]);
            if pid == 0x0100 {
                seen += 1;
                if seen == nth {
                    let mut modified = [0u8; 188];
                    modified.copy_from_slice(pkt);
                    modified[187] ^= 0xFF; // corrupt one payload byte
                    out.extend_from_slice(&modified);
                }
            }
        }
        i += 188;
    }
    out.extend_from_slice(&stream[i..]);
    out
}

/// (e): a same-CC packet whose bytes DIFFER from its predecessor
/// is NOT a spec-legal duplicate (H.222.0 §2.4.3.3 requires duplicates to be
/// bit-identical apart from a refreshed PCR). It must be routed like any
/// other packet — surfacing a `ContinuityJump` — so differing data is never
/// silently swallowed; CC-only detection mis-suppresses the
/// malformed-pes-lenient scenario's deliberately-different packet.
#[test]
fn cc_same_cc_different_payload_not_suppressed() {
    let clean = build_clean_stream();

    let mut d_clean = Demuxer::new();
    d_clean.feed(&clean).unwrap();
    d_clean.flush();
    let expected_aus = collect_video_raw_bytes(&mut d_clean);
    let expected_total: usize = expected_aus.iter().map(Vec::len).sum();

    let patched = insert_video_packet_same_cc_different_payload(&clean, 2);
    let mut d = Demuxer::new();
    d.feed(&patched).unwrap();
    d.flush();

    let mut saw_jump = false;
    let mut aus = Vec::new();
    while let Some(ev) = d.next_event() {
        if matches!(
            ev,
            DemuxEvent::Discontinuity {
                kind: DiscontinuityKind::ContinuityJump { .. },
                ..
            }
        ) {
            saw_jump = true;
        }
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = ev
        {
            aus.push(raw.as_slice().to_vec());
        }
    }

    assert!(
        saw_jump,
        "same-CC different-payload packet must surface ContinuityJump, not be suppressed"
    );
    let total: usize = aus.iter().map(Vec::len).sum();
    assert!(
        total > expected_total,
        "the differing packet's payload must be routed (old lenient behavior), \
         got {total} bytes vs clean {expected_total}"
    );
}
