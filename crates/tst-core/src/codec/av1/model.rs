//! AV1 parsed type definitions.

use crate::codec::{ChromaFormat, CodecParseError, ColorInfo, Rational};
use alloc::vec::Vec;

crate::binding_record! {
    /// Parsed AV1 Sequence Header OBU.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Av1SequenceHeader {
        /// `seq_profile` — 0=Main, 1=High, 2=Professional.
        pub profile: u8,
        /// `seq_level_idx[0]` — operating point 0 level index.
        pub level: u8,
        /// `seq_tier[0]` — operating point 0 tier (0 unless level > 7).
        pub tier: u8,
        /// `max_frame_width_minus_1 + 1`.
        pub max_frame_width: u32,
        /// `max_frame_height_minus_1 + 1`.
        pub max_frame_height: u32,
        /// 8, 10, or 12 per BitDepth derivation in AV1 §5.5.2.
        pub bit_depth: u8,
        /// True when `mono_chrome = 1` (Y-only stream).
        pub monochrome: bool,
        /// Chroma subsampling format derived from profile + mono_chrome bits.
        pub chroma_format: ChromaFormat,
        /// True when `still_picture = 1`.
        pub still_picture: bool,
        /// True when `reduced_still_picture_header = 1`.
        pub reduced_still_picture_header: bool,
        /// Color metadata. AV1 always carries a `color_range` bit in the wire
        /// format (see §6.4.2), so a successful parse populates this with at
        /// least the dynamic-range signal. When `color_description_present_flag
        /// == 0`, `primaries`/`transfer`/`matrix` default to `Unspecified` per
        /// §5.5.2 and only `full_range` carries observed data. The `Option`
        /// wrapper is kept for `ColorInfo` parity with the H.26x parsers and for
        /// forward-compatibility with future error-recovery paths.
        pub color_info: Option<ColorInfo>,
        /// Frame rate as a rational number, derived from `time_scale /
        /// num_units_in_display_tick` when `timing_info_present_flag == 1` and
        /// `equal_picture_interval == 1`; otherwise absent.
        pub frame_rate: Option<Rational>,
        /// The original payload bytes as passed to the parser.
        pub raw: Vec<u8>,
    }
}

crate::binding_record! {
    /// Light-weight AV1 Frame Header.
    ///
    /// Light scope: `frame_type`, `show_frame` and `show_existing_frame` only.
    /// `frame_size` is never set: full per-frame size extraction requires
    /// reference-frame management beyond this parser's scope.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Av1FrameHeaderLight {
        /// `frame_type` per AV1 §5.9.1: 0=KEY_FRAME, 1=INTER_FRAME,
        /// 2=INTRA_ONLY_FRAME, 3=SWITCH_FRAME.
        pub frame_type: u8,
        /// True when the decoded frame is displayed immediately.
        pub show_frame: bool,
        /// True when this OBU references a previously decoded frame for display.
        pub show_existing_frame: bool,
        /// Per-frame size override. The current scope never sets it: the bit
        /// position of the override field depends on frame_type and
        /// frame_id_numbers_present_flag in ways this parser does not decode.
        /// Consumers needing per-frame size should drive a full decoder.
        @[custom] pub frame_size: Option<(u32, u32)>,
        /// The original payload bytes as passed to the parser.
        pub raw: Vec<u8>,
    }
}

crate::binding_record! {
    /// Aggregate of all typed structs extracted from a sequence of AV1 OBUs.
    ///
    /// Build a list of OBUs, then call the OBU-stream parser to populate the
    /// three fields. Partial-success-tolerant: OBUs that fail to parse accumulate
    /// in `unparseable` rather than aborting the walk.
    #[must_use]
    #[non_exhaustive]
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Av1ObuStream {
        /// All successfully parsed Sequence Header OBUs in encounter order.
        pub sequence_headers: Vec<Av1SequenceHeader>,
        /// All successfully parsed Frame Header OBUs in encounter order.
        pub frame_headers: Vec<Av1FrameHeaderLight>,
        /// `(obu_type, error)` for each OBU that failed to parse. Frame header
        /// OBUs that arrive before a sequence header land here too, with a
        /// synthesized "frame header before sequence header" error.
        @[custom] pub unparseable: Vec<(u8, CodecParseError)>,
    }
}
