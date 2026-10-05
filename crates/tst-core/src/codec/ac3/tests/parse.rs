//! Unit tests for `codec::ac3::parse_syncframe`.

use crate::codec::CodecParseError;
use crate::codec::ac3::{Ac3SyncInfo, parse_syncframe};

/// Helper: build the first 8 bytes of an AC-3 syncframe with caller-chosen
/// fscod, frmsizecod, bsid, bsmod, acmod, lfeon. Optional fields driven
/// by acmod (cmixlev, surmixlev, dsurmod) are filled with zeros.
fn build_syncinfo(
    fscod: u8,
    frmsizecod: u8,
    bsid: u8,
    bsmod: u8,
    acmod: u8,
    lfeon: bool,
) -> Vec<u8> {
    let mut h = vec![0u8; 8];
    // bytes[0..2] = 0x0B77
    h[0] = 0x0B;
    h[1] = 0x77;
    // bytes[2..4] = crc1 (don't care, leave 0)
    // bytes[4] = fscod(2) + frmsizecod(6)
    h[4] = ((fscod & 0b11) << 6) | (frmsizecod & 0b0011_1111);
    // bytes[5] = bsid(5) + bsmod(3)
    h[5] = ((bsid & 0b1_1111) << 3) | (bsmod & 0b0000_0111);
    // bytes[6..]: acmod(3) + optional(cmixlev/surmixlev/dsurmod) + lfeon(1)
    // We pack bits MSB-first. Compute total bits used; lfeon is the last.
    let mut bit_pos: u32 = 0;
    let mut buf = [0u8; 2]; // 16 bits suffices (max 8 bits before lfeon)
    let mut emit = |val: u32, n: u32| {
        // MSB-first emission.
        for i in (0..n).rev() {
            let bit = ((val >> i) & 1) as u8;
            if bit == 1 {
                buf[(bit_pos / 8) as usize] |= 1 << (7 - (bit_pos % 8));
            }
            bit_pos += 1;
        }
    };
    emit(acmod as u32, 3);
    if (acmod & 0x1) != 0 && acmod != 0x1 {
        emit(0, 2); // cmixlev
    }
    if (acmod & 0x4) != 0 {
        emit(0, 2); // surmixlev
    }
    if acmod == 0x2 {
        emit(0, 2); // dsurmod
    }
    emit(if lfeon { 1 } else { 0 }, 1);
    h[6] = buf[0];
    h[7] = buf[1];
    h
}

#[test]
fn parse_48khz_stereo_192kbps_bsid8() {
    // fscod=0 (48kHz), frmsizecod=20 (192 kbps, words=384, frame_len=768),
    // bsid=8, bsmod=0 (CM), acmod=2 (2/0 stereo), lfeon=false.
    let h = build_syncinfo(0, 20, 8, 0, 2, false);
    let info = parse_syncframe(&h).unwrap();
    assert_eq!(info.fscod, 0);
    assert_eq!(info.frmsizecod, 20);
    assert_eq!(info.bsid, 8);
    assert_eq!(info.bsmod, 0);
    assert_eq!(info.acmod, 2);
    assert!(!info.lfeon);
    assert_eq!(info.sample_rate_hz, 48_000);
    assert_eq!(info.bit_rate_kbps, 192);
    assert_eq!(info.frame_length_bytes, 768); // 384 words * 2
    assert_eq!(info.num_full_bandwidth_channels, 2);
}

#[test]
fn parse_44_1khz_stereo_128kbps_uses_odd_table_row() {
    // fscod=1 (44.1kHz), frmsizecod=17 (128 kbps, words=279 odd row).
    let h = build_syncinfo(1, 17, 8, 0, 2, false);
    let info = parse_syncframe(&h).unwrap();
    assert_eq!(info.sample_rate_hz, 44_100);
    assert_eq!(info.bit_rate_kbps, 128);
    assert_eq!(info.frame_length_bytes, 279 * 2); // 558 bytes
}

#[test]
fn parse_32khz_5_1_with_lfe() {
    // fscod=2 (32kHz), frmsizecod=24 (256 kbps, words=768),
    // acmod=7 (3/2), lfeon=true → "5.1" channel layout.
    let h = build_syncinfo(2, 24, 8, 0, 7, true);
    let info = parse_syncframe(&h).unwrap();
    assert_eq!(info.sample_rate_hz, 32_000);
    assert_eq!(info.bit_rate_kbps, 256);
    assert_eq!(info.acmod, 7);
    assert!(info.lfeon);
    assert_eq!(info.num_full_bandwidth_channels, 5);
}

#[test]
fn parse_bad_sync_word_returns_bad_sync() {
    let mut h = build_syncinfo(0, 20, 8, 0, 2, false);
    h[0] = 0xAB;
    h[1] = 0xCD;
    let err = parse_syncframe(&h).unwrap_err();
    assert!(matches!(
        err,
        CodecParseError::BadSyncWord {
            expected: 0x0B77,
            found: 0xABCD
        }
    ));
}

#[test]
fn parse_too_short_returns_truncated() {
    let err = parse_syncframe(&[0x0B, 0x77]).unwrap_err();
    assert!(matches!(
        err,
        CodecParseError::Truncated { needed: 6, had: 2 }
    ));
}

#[test]
fn parse_reserved_fscod_3_returns_forbidden() {
    // fscod=3 is reserved per A/52 Table 5.6 — must be rejected.
    let h = build_syncinfo(3, 20, 8, 0, 2, false);
    let err = parse_syncframe(&h).unwrap_err();
    assert!(matches!(
        err,
        CodecParseError::Forbidden {
            field: "ac3_fscod_reserved"
        }
    ));
}

#[test]
fn parse_reserved_frmsizecod_38_returns_reserved_value() {
    // frmsizecod 38..=63 are reserved per A/52 Table 5.18.
    let h = build_syncinfo(0, 38, 8, 0, 2, false);
    let err = parse_syncframe(&h).unwrap_err();
    assert!(matches!(
        err,
        CodecParseError::ReservedValue {
            field: "ac3_frmsizecod",
            value: 38
        }
    ));
}

#[test]
fn parse_eac3_bsid_16_returns_unsupported_profile() {
    // bsid=16 = E-AC-3 (Annex E), explicitly rejected.
    let h = build_syncinfo(0, 20, 16, 0, 2, false);
    let err = parse_syncframe(&h).unwrap_err();
    assert!(matches!(
        err,
        CodecParseError::UnsupportedProfile { profile_idc: 16 }
    ));
}

#[test]
fn parse_returned_struct_is_pub_struct() {
    // Smoke test: Ac3SyncInfo is public, fields readable.
    let h = build_syncinfo(0, 20, 8, 1, 2, false);
    let info: Ac3SyncInfo = parse_syncframe(&h).unwrap();
    assert_eq!(info.bsmod, 1);
}

#[test]
fn parse_eac3_bsid_16_with_invalid_fscod_returns_unsupported_profile_not_forbidden() {
    // bsid must be read and classified BEFORE fscod/frmsizecod are
    // validated. An E-AC-3 frame (bsid=16) whose fscod bits happen to equal
    // 0b11 (reserved for AC-3) must still return UnsupportedProfile, not
    // Forbidden — the AC-3 field constraints are irrelevant once we know
    // the bitstream is E-AC-3.
    //
    // fscod=3 (reserved per A/52 Table 5.6), frmsizecod=20 (valid), bsid=16.
    let h = build_syncinfo(3, 20, 16, 0, 2, false);
    let err = parse_syncframe(&h).unwrap_err();
    assert!(
        matches!(err, CodecParseError::UnsupportedProfile { profile_idc: 16 }),
        "expected UnsupportedProfile(16), got {err:?}"
    );
}

#[test]
fn parse_eac3_bsid_11_with_reserved_frmsizecod_returns_unsupported_profile_not_reserved() {
    // Companion to the fscod case — frmsizecod validation also must
    // not run before the bsid check. bsid=11 (E-AC-3 range 11..=16),
    // frmsizecod=38 (reserved per A/52 Table 5.18).
    let h = build_syncinfo(0, 38, 11, 0, 2, false);
    let err = parse_syncframe(&h).unwrap_err();
    assert!(
        matches!(err, CodecParseError::UnsupportedProfile { profile_idc: 11 }),
        "expected UnsupportedProfile(11), got {err:?}"
    );
}
