//! Shared types for `mpegts::mux` and `mpegts::demux`.
//!
//! Concrete types only — no traits, no generics.

pub mod crc32;
pub(crate) mod handle_pack;

/// MPEG-TS PMT `stream_type` values used by this library.
///
/// Single enum covers both directions: mux looks up by codec / KLV mode,
/// demux looks up by parsed byte. Variants kept narrow to current scope —
/// not a general MPEG-TS type registry.
///
/// # Relationship with [`StreamTypeCode`]
///
/// `StreamType` only enumerates the codes this library recognizes.
/// Real-world MPEG-TS streams may carry `stream_type` bytes that fall
/// outside this set (vendor-private codes, future codecs, reserved
/// ranges). For demux-side reporting where the full PMT byte space
/// matters, use [`StreamTypeCode`] — it wraps a `Self` for known codes
/// and preserves the raw `u8` for unknown codes. The two enums are
/// paired: `StreamTypeCode::from_byte(b).known()` returns
/// `Option<&StreamType>`, and `StreamType::as_u8()` converts back to the
/// underlying byte. Mux-side APIs use `StreamType` directly because the
/// caller picks a known codec; demux-side stats and event APIs use
/// `StreamTypeCode` so unrecognized PMT bytes are preserved rather than
/// elided.
///
/// # Forward compatibility
///
/// `StreamType` is `#[non_exhaustive]` — outside-crate code (bindings,
/// downstream consumers) cannot exhaustively pattern-match this enum and
/// must include a wildcard arm. This is intentional: new MPEG-TS stream
/// types (vendor codes, future codecs, reserved-range expansions) may be
/// added in minor releases without forcing a source-level break on
/// consumers. To handle unknown PMT bytes specifically, prefer the
/// [`StreamTypeCode`] wrapper which surfaces unrecognized bytes as
/// `StreamTypeCode::Unknown(byte)` rather than triggering a wildcard arm
/// at the typed layer.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamType {
    /// H.264 / AVC video (ITU-T H.264 / ISO/IEC 14496-10).
    H264 = 0x1B,
    /// H.265 / HEVC video (ITU-T H.265 / ISO/IEC 23008-2).
    H265 = 0x24,
    /// H.266 / VVC video (ITU-T H.266 V4 / ISO/IEC 23090-3).
    /// Stream_type assignment from ISO/IEC 13818-1 (ITU-T H.222.0
    /// 2023-08, §2.4.4.x).
    H266 = 0x33,
    /// PES private data, typically used for KLV per ST 1402 async.
    KlvPrivate = 0x06,
    /// Synchronous metadata stream per ST 1402 sync.
    KlvSyncMetadata = 0x15,
    /// MPEG-1 Audio (ISO/IEC 11172-3) — covers Layer I, II, and III (MP3).
    AudioMp2 = 0x03,
    /// AAC audio in ADTS framing (ISO/IEC 13818-7).
    AudioAac = 0x0F,
    /// AAC audio in LATM framing (ISO/IEC 14496-3).
    AudioAacLatm = 0x11,
    /// ATSC AC-3 audio.
    AudioAc3 = 0x81,
}

impl StreamType {
    /// Return the underlying byte value as written in PMT.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Typed wrapper around the PMT `stream_type` byte.
///
/// MPEG-TS PMT carries the stream type as a single byte. The
/// [`StreamType`] enum names the byte values this library recognizes,
/// but real-world streams may carry codes outside that set (custom
/// vendor codes, reserved values, codes for codecs not yet supported).
/// `StreamTypeCode` preserves the full PMT byte space while giving
/// callers a typed view of known codes.
///
/// # Construction
///
/// Use [`Self::from_byte`] to construct from a PMT byte:
///
/// ```
/// use tst_core::mpegts::common::{StreamType, StreamTypeCode};
///
/// let code = StreamTypeCode::from_byte(0x1B);
/// assert_eq!(code.known(), Some(&StreamType::H264));
///
/// let unknown = StreamTypeCode::from_byte(0xFF);
/// assert!(unknown.known().is_none());
/// assert_eq!(unknown.as_byte(), 0xFF);
/// ```
///
/// # C ABI
///
/// The C ABI exposes the raw byte (`tst_stream_stats_t.stream_type: uint8_t`);
/// the Rust-to-C bridge calls [`Self::as_byte`] at conversion. Bindings
/// that want typed matching on known codes can use the `StreamType`
/// enum directly.
///
/// # Cross-reference
///
/// See [`StreamType`] for the list of codes this library recognizes.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTypeCode {
    /// PMT byte maps to a `StreamType` this library recognizes.
    Known(StreamType),
    /// PMT byte does not map to any known [`StreamType`] — the raw byte
    /// is preserved verbatim for downstream inspection.
    Unknown(u8),
}

impl StreamTypeCode {
    /// Decode a PMT `stream_type` byte into a typed [`StreamTypeCode`].
    ///
    /// Returns [`Self::Known`] if the byte matches a [`StreamType`]
    /// variant, otherwise [`Self::Unknown`] carrying the raw byte.
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0x1B => Self::Known(StreamType::H264),
            0x24 => Self::Known(StreamType::H265),
            0x33 => Self::Known(StreamType::H266),
            0x06 => Self::Known(StreamType::KlvPrivate),
            0x15 => Self::Known(StreamType::KlvSyncMetadata),
            0x03 => Self::Known(StreamType::AudioMp2),
            0x0F => Self::Known(StreamType::AudioAac),
            0x11 => Self::Known(StreamType::AudioAacLatm),
            0x81 => Self::Known(StreamType::AudioAc3),
            other => Self::Unknown(other),
        }
    }

    /// Return the underlying PMT byte (recovers the input to [`Self::from_byte`]).
    pub fn as_byte(&self) -> u8 {
        match self {
            Self::Known(st) => st.as_u8(),
            Self::Unknown(b) => *b,
        }
    }

    /// Return a reference to the typed [`StreamType`] if the code is
    /// known, or `None` if it's [`Self::Unknown`].
    ///
    /// Bindings that want exhaustive matching on known codes can use
    /// this to peel off the typed variant:
    ///
    /// ```
    /// use tst_core::mpegts::common::{StreamType, StreamTypeCode};
    ///
    /// let code = StreamTypeCode::from_byte(0x1B);
    /// match code.known() {
    ///     Some(StreamType::H264) => { /* H.264 path */ }
    ///     Some(StreamType::H265) => { /* H.265 path */ }
    ///     _ => { /* other / unknown */ }
    /// }
    /// ```
    pub fn known(&self) -> Option<&StreamType> {
        match self {
            Self::Known(st) => Some(st),
            Self::Unknown(_) => None,
        }
    }
}

impl Default for StreamTypeCode {
    /// `Unknown(0x00)` — matches the pre-typed-migration default for
    /// PSI PIDs and unconfigured streams produced via
    /// `StreamStats::default()`.
    fn default() -> Self {
        Self::Unknown(0x00)
    }
}

/// MPEG-TS descriptor tags relevant to mux output.
pub mod descriptor {
    /// `registration_descriptor` (ISO/IEC 13818-1 §2.6.8).
    pub const REGISTRATION: u8 = 0x05;

    /// 4-byte `format_identifier` for KLV asynchronous data per ST 1402 §5.4.
    pub const KLVA: [u8; 4] = *b"KLVA";
}

/// MPEG-TS PID assignments + helpers.
pub mod pid {
    /// PID 0x0000 — Program Association Table (PAT).
    pub const PAT: u16 = 0x0000;

    /// PID 0x1FFF — null packets (used for stuffing in CBR; we don't emit).
    pub const NULL: u16 = 0x1FFF;

    /// User-program PIDs are 0x0010..=0x1FFE per ISO/IEC 13818-1 §2.4.
    pub fn is_user_pid(pid: u16) -> bool {
        (0x0010..=0x1FFE).contains(&pid)
    }
}

/// 90 kHz timestamp — used for PES PTS/DTS encoding.
///
/// Newtype around `i64` so callers can't accidentally pass milliseconds or
/// 27 MHz values. Spec range is 33-bit unsigned (0..=2^33-1), but we accept
/// `i64` to match what encoders produce; values are masked at encoding time
/// (PES PTS) and at PCR derivation (`Pcr27mhz::from_pts`).
///
/// # Arithmetic
///
/// `Pts90khz` does NOT currently implement `Add` / `Sub` / `Mul` — the
/// wrap-vs-saturate-vs-check semantics across the 33-bit PTS rollover
/// boundary need design work. See the "Deep typed-time migration" entry in
/// the repository's `docs/project/deferred-features.md` for the follow-up plan. For
/// now, recover the raw `i64` with [`as_ticks`] and reconstruct after
/// arithmetic with [`new`].
///
/// [`as_ticks`]: Self::as_ticks
/// [`new`]: Self::new
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pts90khz(i64);

impl Pts90khz {
    /// Construct from raw 90 kHz ticks.
    pub const fn new(ticks: i64) -> Self {
        Self(ticks)
    }

    /// Return the raw 90 kHz tick count.
    pub const fn as_ticks(self) -> i64 {
        self.0
    }

    /// Convert milliseconds to 90 kHz ticks.
    pub fn from_millis(ms: i64) -> Self {
        Self(ms * 90)
    }

    /// Mask to the 33-bit PES PTS field range.
    pub fn masked_33bit(self) -> u64 {
        (self.0 as u64) & ((1u64 << 33) - 1)
    }
}

/// 27 MHz PCR timestamp — base × 300 + extension per ISO/IEC 13818-1 §2.4.3.5.
///
/// Stored as the full 27 MHz value; encoding splits it into the 33-bit base
/// and 9-bit extension at write time.
///
/// # Arithmetic
///
/// Like [`Pts90khz`], `Pcr27mhz` does not currently implement arithmetic
/// traits. The full 27 MHz value wraps at `(1 << 33) × 300`, ≈ once every
/// 26.5 hours; design of typed wrap-aware arithmetic is tracked in the
/// "Deep typed-time migration" entry in `docs/project/deferred-features.md`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pcr27mhz(u64);

impl Pcr27mhz {
    /// Construct from raw 27 MHz ticks.
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Return the raw 27 MHz tick count.
    pub const fn as_ticks(self) -> u64 {
        self.0
    }

    /// Build from milliseconds.
    pub fn from_millis(ms: u64) -> Self {
        Self(ms * 27_000)
    }

    /// Build from a 90 kHz PTS (PCR base = PTS, extension = 0).
    pub fn from_pts(pts: Pts90khz) -> Self {
        Self(pts.masked_33bit() * 300)
    }

    /// 33-bit PCR base (90 kHz units).
    #[must_use]
    pub fn base(self) -> u64 {
        (self.0 / 300) & ((1u64 << 33) - 1)
    }

    /// 9-bit PCR extension (27 MHz units modulo 300).
    #[must_use]
    pub fn extension(self) -> u16 {
        (self.0 % 300) as u16
    }
}

/// 90 kHz tick rate per second — matches the PTS clock per ISO/IEC 13818-1
/// §2.4.2.2 (system_clock_frequency / 300). Use with [`Pts90khz::from_millis`]
/// or in callers that compute frame durations directly.
pub const PTS_TICKS_PER_SECOND: i64 = 90_000;

/// 27 MHz tick rate per second — matches the PCR clock per ISO/IEC 13818-1
/// §2.4.2.2 (the full system_clock_frequency before the 300× division to PTS).
/// Use to convert [`Pcr27mhz`] deltas to wall-clock seconds, e.g. divide a
/// `NonConformantIssue::PcrAnomaly.delta` by this constant to get seconds.
pub const PCR_TICKS_PER_SECOND: u64 = 27_000_000;

/// Standard MPEG-TS packet size in bytes, per ITU-T H.222.0 §2.4.3.2.
///
/// All MPEG-TS packets in this library are exactly 188 bytes (no DVB-ASI
/// 204-byte FEC-augmented packets — see `docs/project/deferred-features.md`).
pub const TS_PACKET_SIZE: usize = 188;

/// MPEG-TS sync byte, per ITU-T H.222.0 §2.4.3.2. Every TS packet begins
/// with this byte at offset 0.
pub const TS_SYNC_BYTE: u8 = 0x47;

/// Number of MPEG-TS packets in libsrt's default SRT live-mode payload — the
/// "TS bundle" libsrt expects from a TS-over-SRT sender.
///
/// libsrt's `SRTO_PAYLOADSIZE` default is 1316 bytes = 7 × 188.
pub const SRT_TS_BUNDLE_PACKETS: usize = 7;

/// libsrt's default SRT live-mode payload size in bytes
/// (`SRT_TS_BUNDLE_PACKETS × TS_PACKET_SIZE = 1316`). Senders and receivers
/// use this as a safe fallback when the inner transport's `max_payload()`
/// is unavailable (mid-reconnect, factory-deferred, etc.).
pub const SRT_TS_BUNDLE_BYTES: usize = SRT_TS_BUNDLE_PACKETS * TS_PACKET_SIZE;

/// Signed difference `now - last` interpreted across the 33-bit PTS
/// rollover boundary. Returns the smaller-magnitude wrap-aware delta.
///
/// Used by both the muxer and the demuxer as a wrap-aware backward-PTS
/// anomaly detector — large negative deltas indicate a non-conformant
/// backward jump rather than a benign 33-bit wrap. By default the
/// demuxer does NOT use this to accumulate stream-monotonic ticks;
/// `DemuxEvent::Sample.pts` and `DemuxEvent::Metadata.pts` remain raw
/// 33-bit values that wrap to 0 at the H.222.0 §2.4.3.7 rollover
/// (≈ every 26.5 h of 90 kHz). The one exception: when the opt-in
/// [`DemuxerConfig::unwrap_timestamps`](crate::mpegts::demux::DemuxerConfig::unwrap_timestamps)
/// is enabled, the demuxer's per-PID accumulator DOES use this function:
/// it adds each sample's signed delta onto the previous unwrapped value,
/// so the emitted `i64` pts/dts carry across the rollover instead of
/// wrapping. The result is not monotonic by construction — a genuine
/// out-of-order arrival yields a negative delta and steps the emitted
/// value back by its true distance.
pub fn pts_diff_33bit(now: u64, last: u64) -> i64 {
    const RANGE: u64 = 1u64 << 33;
    const HALF: u64 = 1u64 << 32;
    debug_assert!(now < RANGE, "now must be 33-bit-masked");
    debug_assert!(last < RANGE, "last must be 33-bit-masked");
    let raw = (now + RANGE - last) % RANGE;
    if raw > HALF {
        (raw as i64) - (RANGE as i64)
    } else {
        raw as i64
    }
}

/// Signed difference `now - last` for PCR-27MHz values, interpreted across
/// the 33-bit-base × 300 wrap. PCR-27MHz = `base × 300 + ext` where `base`
/// is 33 bits and `ext` is 9 bits. The full 27 MHz value wraps at
/// `(1 << 33) × 300 ≈ 2.577 × 10^12`, which is once every ~26.5 hours.
///
/// Used by the demuxer to detect PCR jumps without false-positives across
/// the long-stream rollover boundary.
pub fn pcr_diff_27mhz(now: u64, last: u64) -> i64 {
    const WRAP: i128 = (1i128 << 33) * 300;
    const HALF: i128 = WRAP / 2;
    let diff = now as i128 - last as i128;
    let adjusted = if diff > HALF {
        diff - WRAP
    } else if diff < -HALF {
        diff + WRAP
    } else {
        diff
    };
    adjusted as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_type_byte_values() {
        assert_eq!(StreamType::H264.as_u8(), 0x1B);
        assert_eq!(StreamType::H265.as_u8(), 0x24);
        assert_eq!(StreamType::KlvPrivate.as_u8(), 0x06);
        assert_eq!(StreamType::KlvSyncMetadata.as_u8(), 0x15);
    }

    #[test]
    fn stream_type_h266_byte() {
        assert_eq!(StreamType::H266.as_u8(), 0x33);
    }

    #[test]
    fn klva_descriptor_bytes() {
        assert_eq!(descriptor::KLVA, [0x4B, 0x4C, 0x56, 0x41]);
        assert_eq!(descriptor::REGISTRATION, 0x05);
    }

    #[test]
    fn user_pid_range() {
        assert!(!pid::is_user_pid(0x0000));
        assert!(!pid::is_user_pid(0x000F));
        assert!(pid::is_user_pid(0x0010));
        assert!(pid::is_user_pid(0x1011));
        assert!(pid::is_user_pid(0x1FFE));
        assert!(!pid::is_user_pid(0x1FFF));
    }

    #[test]
    fn pts_from_millis() {
        assert_eq!(Pts90khz::from_millis(0).as_ticks(), 0);
        assert_eq!(Pts90khz::from_millis(1000).as_ticks(), 90_000);
        assert_eq!(Pts90khz::from_millis(1000).as_ticks(), PTS_TICKS_PER_SECOND);
    }

    #[test]
    fn pts_masking() {
        // 33-bit max = 0x1_FFFF_FFFF
        assert_eq!(Pts90khz::new(0x1_FFFF_FFFF).masked_33bit(), 0x1_FFFF_FFFF);
        // Higher bits get masked off.
        assert_eq!(Pts90khz::new(0x3_FFFF_FFFF).masked_33bit(), 0x1_FFFF_FFFF);
    }

    #[test]
    fn pcr_from_millis() {
        assert_eq!(Pcr27mhz::from_millis(0).as_ticks(), 0);
        assert_eq!(Pcr27mhz::from_millis(40).as_ticks(), 40 * 27_000);
        assert_eq!(Pcr27mhz::from_millis(1000).as_ticks(), PCR_TICKS_PER_SECOND);
    }

    #[test]
    fn pcr_base_extension_split() {
        // 90 kHz tick * 300 = exactly one base unit, zero extension.
        let pcr = Pcr27mhz::from_pts(Pts90khz::new(1));
        assert_eq!(pcr.base(), 1);
        assert_eq!(pcr.extension(), 0);

        // Mid-base value: 1 base + 150 extension.
        let pcr = Pcr27mhz::new(300 + 150);
        assert_eq!(pcr.base(), 1);
        assert_eq!(pcr.extension(), 150);
    }

    #[test]
    fn pcr_base_masks_to_33bit() {
        // Exactly at the 33-bit boundary: bit 33 set, low bits clear.
        // Pre-mask base = 1u64 << 33; post-mask should be 0.
        let pcr = Pcr27mhz::new((1u64 << 33) * 300);
        assert_eq!(pcr.base(), 0);

        // Bit 33 set plus a low-bit value: the high bit gets stripped, low bits preserved.
        let pcr = Pcr27mhz::new(((1u64 << 33) + 1) * 300);
        assert_eq!(pcr.base(), 1);

        // Far-above-mask value: only the low 33 bits should survive.
        let pcr = Pcr27mhz::new(((1u64 << 50) | 0x1234_5678) * 300);
        assert_eq!(pcr.base(), 0x1234_5678);
    }

    #[test]
    fn pts_diff_33bit_forward_simple() {
        // Forward by 90 ticks (1ms at 90kHz).
        assert_eq!(pts_diff_33bit(1_000, 910), 90);
    }

    #[test]
    fn pts_diff_33bit_backward_simple() {
        // Backward by 90 ticks — signed delta is negative.
        assert_eq!(pts_diff_33bit(910, 1_000), -90);
    }

    #[test]
    fn pts_diff_33bit_zero() {
        assert_eq!(pts_diff_33bit(0, 0), 0);
        assert_eq!(pts_diff_33bit(12_345, 12_345), 0);
    }

    #[test]
    fn pts_diff_33bit_wrap_forward() {
        // last is just below 2^33-1, now is 100 (wrapped past zero).
        // True delta is +100 + (2^33 - (2^33 - 50)) = +150.
        let last = (1u64 << 33) - 50;
        let now = 100u64;
        assert_eq!(pts_diff_33bit(now, last), 150);
    }

    #[test]
    fn pts_diff_33bit_wrap_backward() {
        // The opposite: now is just below 2^33, last is small. Backward by ~50.
        let last = 100u64;
        let now = (1u64 << 33) - 50;
        assert_eq!(pts_diff_33bit(now, last), -150);
    }

    #[test]
    fn pts_diff_33bit_half_range_boundary() {
        // Exactly half the 33-bit range — by convention, treat as forward.
        let last = 0u64;
        let now = 1u64 << 32;
        assert_eq!(pts_diff_33bit(now, last), 1i64 << 32);
        // One past half: treated as backward (raw > HALF → negative delta).
        assert_eq!(pts_diff_33bit((1u64 << 32) + 1, 0), -(1i64 << 32) + 1);
    }

    #[test]
    fn pcr_diff_27mhz_forward_simple() {
        // 1000 ticks forward.
        assert_eq!(pcr_diff_27mhz(2_000, 1_000), 1_000);
    }

    #[test]
    fn pcr_diff_27mhz_backward_simple() {
        assert_eq!(pcr_diff_27mhz(1_000, 2_000), -1_000);
    }

    #[test]
    fn pcr_diff_27mhz_wrap_forward() {
        // last just before wrap, now just after wrap.
        let wrap = (1u64 << 33) * 300;
        let last = wrap - 100;
        let now = 50; // wrapped
        assert_eq!(pcr_diff_27mhz(now, last), 150);
    }

    #[test]
    fn pcr_diff_27mhz_wrap_backward() {
        // last just after wrap, now just before wrap (a real backward jump).
        let wrap = (1u64 << 33) * 300;
        let last = 50;
        let now = wrap - 100;
        assert_eq!(pcr_diff_27mhz(now, last), -150);
    }

    #[test]
    fn pts_pcr_accessors_round_trip() {
        let pts = Pts90khz::new(90_000);
        assert_eq!(pts.as_ticks(), 90_000);

        let pcr = Pcr27mhz::new(27_000_000);
        assert_eq!(pcr.as_ticks(), 27_000_000);
    }
}

#[cfg(test)]
mod stream_type_code_tests {
    use super::*;

    #[test]
    fn from_byte_known_h264() {
        let code = StreamTypeCode::from_byte(0x1B);
        assert_eq!(code.known(), Some(&StreamType::H264));
        assert_eq!(code.as_byte(), 0x1B);
    }

    #[test]
    fn from_byte_known_h265() {
        let code = StreamTypeCode::from_byte(0x24);
        assert_eq!(code.known(), Some(&StreamType::H265));
        assert_eq!(code.as_byte(), 0x24);
    }

    #[test]
    fn from_byte_known_klv_private() {
        let code = StreamTypeCode::from_byte(0x06);
        assert_eq!(code.known(), Some(&StreamType::KlvPrivate));
        assert_eq!(code.as_byte(), 0x06);
    }

    #[test]
    fn from_byte_unknown_preserves_byte() {
        let code = StreamTypeCode::from_byte(0xFF);
        assert!(code.known().is_none());
        assert_eq!(code.as_byte(), 0xFF);
    }

    #[test]
    fn round_trip_all_known_variants() {
        for st in [
            StreamType::H264,
            StreamType::H265,
            StreamType::H266,
            StreamType::KlvPrivate,
            StreamType::KlvSyncMetadata,
            StreamType::AudioMp2,
            StreamType::AudioAac,
            StreamType::AudioAacLatm,
            StreamType::AudioAc3,
        ] {
            let byte = st.as_u8();
            let code = StreamTypeCode::from_byte(byte);
            assert_eq!(
                code.known().copied(),
                Some(st),
                "byte 0x{byte:02X} should map to {st:?}"
            );
            assert_eq!(code.as_byte(), byte);
        }
    }

    #[test]
    fn default_is_unknown_zero() {
        let code = StreamTypeCode::default();
        assert_eq!(code, StreamTypeCode::Unknown(0x00));
        assert_eq!(code.as_byte(), 0x00);
        assert!(code.known().is_none());
    }
}
