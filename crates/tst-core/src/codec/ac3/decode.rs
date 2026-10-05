//! AC-3 syncframe decode helpers.
//!
//! Spec: ATSC A/52:2018 §5.4 (syncinfo + bsi prefix).

use crate::codec::CodecParseError;

/// AC-3 syncframe sync word per A/52 §5.4.1.1 — must be 0x0B77.
pub(crate) const AC3_SYNC_WORD: u16 = 0x0B77;

/// Decoded syncinfo + first-few-bsi-fields of an AC-3 syncframe.
///
/// All fields are derived from the first ~6 bytes of the syncframe per
/// ATSC A/52:2018 §5.4.1 + §5.4.2. The deeper bsi fields (dialnorm,
/// compr, langcod, audprodie, addbsi) are skipped — the muxer's
/// AC-3_audio_stream_descriptor (Table A4.1) needs only the fields
/// surfaced here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ac3SyncInfo {
    /// `fscod` — 2-bit sample rate code per A/52 Table 5.6.
    /// 0=48kHz, 1=44.1kHz, 2=32kHz, 3=reserved (rejected as `Forbidden`).
    pub fscod: u8,
    /// `frmsizecod` — 6-bit frame size code per A/52 Table 5.18.
    /// Combines with `fscod` to determine `frame_length_bytes` and
    /// nominal bit rate. Values 0..=37 are defined; 38..=63 are reserved
    /// (rejected as `ReservedValue`).
    pub frmsizecod: u8,
    /// `bsid` — 5-bit bitstream identification per A/52 §5.4.2.1.
    /// `8` is the canonical value for AC-3 conformant to A/52:2018.
    /// Values `0..=8` are AC-3; `9` and `10` are alternative bitstreams;
    /// `11..=16` are E-AC-3 per Annex E. This parser rejects `>= 9` as
    /// `UnsupportedProfile` — E-AC-3 needs a separate decoder.
    pub bsid: u8,
    /// `bsmod` — 3-bit bitstream mode (service type) per A/52 Table 5.7.
    /// 0=CM (complete main), 1=ME (music+effects), 7=VO (voiceover).
    pub bsmod: u8,
    /// `acmod` — 3-bit audio coding mode per A/52 Table 5.8.
    /// 1=1/0 mono, 2=2/0 stereo (most common), 7=3/2 surround. 0 is the
    /// 1+1 dual-mono mode and carries different downstream semantics.
    pub acmod: u8,
    /// `lfeon` — 1-bit LFE channel present flag per A/52 §5.4.2.7.
    /// 1=LFE present (the ".1" in e.g. "5.1 surround").
    pub lfeon: bool,
    /// Decoded sample rate in Hz (48000, 44100, 32000).
    pub sample_rate_hz: u32,
    /// Nominal bit rate in kbps per A/52 Table 5.18 (`frmsizecod >> 1`
    /// indexes the bit rate table).
    pub bit_rate_kbps: u32,
    /// Total syncframe length in bytes (header + body + crc2). Derived
    /// from `(fscod, frmsizecod)` per A/52 Table 5.18; each table entry
    /// is in 16-bit words, so this value is `2 * words`.
    pub frame_length_bytes: u32,
    /// Number of full-bandwidth channels encoded in this acmod
    /// (excluding the LFE channel). Derived from acmod via A/52
    /// Table 5.8: 0→2 (1+1 dual mono), 1→1, 2→2, 3→3, 4→3, 5→4, 6→4, 7→5.
    pub num_full_bandwidth_channels: u8,
}

/// Parse an AC-3 syncframe header from `bytes`.
///
/// Reads the syncinfo (5 bytes) + the first few bsi fields (up to ~3
/// more bytes depending on acmod). Returns an [`Ac3SyncInfo`] on
/// success; does NOT validate the syncframe body or CRCs (consumers
/// can do that separately if needed).
///
/// # Errors
///
/// - [`CodecParseError::Truncated`] when `bytes` is shorter than the
///   header needs (typically 6 bytes; up to 8 with all optional
///   acmod-dependent fields present).
/// - [`CodecParseError::BadSyncWord`] when the first 16 bits are not
///   `0x0B77`.
/// - [`CodecParseError::Forbidden`] when `fscod == 3` (reserved sample
///   rate per A/52 Table 5.6); applies only to AC-3 frames (`bsid ≤ 8`).
/// - [`CodecParseError::ReservedValue`] when `frmsizecod > 37`
///   (reserved frame-size code per A/52 Table 5.18); applies only to
///   AC-3 frames (`bsid ≤ 8`).
/// - [`CodecParseError::UnsupportedProfile`] when `bsid >= 9`
///   (E-AC-3 or alternative bitstream — needs a separate parser).
pub fn parse_syncframe(bytes: &[u8]) -> Result<Ac3SyncInfo, CodecParseError> {
    // Minimum-bytes check up-front: syncinfo (5 bytes) + 1 byte for bsid+bsmod
    // + 1 byte for acmod + optional bits.
    if bytes.len() < 6 {
        return Err(CodecParseError::Truncated {
            needed: 6,
            had: bytes.len() as u32,
        });
    }

    let sync = ((bytes[0] as u16) << 8) | (bytes[1] as u16);
    if sync != AC3_SYNC_WORD {
        return Err(CodecParseError::BadSyncWord {
            expected: AC3_SYNC_WORD,
            found: sync,
        });
    }

    // bytes[2..4] = crc1 (16 bits) — not validated here.

    // bytes[5]: bsid(5 bits MSB) + bsmod(3 bits LSB).
    // Read bsid BEFORE validating fscod/frmsizecod: an E-AC-3 frame (bsid
    // 11..=16) may have fscod/frmsizecod values that are illegal in AC-3 but
    // valid in E-AC-3's different bitstream syntax. Classifying such a frame
    // as Forbidden or ReservedValue (from the AC-3 field constraints) instead
    // of UnsupportedProfile would be a diagnostic misclassification —
    // ATSC A/52 §5.4.2.1 establishes bsid as the authoritative bitstream-type
    // indicator.
    let bsid = (bytes[5] >> 3) & 0b1_1111;
    let bsmod = bytes[5] & 0b0000_0111;
    if bsid >= 9 {
        // bsid 16 is Annex E (E-AC-3); bsid 11..=15 are the reserved backward-compatible
        // range; bsid 9 and 10 are alternative bitstreams. All are rejected here.
        return Err(CodecParseError::UnsupportedProfile { profile_idc: bsid });
    }

    // bytes[4]: fscod(2 bits MSB) + frmsizecod(6 bits LSB).
    // Validated after bsid so that E-AC-3 frames are classified by bitstream
    // type rather than by AC-3-specific field constraints.
    let fscod = (bytes[4] >> 6) & 0b11;
    let frmsizecod = bytes[4] & 0b0011_1111;

    let sample_rate_hz = match fscod {
        0 => 48_000,
        1 => 44_100,
        2 => 32_000,
        _ => {
            return Err(CodecParseError::Forbidden {
                field: "ac3_fscod_reserved",
            });
        }
    };

    let (frame_length_bytes, bit_rate_kbps) = frame_size_lookup(fscod, frmsizecod)?;

    // Remaining fields (acmod, optional mixlev fields, dsurmod, lfeon)
    // are bit-packed past byte 6. AC-3 has no emulation-prevention bytes
    // (unlike H.264/265 RBSP), so we use a plain bit cursor — the
    // workspace `codec::bitreader::BitReader` would incorrectly skip
    // `00 00 03` triples that happen to occur in AC-3 bsi data.
    //
    // Layout per A/52 §5.4.2.3..§5.4.2.7 (after bsmod):
    //   acmod          3 bits
    //   if ((acmod & 0x1) && (acmod != 0x1))   cmixlev   2 bits  -- 3 front channels
    //   if (acmod & 0x4)                       surmixlev 2 bits  -- surround channel exists
    //   if (acmod == 0x2)                      dsurmod   2 bits  -- 2/0 mode
    //   lfeon          1 bit
    //
    // Worst case before lfeon: 3 + 2 + 2 + 1 = 8 bits — fits in byte 6.
    // (acmod=2 has cmixlev=NO + surmixlev=NO + dsurmod=YES so 3+2+1=6 bits;
    // acmod=7 (3/2) has cmixlev=YES + surmixlev=YES so 3+2+2+1=8 bits.)
    let mut cursor = Ac3BitCursor::new(&bytes[6..]);
    let trunc = || CodecParseError::Truncated {
        needed: 7,
        had: bytes.len() as u32,
    };
    let acmod = cursor.read_bits(3).ok_or_else(trunc)? as u8;
    // Discard cmixlev when (acmod & 0x1) && (acmod != 0x1) — i.e., 3 front
    // channels: acmod ∈ {3, 5, 7}.
    if (acmod & 0x1) != 0 && acmod != 0x1 {
        cursor.read_bits(2).ok_or_else(trunc)?;
    }
    // Discard surmixlev when acmod has surround channels — acmod ∈ {4,5,6,7}.
    if (acmod & 0x4) != 0 {
        cursor.read_bits(2).ok_or_else(trunc)?;
    }
    // Discard dsurmod when acmod == 2 (2/0 stereo).
    if acmod == 0x2 {
        cursor.read_bits(2).ok_or_else(trunc)?;
    }
    let lfeon = cursor.read_bits(1).ok_or_else(trunc)? != 0;

    let num_full_bandwidth_channels = match acmod {
        0 => 2, // 1+1 dual mono — two independent mono streams
        1 => 1, // 1/0 mono
        2 => 2, // 2/0 stereo
        3 => 3, // 3/0 L,C,R
        4 => 3, // 2/1 L,R,S
        5 => 4, // 3/1 L,C,R,S
        6 => 4, // 2/2 L,R,SL,SR
        7 => 5, // 3/2 L,C,R,SL,SR
        _ => unreachable!("acmod is 3 bits, masked to 0..=7"),
    };

    Ok(Ac3SyncInfo {
        fscod,
        frmsizecod,
        bsid,
        bsmod,
        acmod,
        lfeon,
        sample_rate_hz,
        bit_rate_kbps,
        frame_length_bytes,
        num_full_bandwidth_channels,
    })
}

/// Lookup `(frame_length_bytes, bit_rate_kbps)` from `(fscod, frmsizecod)`
/// per A/52 Table 5.18. The table is indexed by `frmsizecod >> 1` (bit
/// rates) and `fscod` (sample rates); the LSB of `frmsizecod` only
/// matters for 44.1 kHz (because some 44.1 kHz frame sizes aren't whole
/// 16-bit words — see the table's odd entries).
fn frame_size_lookup(fscod: u8, frmsizecod: u8) -> Result<(u32, u32), CodecParseError> {
    if frmsizecod > 37 {
        return Err(CodecParseError::ReservedValue {
            field: "ac3_frmsizecod",
            value: frmsizecod as u32,
        });
    }
    // bit_rate_kbps indexed by frmsizecod >> 1 (0..=18).
    const BIT_RATES_KBPS: [u32; 19] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ];
    let bit_rate_kbps = BIT_RATES_KBPS[(frmsizecod >> 1) as usize];

    // words_per_syncframe[fscod][frmsizecod] per A/52 Table 5.18.
    // 38 rows (frmsizecod 0..=37), 3 columns (fs = 32, 44.1, 48 kHz).
    // Indexed by fscod: 0=48, 1=44.1, 2=32 — order matches the column
    // headers; we need fscod-to-column mapping.
    const WORDS_48KHZ: [u32; 38] = [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256, 320,
        320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896, 896, 1024, 1024, 1152, 1152,
        1280, 1280,
    ];
    const WORDS_44_1KHZ: [u32; 38] = [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 279, 348,
        349, 417, 418, 487, 488, 557, 558, 696, 697, 835, 836, 975, 976, 1114, 1115, 1253, 1254,
        1393, 1394,
    ];
    const WORDS_32KHZ: [u32; 38] = [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384, 384,
        480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152, 1152, 1344, 1344, 1536, 1536, 1728,
        1728, 1920, 1920,
    ];

    let words = match fscod {
        0 => WORDS_48KHZ[frmsizecod as usize],
        1 => WORDS_44_1KHZ[frmsizecod as usize],
        2 => WORDS_32KHZ[frmsizecod as usize],
        _ => unreachable!("fscod 3 was caught above as Forbidden"),
    };

    // Each word is 16 bits = 2 bytes.
    let frame_length_bytes = words * 2;
    Ok((frame_length_bytes, bit_rate_kbps))
}

/// Minimal MSB-first bit cursor for AC-3 bsi reads. Distinct from
/// [`crate::codec::bitreader::BitReader`] which skips `00 00 03`
/// emulation-prevention bytes — a video-RBSP convention that does not
/// apply to AC-3 audio bitstreams.
struct Ac3BitCursor<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> Ac3BitCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    /// Read `n` bits (n ≤ 16). Returns `None` when running past `bytes.len()`.
    fn read_bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 16, "Ac3BitCursor::read_bits caps at 16");
        let mut acc = 0u32;
        for _ in 0..n {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            let byte = *self.bytes.get(byte_idx)?;
            let bit = (byte >> bit_idx) & 0x1;
            acc = (acc << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Some(acc)
    }
}
