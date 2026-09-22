//! One error-kind table for every binding (C, Python, JVM).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! `BindingErrorKind` is the superset of [`crate::ShellErrorKind`] over the
//! transport / mux / demux / KLV / codec / RTSP / UDP / TCP / HLS / RIST error
//! types. The discriminant of every variant that C already numbers is the frozen
//! `TST_E_*` code (`bindings/c/core/src/error.rs`); kinds C never had are numbered
//! from −49 downwards and [`BindingErrorKind::c_projection`] says which frozen
//! code C emits for them. `name()` is the string Python and the JVM resolve
//! (`getattr(<Domain>ErrorKind, kind.name())` / `GetStaticField`), so a name that
//! does not resolve is a startup failure in the binding, never a runtime one — the
//! `scripts/check/repo/kind-equivalence.sh` rail imports every member at test time.
//!
//! Naming rule: cross-domain kinds are unprefixed; domain kinds carry their domain
//! (`Srt`, `Udp`, `Tcp`, `Hls`, `Rist`, `Rtp`, `Rtsp`, `Demux`, `KlvDecode`,
//! `KlvEncode`, `Codec`) in [`BindingErrorKind::variant_name`]; [`BindingErrorKind::name`]
//! strips it, because the per-domain binding enums spell the unprefixed member
//! (`UdpErrorKind.IO`).

use std::string::String;

/// One row per kind: `Variant = discriminant => "VARIANT_NAME", "NAME", c_projection;`
///
/// The row format is what the `--print-kinds` bin and the unit tests pin:
/// VARIANT_NAME must be SCREAMING_SNAKE of the variant, NAME (what the
/// bindings resolve) must be VARIANT_NAME minus exactly one domain prefix
/// (or VARIANT_NAME itself), c_projection must be a TST_E
/// code in -48..=-1 and must equal the discriminant for C-numbered kinds.
macro_rules! kinds {
    ( $( $(#[$doc:meta])* $variant:ident = $code:literal => $name:literal, $member:literal, $proj:literal ; )+ ) => {
        /// The one error-kind table shared by every binding (spec §3.3).
        ///
        /// **Stability: Provisional** — see the
        /// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
        ///
        /// `#[repr(i32)]`: the discriminant is the frozen C `TST_E_*` code for
        /// the 41 kinds C already numbers and a new number ≤ −49 for the rest
        /// (see [`Self::c_projection`]). `#[non_exhaustive]`: match with a
        /// wildcard from outside `tst-pipeline`; new kinds are additive.
        #[repr(i32)]
        #[non_exhaustive]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BindingErrorKind {
            $( $(#[$doc])* $variant = $code, )+
        }

        impl BindingErrorKind {
            /// Every variant, in table order (the order the kind-equivalence
            /// rail's TSV follows and the order new discriminants were assigned in).
            pub const ALL: &'static [BindingErrorKind] = &[ $( BindingErrorKind::$variant, )+ ];

            #[cfg(test)]
            pub(crate) const VARIANT_IDENTS: &'static [&'static str] = &[ $( stringify!($variant), )+ ];

            /// SCREAMING_SNAKE of the Rust variant (`UdpIo` → `UDP_IO`); unique
            /// per kind. The TSV's `rust_name` column.
            pub fn variant_name(&self) -> &'static str {
                match self { $( Self::$variant => $name, )+ }
            }

            /// The kind's name as the bindings resolve it (the kind rule, spec
            /// §3.3): [`Self::variant_name`] with the domain prefix removed
            /// (`UdpIo` → `IO`, what `getattr(UdpErrorKind, …)` /
            /// `GetStaticField(UdpException$Kind, …)` looks up); equal to
            /// `variant_name()` for cross-domain kinds. NOT unique across
            /// domains (`IO` is the name of six kinds).
            pub fn name(&self) -> &'static str {
                match self { $( Self::$variant => $member, )+ }
            }

            /// The `TST_E_*` code C emits for this kind: the discriminant itself for
            /// the C-numbered kinds, the frozen code this kind was folded into
            /// before the table existed for the ≤ −49 kinds (e.g. `TcpTlsDisabled`
            /// → −33 `TST_E_TCP_TLS`). Always in `-48..=-1`.
            pub fn c_projection(&self) -> i32 {
                match self { $( Self::$variant => $proj, )+ }
            }
        }
    };
}

kinds! {
    /// Configuration rejected (`ShellErrorKind::ConfigInvalid`, `MuxErrorKind::ConfigInvalid`, SRT address/option/URL errors).
    ConfigInvalid = -1 => "CONFIG_INVALID", "CONFIG_INVALID", -1;
    /// `MuxError::InvalidNal`.
    InvalidNal = -2 => "INVALID_NAL", "INVALID_NAL", -2;
    /// Input bytes malformed (`ShellErrorKind::InputMalformed`, TS framing, over-size audio/subtitle/data pushes).
    InputMalformed = -3 => "INPUT_MALFORMED", "INPUT_MALFORMED", -3;
    /// Transient refusal, retry later (`TransportError::Backpressure`, `MuxError::BufferFull`, SRT `QueueFull`).
    Backpressure = -4 => "BACKPRESSURE", "BACKPRESSURE", -4;
    /// `MuxError::KlvTooLarge`.
    KlvTooLarge = -5 => "KLV_TOO_LARGE", "KLV_TOO_LARGE", -5;
    /// Payload exceeds the transport cap (`TransportError::TooLarge`, SRT `PayloadTooLarge` / `BufferTooSmall`).
    TooLarge = -6 => "TOO_LARGE", "TOO_LARGE", -6;
    /// Caller closed or cancelled (`TransportError::{Closed on a sender, ExplicitClose}`, `HandleState::Closed`, `TcpError::Closed`, SRT `ListenerClosed` / `SocketClosed`).
    Closed = -7 => "CLOSED", "CLOSED", -7;
    /// Transport dead (`TransportError::Broken`, SRT `ConnectionBroken`).
    Broken = -8 => "BROKEN", "BROKEN", -8;
    /// Wrong handle / target for the call (`MuxErrorKind::InvalidUsage`).
    InvalidUsage = -9 => "INVALID_USAGE", "INVALID_USAGE", -9;
    /// Library-internal failure, a poisoned handle lock, or an unmapped variant of a `#[non_exhaustive]` upstream enum.
    Internal = -10 => "INTERNAL", "INTERNAL", -10;
    /// A panic caught at the binding boundary (`HandleState::Panicked`).
    PanicCaught = -11 => "PANIC_CAUGHT", "PANIC_CAUGHT", -11;
    /// Peer closed cleanly on a receiver shell (`ShellErrorKind::EndOfStream`).
    EndOfStream = -12 => "END_OF_STREAM", "END_OF_STREAM", -12;
    /// Transient: value not available right now (C managed-stats getters mid-reconnect).
    NotAvailable = -13 => "NOT_AVAILABLE", "NOT_AVAILABLE", -13;
    /// Persistent: key never observed (C per-PID getters).
    NotFound = -14 => "NOT_FOUND", "NOT_FOUND", -14;
    /// `MuxError::InvalidAv1Obu`.
    InvalidAv1Obu = -44 => "INVALID_AV1_OBU", "INVALID_AV1_OBU", -44;
    /// `MuxError::MispTime`.
    MispTime = -45 => "MISP_TIME", "MISP_TIME", -45;
    /// C `tst_misp_time_extract` on a malformed ST 0604 SEI.
    MispTimeMalformed = -46 => "MISP_TIME_MALFORMED", "MISP_TIME_MALFORMED", -46;
    /// C `tst_st0601_get_{f64,u64}` on a tag of another native type.
    WrongType = -47 => "WRONG_TYPE", "WRONG_TYPE", -47;
    /// RTSP wire/protocol failure not covered by a finer RTSP kind.
    RtspProtocol = -16 => "RTSP_PROTOCOL", "PROTOCOL", -16;
    /// RTSP credentials exhausted.
    RtspAuthFailed = -17 => "RTSP_AUTH_FAILED", "AUTH_FAILED", -17;
    /// RTSP 401, or an unsupported auth scheme was demanded.
    RtspAuthRequired = -18 => "RTSP_AUTH_REQUIRED", "AUTH_REQUIRED", -18;
    /// RTSP 404, or no uniquely-identified SDP media.
    RtspNotFound = -19 => "RTSP_NOT_FOUND", "NOT_FOUND", -19;
    /// RTSP 461 on every transport, or packetization-mode 2.
    RtspUnsupportedTransport = -20 => "RTSP_UNSUPPORTED_TRANSPORT", "UNSUPPORTED_TRANSPORT", -20;
    /// RTSP TLS failure (client or server).
    RtspTls = -21 => "RTSP_TLS", "TLS", -21;
    /// RTSP control-channel I/O failure (client, or server listener / bind-in-use).
    RtspIo = -22 => "RTSP_IO", "IO", -22;
    /// RTSP request timeout.
    RtspTimeout = -23 => "RTSP_TIMEOUT", "TIMEOUT", -23;
    /// RTSP server lifecycle misuse (`AlreadyStarted` / `NotStarted` / `Shutdown`).
    RtspServer = -24 => "RTSP_SERVER", "SERVER", -24;
    /// RTSP mount failure (`MountError`, mount-path / multicast-group / duplicate / config).
    RtspMount = -25 => "RTSP_MOUNT", "MOUNT", -25;
    /// `UdpError::Io`.
    UdpIo = -26 => "UDP_IO", "IO", -26;
    /// `UdpError::InvalidConfig`.
    UdpInvalidConfig = -27 => "UDP_INVALID_CONFIG", "INVALID_CONFIG", -27;
    /// `TcpError::Io`.
    TcpIo = -30 => "TCP_IO", "IO", -30;
    /// `TcpError::InvalidConfig`.
    TcpInvalidConfig = -31 => "TCP_INVALID_CONFIG", "INVALID_CONFIG", -31;
    /// `TcpError::ConnectTimeout`.
    TcpConnectTimeout = -32 => "TCP_CONNECT_TIMEOUT", "CONNECT_TIMEOUT", -32;
    /// `TcpError::Tls`.
    TcpTls = -33 => "TCP_TLS", "TLS", -33;
    /// `HlsError::Io`.
    HlsIo = -34 => "HLS_IO", "IO", -34;
    /// `HlsError::InvalidConfig`.
    HlsInvalidConfig = -35 => "HLS_INVALID_CONFIG", "INVALID_CONFIG", -35;
    /// `HlsError::Finished`.
    HlsFinished = -36 => "HLS_FINISHED", "FINISHED", -36;
    /// `HlsError::Tls`.
    HlsTls = -37 => "HLS_TLS", "TLS", -37;
    /// `RistError::Ffi`.
    RistFfi = -38 => "RIST_FFI", "FFI", -38;
    /// `RistError::InvalidConfig`.
    RistInvalidConfig = -39 => "RIST_INVALID_CONFIG", "INVALID_CONFIG", -39;
    /// `RistError::EncryptionDisabled`.
    RistEncryptionDisabled = -41 => "RIST_ENCRYPTION_DISABLED", "ENCRYPTION_DISABLED", -41;
    /// SRT connect / bind failed (refused, rejected, bad encryption, address in use, permission, system).
    SrtConnectFailed = -49 => "SRT_CONNECT_FAILED", "CONNECT_FAILED", -8;
    /// SRT accept failed (peer rejected during handshake, system).
    SrtAcceptFailed = -50 => "SRT_ACCEPT_FAILED", "ACCEPT_FAILED", -8;
    /// SRT connect / accept / send / recv timed out.
    SrtTimeout = -51 => "SRT_TIMEOUT", "TIMEOUT", -8;
    /// SRT low-level I/O failure (`IoError`, `SendError` / `RecvError` system or libsrt errors).
    SrtIo = -52 => "SRT_IO", "IO", -8;
    /// `UdpError::Url`.
    UdpUrl = -53 => "UDP_URL", "URL", -27;
    /// `TcpError::Url`.
    TcpUrl = -54 => "TCP_URL", "URL", -31;
    /// `TcpError::TlsDisabled`.
    TcpTlsDisabled = -55 => "TCP_TLS_DISABLED", "TLS_DISABLED", -33;
    /// `HlsError::Url`.
    HlsUrl = -56 => "HLS_URL", "URL", -35;
    /// `HlsError::BindFailed`.
    HlsBindFailed = -57 => "HLS_BIND_FAILED", "BIND_FAILED", -34;
    /// `HlsError::UnalignedPushTs`.
    HlsUnalignedPushTs = -58 => "HLS_UNALIGNED_PUSH_TS", "UNALIGNED_PUSH_TS", -35;
    /// `HlsError::TlsDisabled`.
    HlsTlsDisabled = -59 => "HLS_TLS_DISABLED", "TLS_DISABLED", -37;
    /// `RistError::Url`.
    RistUrl = -60 => "RIST_URL", "URL", -39;
    /// `RistError::ContextCreateFailed`.
    RistContextCreateFailed = -61 => "RIST_CONTEXT_CREATE_FAILED", "CONTEXT_CREATE_FAILED", -38;
    /// `RistError::PeerCreateFailed`.
    RistPeerCreateFailed = -62 => "RIST_PEER_CREATE_FAILED", "PEER_CREATE_FAILED", -38;
    /// tst-rtp `ConnectError::PayloadTypeParam`.
    RtpPayloadTypeParam = -63 => "RTP_PAYLOAD_TYPE_PARAM", "PAYLOAD_TYPE_PARAM", -15;
    /// tst-rtp `ConnectError::MissingPayloadTypeParam`.
    RtpMissingPayloadTypeParam = -64 => "RTP_MISSING_PAYLOAD_TYPE_PARAM", "MISSING_PAYLOAD_TYPE_PARAM", -15;
    /// tst-rtp `ConnectError::Url`.
    RtpUrl = -65 => "RTP_URL", "URL", -15;
    /// tst-rtp `ConnectError::HostNotLiteral`.
    RtpHostNotLiteral = -66 => "RTP_HOST_NOT_LITERAL", "HOST_NOT_LITERAL", -15;
    /// tst-rtp `ConnectError::Io`.
    RtpIo = -67 => "RTP_IO", "IO", -15;
    /// tst-rtp `ConnectError::IfaceUnsupported`.
    RtpIfaceUnsupported = -68 => "RTP_IFACE_UNSUPPORTED", "IFACE_UNSUPPORTED", -15;
    /// `DemuxError::Unrecoverable`.
    DemuxUnrecoverable = -69 => "DEMUX_UNRECOVERABLE", "UNRECOVERABLE", -3;
    /// `DemuxError::StrictRejection`.
    DemuxStrictRejection = -70 => "DEMUX_STRICT_REJECTION", "STRICT_REJECTION", -3;
    /// `DemuxError::MalformedPsi`.
    DemuxMalformedPsi = -71 => "DEMUX_MALFORMED_PSI", "MALFORMED_PSI", -3;
    /// `DemuxError::MalformedPes`.
    DemuxMalformedPes = -72 => "DEMUX_MALFORMED_PES", "MALFORMED_PES", -3;
    /// `DemuxError::SyncBufExhausted`.
    DemuxSyncBufExhausted = -73 => "DEMUX_SYNC_BUF_EXHAUSTED", "SYNC_BUF_EXHAUSTED", -6;
    /// KLV set truncated / BER length malformed or overflowing.
    KlvDecodeTruncatedSet = -74 => "KLV_DECODE_TRUNCATED_SET", "TRUNCATED_SET", -48;
    /// Unexpected universal label.
    KlvDecodeBadUniversalLabel = -75 => "KLV_DECODE_BAD_UNIVERSAL_LABEL", "BAD_UNIVERSAL_LABEL", -48;
    /// ST 0601 checksum or ST 0806 CRC-32 mismatch.
    KlvDecodeChecksumMismatch = -76 => "KLV_DECODE_CHECKSUM_MISMATCH", "CHECKSUM_MISMATCH", -48;
    /// Duplicate tag in a set.
    KlvDecodeDuplicateTag = -77 => "KLV_DECODE_DUPLICATE_TAG", "DUPLICATE_TAG", -48;
    /// A spec-mandatory tag is missing or misplaced.
    KlvDecodeMissingRequiredTag = -78 => "KLV_DECODE_MISSING_REQUIRED_TAG", "MISSING_REQUIRED_TAG", -48;
    /// Any other structural KLV decode failure (malformed tag, non-canonical BER, trailing bytes, field validation).
    KlvDecodeMalformedBytes = -79 => "KLV_DECODE_MALFORMED_BYTES", "MALFORMED_BYTES", -48;
    /// `KlvEncodeError::BufferTooSmall`.
    KlvEncodeBufferTooSmall = -80 => "KLV_ENCODE_BUFFER_TOO_SMALL", "BUFFER_TOO_SMALL", -10;
    /// `KlvEncodeError::RecordTooLarge`.
    KlvEncodeRecordTooLarge = -81 => "KLV_ENCODE_RECORD_TOO_LARGE", "RECORD_TOO_LARGE", -10;
    /// `KlvEncodeError::OutOfRange`.
    KlvEncodeOutOfRange = -82 => "KLV_ENCODE_OUT_OF_RANGE", "OUT_OF_RANGE", -10;
    /// `KlvEncodeError::StringTooLong`.
    KlvEncodeStringTooLong = -83 => "KLV_ENCODE_STRING_TOO_LONG", "STRING_TOO_LONG", -10;
    /// `KlvEncodeError::UnsupportedImapbLength`.
    KlvEncodeUnsupportedImapbLength = -84 => "KLV_ENCODE_UNSUPPORTED_IMAPB_LENGTH", "UNSUPPORTED_IMAPB_LENGTH", -10;
    /// `KlvEncodeError::InvalidImapbParams`.
    KlvEncodeInvalidImapbParams = -85 => "KLV_ENCODE_INVALID_IMAPB_PARAMS", "INVALID_IMAPB_PARAMS", -10;
    /// `KlvEncodeError::MissingMandatoryItem`.
    KlvEncodeMissingMandatoryItem = -86 => "KLV_ENCODE_MISSING_MANDATORY_ITEM", "MISSING_MANDATORY_ITEM", -10;
    /// `KlvEncodeError::ReservedTagInUnknown`.
    KlvEncodeReservedTagInUnknown = -87 => "KLV_ENCODE_RESERVED_TAG_IN_UNKNOWN", "RESERVED_TAG_IN_UNKNOWN", -10;
    /// `KlvEncodeError::VTargetPackEmpty`.
    KlvEncodeVTargetPackEmpty = -88 => "KLV_ENCODE_V_TARGET_PACK_EMPTY", "V_TARGET_PACK_EMPTY", -10;
    /// `KlvEncodeError::DuplicateTargetId`.
    KlvEncodeDuplicateTargetId = -89 => "KLV_ENCODE_DUPLICATE_TARGET_ID", "DUPLICATE_TARGET_ID", -10;
    /// `KlvEncodeError::ForbiddenStandaloneOffset`.
    KlvEncodeForbiddenStandaloneOffset = -90 => "KLV_ENCODE_FORBIDDEN_STANDALONE_OFFSET", "FORBIDDEN_STANDALONE_OFFSET", -10;
    /// `CodecParseError::TruncatedRbsp`.
    CodecTruncatedRbsp = -91 => "CODEC_TRUNCATED_RBSP", "TRUNCATED_RBSP", -10;
    /// `CodecParseError::InvalidGolomb`.
    CodecInvalidGolomb = -92 => "CODEC_INVALID_GOLOMB", "INVALID_GOLOMB", -10;
    /// `CodecParseError::ReservedValue`.
    CodecReservedValue = -93 => "CODEC_RESERVED_VALUE", "RESERVED_VALUE", -10;
    /// `CodecParseError::UnsupportedProfile`.
    CodecUnsupportedProfile = -94 => "CODEC_UNSUPPORTED_PROFILE", "UNSUPPORTED_PROFILE", -10;
    /// `CodecParseError::DanglingSpsReference`.
    CodecDanglingSpsReference = -95 => "CODEC_DANGLING_SPS_REFERENCE", "DANGLING_SPS_REFERENCE", -10;
    /// `CodecParseError::DanglingVpsReference`.
    CodecDanglingVpsReference = -96 => "CODEC_DANGLING_VPS_REFERENCE", "DANGLING_VPS_REFERENCE", -10;
    /// `CodecParseError::EngineError`.
    CodecEngineError = -97 => "CODEC_ENGINE_ERROR", "ENGINE_ERROR", -10;
    /// `CodecParseError::InvalidLeb128`.
    CodecInvalidLeb128 = -98 => "CODEC_INVALID_LEB128", "INVALID_LEB128", -10;
    /// `CodecParseError::BadSyncWord`.
    CodecBadSyncWord = -99 => "CODEC_BAD_SYNC_WORD", "BAD_SYNC_WORD", -10;
    /// `CodecParseError::Truncated`.
    CodecTruncated = -100 => "CODEC_TRUNCATED", "TRUNCATED", -10;
    /// `CodecParseError::Forbidden`.
    CodecForbidden = -101 => "CODEC_FORBIDDEN", "FORBIDDEN", -10;
    /// `CodecParseError::UnsupportedFreeFormat`.
    CodecUnsupportedFreeFormat = -102 => "CODEC_UNSUPPORTED_FREE_FORMAT", "UNSUPPORTED_FREE_FORMAT", -10;
    /// `CodecParseError::InvalidLengthSize` (C: −1, `codec_framing.rs:44`).
    CodecInvalidLengthSize = -103 => "CODEC_INVALID_LENGTH_SIZE", "INVALID_LENGTH_SIZE", -1;
    /// `CodecParseError::NalLengthOverflow` (C: −6, `codec_framing.rs:45`).
    CodecNalLengthOverflow = -104 => "CODEC_NAL_LENGTH_OVERFLOW", "NAL_LENGTH_OVERFLOW", -6;
    /// `CodecParseError::BufferTooSmall` (C: −4, the two-call size-query idiom).
    CodecBufferTooSmall = -105 => "CODEC_BUFFER_TOO_SMALL", "BUFFER_TOO_SMALL", -4;
}

impl BindingErrorKind {
    /// The discriminant (`*self as i32`); the frozen `TST_E_*` number for the
    /// C-numbered kinds, ≤ −49 otherwise.
    pub fn c_code(&self) -> i32 {
        *self as i32
    }

    /// `true` when the discriminant is a frozen C code (`-48..=-1`).
    pub fn is_c_frozen(&self) -> bool {
        self.c_code() >= -48
    }
}

/// A kind plus the human-readable detail the binding surfaces as the
/// exception message / `tst_get_last_error_str()`.
///
/// **Stability: Provisional** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingError {
    pub kind: BindingErrorKind,
    pub detail: String,
}

impl BindingError {
    /// A kind with its detail message.
    pub fn new(kind: BindingErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl core::fmt::Display for BindingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for BindingError {}

use tst_core::codec::CodecParseError;
use tst_core::error::{
    DemuxError, KlvDecodeError, KlvEncodeError, KlvFieldError, MuxError, MuxErrorKind,
};

/// The detail a K7 wildcard produces: a variant of a `#[non_exhaustive]`
/// upstream enum that this table does not map yet. Loud on purpose — the
/// `scripts/check/rust/kind-table-coverage.sh` rail fails before such a
/// variant can reach a user, so this string only ever appears if the rail
/// itself was bypassed.
fn unmapped(kind: BindingErrorKind, enum_name: &str, e: &dyn core::fmt::Debug) -> BindingError {
    BindingError::new(kind, format!("unmapped {enum_name} variant: {e:?}"))
}

/// K4: the four `MuxError` variants C already numbers precisely keep their
/// own kinds; everything else folds to `MuxError::kind()`, whose own
/// per-variant coverage is `scripts/check/rust/mux-error-kind-coverage.sh`
/// (tst-core). Both matches need a wildcard (`MuxError` and `MuxErrorKind`
/// are `#[non_exhaustive]` in tst-core).
pub fn kind_of_mux(e: &MuxError) -> BindingErrorKind {
    match e {
        MuxError::InvalidNal => BindingErrorKind::InvalidNal,
        MuxError::InvalidAv1Obu => BindingErrorKind::InvalidAv1Obu,
        MuxError::MispTime(_) => BindingErrorKind::MispTime,
        MuxError::KlvTooLarge { .. } => BindingErrorKind::KlvTooLarge,
        _ => match e.kind() {
            MuxErrorKind::InputMalformed => BindingErrorKind::InputMalformed,
            MuxErrorKind::ConfigInvalid => BindingErrorKind::ConfigInvalid,
            MuxErrorKind::InvalidUsage => BindingErrorKind::InvalidUsage,
            MuxErrorKind::Backpressure => BindingErrorKind::Backpressure,
            MuxErrorKind::Internal => BindingErrorKind::Internal,
            _ => BindingErrorKind::Internal,
        },
    }
}

impl From<MuxError> for BindingError {
    fn from(e: MuxError) -> Self {
        // No `unmapped` routing here: `MuxErrorKind::Internal` is a real
        // mapping target (table row 10), not a fallthrough.
        BindingError::new(kind_of_mux(&e), e.to_string())
    }
}

/// Every `DemuxError` variant, 1:1 (K3). Wildcard required (K7); the
/// kind-table rail greps every variant before it.
pub fn kind_of_demux(e: &DemuxError) -> BindingErrorKind {
    match e {
        DemuxError::Unrecoverable { .. } => BindingErrorKind::DemuxUnrecoverable,
        DemuxError::StrictRejection(_) => BindingErrorKind::DemuxStrictRejection,
        DemuxError::MalformedPsi { .. } => BindingErrorKind::DemuxMalformedPsi,
        DemuxError::MalformedPes { .. } => BindingErrorKind::DemuxMalformedPes,
        DemuxError::SyncBufExhausted { .. } => BindingErrorKind::DemuxSyncBufExhausted,
        _ => BindingErrorKind::Internal,
    }
}

impl From<DemuxError> for BindingError {
    fn from(e: DemuxError) -> Self {
        match kind_of_demux(&e) {
            BindingErrorKind::Internal => unmapped(BindingErrorKind::Internal, "DemuxError", &e),
            k => BindingError::new(k, e.to_string()),
        }
    }
}

/// The six KLV-decode buckets (K3), byte-for-byte the routing at
/// `bindings/python/src/klv.rs:97-121` / `bindings/jvm/src/error.rs:138-163`.
pub fn kind_of_klv_decode(e: &KlvDecodeError) -> BindingErrorKind {
    match e {
        KlvDecodeError::Truncated { .. }
        | KlvDecodeError::MalformedLength { .. }
        | KlvDecodeError::LengthOverflow { .. } => BindingErrorKind::KlvDecodeTruncatedSet,
        KlvDecodeError::UnexpectedUniversalLabel { .. } => {
            BindingErrorKind::KlvDecodeBadUniversalLabel
        }
        KlvDecodeError::ChecksumMismatch { .. } | KlvDecodeError::Crc32Mismatch { .. } => {
            BindingErrorKind::KlvDecodeChecksumMismatch
        }
        KlvDecodeError::DuplicateTag { .. } => BindingErrorKind::KlvDecodeDuplicateTag,
        KlvDecodeError::Tag2NotFirst
        | KlvDecodeError::Tag1NotLast
        | KlvDecodeError::MissingTag65
        | KlvDecodeError::St0102MissingRequiredTag { .. }
        | KlvDecodeError::St0903MissingRequiredTag { .. } => {
            BindingErrorKind::KlvDecodeMissingRequiredTag
        }
        KlvDecodeError::MalformedTag { .. }
        | KlvDecodeError::NonCanonicalLength { .. }
        | KlvDecodeError::NonCanonicalTag { .. }
        | KlvDecodeError::TrailingBytes { .. }
        | KlvDecodeError::BadTimeStampPackLength { .. }
        | KlvDecodeError::ReservedBitsInvalid { .. }
        | KlvDecodeError::St0903InvalidVTargetPack { .. }
        | KlvDecodeError::FieldError(_) => BindingErrorKind::KlvDecodeMalformedBytes,
        _ => BindingErrorKind::Internal,
    }
}

impl From<KlvDecodeError> for BindingError {
    fn from(e: KlvDecodeError) -> Self {
        match kind_of_klv_decode(&e) {
            BindingErrorKind::Internal => {
                unmapped(BindingErrorKind::Internal, "KlvDecodeError", &e)
            }
            k => BindingError::new(k, e.to_string()),
        }
    }
}

/// `KlvFieldError` on its own (the SRT umbrella carries one; the Python
/// klv module raises it directly): `TruncatedField` is a truncated set,
/// every other field failure is malformed bytes
/// (`bindings/python/src/klv.rs:140-141`, table rows 67 + 72).
pub fn kind_of_klv_field(e: &KlvFieldError) -> BindingErrorKind {
    match e {
        KlvFieldError::TruncatedField { .. } => BindingErrorKind::KlvDecodeTruncatedSet,
        KlvFieldError::OutOfRange { .. }
        | KlvFieldError::InvalidUtf8 { .. }
        | KlvFieldError::InvalidLength { .. }
        | KlvFieldError::InvalidUtf16 { .. }
        | KlvFieldError::InvalidCodepoint { .. }
        | KlvFieldError::UnsupportedImapbLength { .. }
        | KlvFieldError::InvalidImapbParams { .. } => BindingErrorKind::KlvDecodeMalformedBytes,
        _ => BindingErrorKind::Internal,
    }
}

impl From<KlvFieldError> for BindingError {
    fn from(e: KlvFieldError) -> Self {
        match kind_of_klv_field(&e) {
            BindingErrorKind::Internal => unmapped(BindingErrorKind::Internal, "KlvFieldError", &e),
            k => BindingError::new(k, e.to_string()),
        }
    }
}

/// Every `KlvEncodeError` variant, 1:1 (K3).
pub fn kind_of_klv_encode(e: &KlvEncodeError) -> BindingErrorKind {
    match e {
        KlvEncodeError::BufferTooSmall { .. } => BindingErrorKind::KlvEncodeBufferTooSmall,
        KlvEncodeError::RecordTooLarge => BindingErrorKind::KlvEncodeRecordTooLarge,
        KlvEncodeError::OutOfRange { .. } => BindingErrorKind::KlvEncodeOutOfRange,
        KlvEncodeError::StringTooLong { .. } => BindingErrorKind::KlvEncodeStringTooLong,
        KlvEncodeError::UnsupportedImapbLength { .. } => {
            BindingErrorKind::KlvEncodeUnsupportedImapbLength
        }
        KlvEncodeError::InvalidImapbParams { .. } => BindingErrorKind::KlvEncodeInvalidImapbParams,
        KlvEncodeError::MissingMandatoryItem { .. } => {
            BindingErrorKind::KlvEncodeMissingMandatoryItem
        }
        KlvEncodeError::ReservedTagInUnknown { .. } => {
            BindingErrorKind::KlvEncodeReservedTagInUnknown
        }
        KlvEncodeError::VTargetPackEmpty { .. } => BindingErrorKind::KlvEncodeVTargetPackEmpty,
        KlvEncodeError::DuplicateTargetId { .. } => BindingErrorKind::KlvEncodeDuplicateTargetId,
        KlvEncodeError::ForbiddenStandaloneOffset { .. } => {
            BindingErrorKind::KlvEncodeForbiddenStandaloneOffset
        }
        _ => BindingErrorKind::Internal,
    }
}

impl From<KlvEncodeError> for BindingError {
    fn from(e: KlvEncodeError) -> Self {
        match kind_of_klv_encode(&e) {
            BindingErrorKind::Internal => {
                unmapped(BindingErrorKind::Internal, "KlvEncodeError", &e)
            }
            k => BindingError::new(k, e.to_string()),
        }
    }
}

/// Every `CodecParseError` variant, 1:1 (K3).
pub fn kind_of_codec(e: &CodecParseError) -> BindingErrorKind {
    match e {
        CodecParseError::TruncatedRbsp { .. } => BindingErrorKind::CodecTruncatedRbsp,
        CodecParseError::InvalidGolomb { .. } => BindingErrorKind::CodecInvalidGolomb,
        CodecParseError::ReservedValue { .. } => BindingErrorKind::CodecReservedValue,
        CodecParseError::UnsupportedProfile { .. } => BindingErrorKind::CodecUnsupportedProfile,
        CodecParseError::DanglingSpsReference { .. } => BindingErrorKind::CodecDanglingSpsReference,
        CodecParseError::DanglingVpsReference { .. } => BindingErrorKind::CodecDanglingVpsReference,
        CodecParseError::EngineError(_) => BindingErrorKind::CodecEngineError,
        CodecParseError::InvalidLeb128 { .. } => BindingErrorKind::CodecInvalidLeb128,
        CodecParseError::BadSyncWord { .. } => BindingErrorKind::CodecBadSyncWord,
        CodecParseError::Truncated { .. } => BindingErrorKind::CodecTruncated,
        CodecParseError::Forbidden { .. } => BindingErrorKind::CodecForbidden,
        CodecParseError::UnsupportedFreeFormat { .. } => {
            BindingErrorKind::CodecUnsupportedFreeFormat
        }
        CodecParseError::InvalidLengthSize { .. } => BindingErrorKind::CodecInvalidLengthSize,
        CodecParseError::NalLengthOverflow { .. } => BindingErrorKind::CodecNalLengthOverflow,
        CodecParseError::BufferTooSmall { .. } => BindingErrorKind::CodecBufferTooSmall,
        _ => BindingErrorKind::Internal,
    }
}

impl From<CodecParseError> for BindingError {
    fn from(e: CodecParseError) -> Self {
        match kind_of_codec(&e) {
            BindingErrorKind::Internal => {
                unmapped(BindingErrorKind::Internal, "CodecParseError", &e)
            }
            k => BindingError::new(k, e.to_string()),
        }
    }
}

use crate::binding::owned::HandleState;
use crate::demux_receiver::{DemuxReceiverError, DemuxReceiverErrorSource};
use crate::mux_publisher::MuxPublisherError;
use crate::mux_sender::{MuxSenderError, MuxSenderErrorSource};
use crate::raw_receiver::{RawReceiverError, RawReceiverErrorSource};
use crate::raw_sender::{RawSenderError, RawSenderErrorSource};
use crate::receiver::{ReceiverError, ReceiverErrorSource};
use crate::sender::{SenderError, SenderErrorSource, TsFramingError};
use crate::shell_error::ShellErrorKind;
use tst_core::transport::TransportError;

impl From<HandleState> for BindingError {
    fn from(s: HandleState) -> Self {
        match s {
            HandleState::Closed => BindingError::new(BindingErrorKind::Closed, "handle is closed"),
            HandleState::Poisoned => {
                BindingError::new(BindingErrorKind::Internal, "handle mutex poisoned")
            }
            HandleState::Panicked { detail } => {
                BindingError::new(BindingErrorKind::PanicCaught, detail)
            }
        }
    }
}

/// `ShellErrorKind` is a projection of the table (spec §3.3). Exhaustive:
/// both enums live in this crate, so a new `ShellErrorKind` variant is a
/// compile error here until it is given a kind.
impl From<ShellErrorKind> for BindingErrorKind {
    fn from(k: ShellErrorKind) -> Self {
        match k {
            ShellErrorKind::ConfigInvalid => BindingErrorKind::ConfigInvalid,
            ShellErrorKind::InputMalformed => BindingErrorKind::InputMalformed,
            ShellErrorKind::Backpressure => BindingErrorKind::Backpressure,
            ShellErrorKind::TransportBroken => BindingErrorKind::Broken,
            ShellErrorKind::Closed => BindingErrorKind::Closed,
            ShellErrorKind::EndOfStream => BindingErrorKind::EndOfStream,
        }
    }
}

/// Kind of a `TransportError`. `ExplicitClose` projects to `Closed` (spec
/// §3.3, confirmed 2026-09-17): C has no cancelled code and its numbers are
/// frozen; the detail string carries the distinction. The wildcard is
/// required by `#[non_exhaustive]` (K7) — `scripts/check/rust/kind-table-coverage.sh`
/// fails if a `TransportError` variant is missing above it.
pub fn kind_of_transport(e: &TransportError) -> BindingErrorKind {
    match e {
        TransportError::Backpressure { .. } => BindingErrorKind::Backpressure,
        TransportError::Broken { .. } => BindingErrorKind::Broken,
        TransportError::Closed => BindingErrorKind::Closed,
        TransportError::TooLarge { .. } => BindingErrorKind::TooLarge,
        TransportError::ExplicitClose => BindingErrorKind::Closed,
        _ => BindingErrorKind::Internal,
    }
}

impl From<TransportError> for BindingError {
    fn from(e: TransportError) -> Self {
        let kind = kind_of_transport(&e);
        let detail = match &e {
            TransportError::Backpressure { msg, .. } | TransportError::Broken { msg, .. } => {
                msg.clone()
            }
            TransportError::Closed => String::from("transport closed"),
            TransportError::ExplicitClose => String::from("cancelled from another thread"),
            TransportError::TooLarge { .. } => e.to_string(),
            _ => format!("unmapped TransportError variant: {e:?}"),
        };
        BindingError::new(kind, detail)
    }
}

impl From<TsFramingError> for BindingError {
    fn from(e: TsFramingError) -> Self {
        let kind = match e {
            TsFramingError::SyncLost { .. } | TsFramingError::NoSyncAfterLimit { .. } => {
                BindingErrorKind::InputMalformed
            }
        };
        BindingError::new(kind, e.to_string())
    }
}

// Shell-error structs project by `source` (K6) — the same split Python and
// the JVM already apply — so `MuxSender::send_video(bad NAL)` is
// INVALID_NAL, not the coarser INPUT_MALFORMED the C shell path emitted.
// The source enums are in-crate, so these matches are exhaustive.

impl From<MuxSenderError> for BindingError {
    fn from(e: MuxSenderError) -> Self {
        match e.source {
            MuxSenderErrorSource::Mux(m) => m.into(),
            MuxSenderErrorSource::Transport(t) => t.into(),
        }
    }
}

impl From<SenderError> for BindingError {
    fn from(e: SenderError) -> Self {
        match e.source {
            SenderErrorSource::Framing(f) => f.into(),
            SenderErrorSource::Transport(t) => t.into(),
        }
    }
}

impl From<RawSenderError> for BindingError {
    fn from(e: RawSenderError) -> Self {
        match e.source {
            RawSenderErrorSource::Transport(t) => t.into(),
        }
    }
}

/// Receiver shells: `TransportError::Closed` means the PEER closed
/// (`shell_error::Direction::Recv`), which is `EndOfStream`; a caller's
/// cancel arrives as `ExplicitClose` and stays `Closed`.
fn recv_transport(t: TransportError) -> BindingError {
    match t {
        TransportError::Closed => {
            BindingError::new(BindingErrorKind::EndOfStream, "peer closed the stream")
        }
        other => other.into(),
    }
}

impl From<DemuxReceiverError> for BindingError {
    fn from(e: DemuxReceiverError) -> Self {
        match e.source {
            DemuxReceiverErrorSource::Transport(t) => recv_transport(t),
            DemuxReceiverErrorSource::Demux(d) => d.into(),
        }
    }
}

impl From<ReceiverError> for BindingError {
    fn from(e: ReceiverError) -> Self {
        match e.source {
            ReceiverErrorSource::Transport(t) => recv_transport(t),
        }
    }
}

impl From<RawReceiverError> for BindingError {
    fn from(e: RawReceiverError) -> Self {
        match e.source {
            RawReceiverErrorSource::Transport(t) => recv_transport(t),
        }
    }
}

/// `MuxPublisher<P>`'s error, generic over the sink error `E` (tst-hls's
/// `HlsError` in practice — its `From<HlsError>` lives in tst-hls, Task
/// A2.5, so this impl needs only `E: Into<BindingError>`). By source
/// (K6): a muxer rejection is the mux kind (today's Python folded it into
/// `HlsErrorKind.INVALID_CONFIG`), the sink's error is its own kind,
/// `Closed` (shell consumed via `finish`) is `Closed` (today's Python said
/// `FINISHED`), `LockPoisoned` is `Internal`. In-crate enum → exhaustive.
impl<E> From<MuxPublisherError<E>> for BindingError
where
    E: Into<BindingError> + std::error::Error + Send + Sync + 'static,
{
    fn from(e: MuxPublisherError<E>) -> Self {
        match e {
            MuxPublisherError::Mux(m) => m.into(),
            MuxPublisherError::Publisher(p) => p.into(),
            MuxPublisherError::Closed => {
                BindingError::new(BindingErrorKind::Closed, "MuxPublisher closed")
            }
            MuxPublisherError::LockPoisoned => {
                BindingError::new(BindingErrorKind::Internal, "MuxPublisher lock poisoned")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::binding::owned::HandleState;
    use crate::shell_error::ShellErrorKind;
    use tst_core::transport::{BrokenCause, TransportError};

    fn broken() -> TransportError {
        TransportError::Broken {
            msg: "peer reset".into(),
            errno_code: Some(2),
            cause: BrokenCause::Unspecified,
        }
    }

    #[test]
    fn handle_state_three_variants() {
        let c: BindingError = HandleState::Closed.into();
        assert_eq!(
            (c.kind, c.detail.as_str()),
            (BindingErrorKind::Closed, "handle is closed")
        );
        let p: BindingError = HandleState::Poisoned.into();
        assert_eq!(
            (p.kind, p.detail.as_str()),
            (BindingErrorKind::Internal, "handle mutex poisoned")
        );
        let k: BindingError = HandleState::Panicked {
            detail: "index out of bounds".into(),
        }
        .into();
        assert_eq!(
            (k.kind, k.detail.as_str()),
            (BindingErrorKind::PanicCaught, "index out of bounds")
        );
    }

    #[test]
    fn transport_error_five_variants() {
        let bp: BindingError = TransportError::Backpressure {
            msg: "queue full".into(),
            errno_code: Some(6),
        }
        .into();
        assert_eq!(bp.kind, BindingErrorKind::Backpressure);
        assert_eq!(bp.detail, "queue full");
        let bk: BindingError = broken().into();
        assert_eq!(bk.kind, BindingErrorKind::Broken);
        assert_eq!(bk.detail, "peer reset");
        let cl: BindingError = TransportError::Closed.into();
        assert_eq!(
            (cl.kind, cl.detail.as_str()),
            (BindingErrorKind::Closed, "transport closed")
        );
        let tl: BindingError = TransportError::TooLarge {
            len: 2000,
            max: 1316,
        }
        .into();
        assert_eq!(tl.kind, BindingErrorKind::TooLarge);
        assert_eq!(
            tl.detail,
            "message too large: 2000 bytes exceeds payload-size cap of 1316 bytes"
        );
        let ec: BindingError = TransportError::ExplicitClose.into();
        assert_eq!(
            (ec.kind, ec.detail.as_str()),
            (BindingErrorKind::Closed, "cancelled from another thread")
        );
    }

    #[test]
    fn shell_error_kind_is_a_projection_of_the_table() {
        use BindingErrorKind as K;
        let rows = [
            (ShellErrorKind::ConfigInvalid, K::ConfigInvalid),
            (ShellErrorKind::InputMalformed, K::InputMalformed),
            (ShellErrorKind::Backpressure, K::Backpressure),
            (ShellErrorKind::TransportBroken, K::Broken),
            (ShellErrorKind::Closed, K::Closed),
            (ShellErrorKind::EndOfStream, K::EndOfStream),
        ];
        for (s, k) in rows {
            assert_eq!(K::from(s), k, "{s:?}");
        }
    }

    #[test]
    fn shell_structs_project_by_source_and_receivers_keep_direction() {
        use crate::demux_receiver::DemuxReceiverError;
        use crate::receiver::ReceiverError;
        use crate::sender::{SenderError, TsFramingError};
        use crate::{MuxSenderError, RawReceiverError, RawSenderError};
        use tst_core::error::MuxError;

        let m: BindingError = MuxSenderError::from(MuxError::InvalidNal).into();
        assert_eq!(m.kind, BindingErrorKind::InvalidNal);
        let m: BindingError = MuxSenderError::from(TransportError::ExplicitClose).into();
        assert_eq!(m.kind, BindingErrorKind::Closed);
        let s: BindingError = SenderError::from(TsFramingError::SyncLost { offset: 7 }).into();
        assert_eq!(s.kind, BindingErrorKind::InputMalformed);
        let s: BindingError = SenderError::from(broken()).into();
        assert_eq!(s.kind, BindingErrorKind::Broken);
        let r: BindingError = RawSenderError::from(TransportError::Closed).into();
        assert_eq!(
            r.kind,
            BindingErrorKind::Closed,
            "sender side: Closed stays CLOSED"
        );
        // Receiver shells: peer EOS is END_OF_STREAM, a cancel is CLOSED.
        let d: BindingError = DemuxReceiverError::from(TransportError::Closed).into();
        assert_eq!(d.kind, BindingErrorKind::EndOfStream);
        let d: BindingError = DemuxReceiverError::from(TransportError::ExplicitClose).into();
        assert_eq!(d.kind, BindingErrorKind::Closed);
        let d: BindingError =
            DemuxReceiverError::from(tst_core::error::DemuxError::SyncBufExhausted {
                observed: 5,
                max: 4,
            })
            .into();
        assert_eq!(d.kind, BindingErrorKind::DemuxSyncBufExhausted);
        let r: BindingError = ReceiverError::from(TransportError::Closed).into();
        assert_eq!(r.kind, BindingErrorKind::EndOfStream);
        let r: BindingError = RawReceiverError::from(TransportError::Closed).into();
        assert_eq!(r.kind, BindingErrorKind::EndOfStream);
        let r: BindingError = RawReceiverError::from(broken()).into();
        assert_eq!(r.kind, BindingErrorKind::Broken);
    }

    #[test]
    fn mux_publisher_error_is_generic_over_the_sink_error() {
        use crate::mux_publisher::MuxPublisherError;
        use tst_core::error::MuxError;
        // Any sink error that already projects into the table works as `E`.
        let e: BindingError = MuxPublisherError::<TransportError>::Mux(MuxError::InvalidNal).into();
        assert_eq!(e.kind, BindingErrorKind::InvalidNal);
        let e: BindingError = MuxPublisherError::<TransportError>::Publisher(broken()).into();
        assert_eq!(
            (e.kind, e.detail.as_str()),
            (BindingErrorKind::Broken, "peer reset")
        );
        let e: BindingError = MuxPublisherError::<TransportError>::Closed.into();
        assert_eq!(
            (e.kind, e.detail.as_str()),
            (BindingErrorKind::Closed, "MuxPublisher closed")
        );
        let e: BindingError = MuxPublisherError::<TransportError>::LockPoisoned.into();
        assert_eq!(
            (e.kind, e.detail.as_str()),
            (BindingErrorKind::Internal, "MuxPublisher lock poisoned")
        );
    }

    /// CamelCase → SCREAMING_SNAKE, the one rule `name()` has to follow.
    /// Digits attach to the preceding word (`Av1Obu` → `AV1_OBU`,
    /// `Leb128` → `LEB128`, `St0102` → `ST0102`).
    fn screaming(camel: &str) -> String {
        let mut out = String::new();
        for (i, c) in camel.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_uppercase());
        }
        out
    }

    #[test]
    fn variant_name_is_screaming_snake_of_the_variant_for_every_kind() {
        assert_eq!(
            BindingErrorKind::ALL.len(),
            BindingErrorKind::VARIANT_IDENTS.len()
        );
        for (k, ident) in BindingErrorKind::ALL
            .iter()
            .zip(BindingErrorKind::VARIANT_IDENTS)
        {
            assert_eq!(
                k.variant_name(),
                screaming(ident),
                "variant_name() drifted from the variant ident {ident}"
            );
        }
    }

    #[test]
    fn name_is_variant_name_minus_exactly_one_domain_prefix() {
        const PREFIXES: &[&str] = &[
            "SRT_",
            "UDP_",
            "TCP_",
            "HLS_",
            "RIST_",
            "RTSP_",
            "RTP_",
            "DEMUX_",
            "KLV_DECODE_",
            "KLV_ENCODE_",
            "CODEC_",
        ];
        for k in BindingErrorKind::ALL {
            let stripped = PREFIXES
                .iter()
                .find_map(|p| k.variant_name().strip_prefix(p));
            match stripped {
                Some(m) => assert_eq!(k.name(), m, "{} is a domain kind", k.variant_name()),
                None => assert_eq!(
                    k.name(),
                    k.variant_name(),
                    "{} is cross-domain",
                    k.variant_name()
                ),
            }
            assert!(!k.name().is_empty());
        }
    }

    /// The C-frozen subset, pinned LITERALLY (tst-pipeline cannot import
    /// `TstError`; these numbers are `bindings/c/core/src/error.rs:19-206`).
    #[test]
    fn c_frozen_discriminants_match_tst_error() {
        use BindingErrorKind as K;
        let frozen: &[(K, i32)] = &[
            (K::ConfigInvalid, -1),
            (K::InvalidNal, -2),
            (K::InputMalformed, -3),
            (K::Backpressure, -4),
            (K::KlvTooLarge, -5),
            (K::TooLarge, -6),
            (K::Closed, -7),
            (K::Broken, -8),
            (K::InvalidUsage, -9),
            (K::Internal, -10),
            (K::PanicCaught, -11),
            (K::EndOfStream, -12),
            (K::NotAvailable, -13),
            (K::NotFound, -14),
            (K::RtspProtocol, -16),
            (K::RtspAuthFailed, -17),
            (K::RtspAuthRequired, -18),
            (K::RtspNotFound, -19),
            (K::RtspUnsupportedTransport, -20),
            (K::RtspTls, -21),
            (K::RtspIo, -22),
            (K::RtspTimeout, -23),
            (K::RtspServer, -24),
            (K::RtspMount, -25),
            (K::UdpIo, -26),
            (K::UdpInvalidConfig, -27),
            (K::TcpIo, -30),
            (K::TcpInvalidConfig, -31),
            (K::TcpConnectTimeout, -32),
            (K::TcpTls, -33),
            (K::HlsIo, -34),
            (K::HlsInvalidConfig, -35),
            (K::HlsFinished, -36),
            (K::HlsTls, -37),
            (K::RistFfi, -38),
            (K::RistInvalidConfig, -39),
            (K::RistEncryptionDisabled, -41),
            (K::InvalidAv1Obu, -44),
            (K::MispTime, -45),
            (K::MispTimeMalformed, -46),
            (K::WrongType, -47),
        ];
        assert_eq!(frozen.len(), 41);
        for (k, code) in frozen {
            assert_eq!(k.c_code(), *code, "{}", k.name());
            assert_eq!(
                k.c_projection(),
                *code,
                "{} is C-numbered: projection == code",
                k.name()
            );
            assert!(k.is_c_frozen());
        }
        // Reserved C codes that nothing produces are NOT variants (K2).
        for reserved in [-15, -28, -29, -40, -42, -43, -48] {
            assert!(
                BindingErrorKind::ALL.iter().all(|k| k.c_code() != reserved),
                "{reserved} must not be a discriminant"
            );
        }
    }

    #[test]
    fn new_kinds_are_numbered_from_minus_49_in_table_order_and_project_onto_frozen_codes() {
        let new: Vec<_> = BindingErrorKind::ALL
            .iter()
            .filter(|k| !k.is_c_frozen())
            .collect();
        assert_eq!(new.len(), 57);
        for (i, k) in new.iter().enumerate() {
            assert_eq!(k.c_code(), -49 - i as i32, "{} out of order", k.name());
            assert!(
                (-48..=-1).contains(&k.c_projection()),
                "{} projects outside TST_E",
                k.name()
            );
        }
        assert_eq!(BindingErrorKind::ALL.len(), 98);
    }

    #[test]
    fn discriminants_and_names_are_unique() {
        let mut codes: Vec<i32> = BindingErrorKind::ALL.iter().map(|k| k.c_code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), BindingErrorKind::ALL.len());
        let mut names: Vec<&str> = BindingErrorKind::ALL
            .iter()
            .map(|k| k.variant_name())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            BindingErrorKind::ALL.len(),
            "variant_name() must be unique (name() is not: IO repeats per domain)"
        );
    }

    #[test]
    fn binding_error_displays_its_detail() {
        let e = BindingError::new(BindingErrorKind::Closed, "handle is closed");
        assert_eq!(e.to_string(), "handle is closed");
        assert_eq!(e.kind, BindingErrorKind::Closed);
    }

    #[test]
    fn mux_error_four_precise_kinds_and_kind_fallback() {
        use BindingErrorKind as K;
        use tst_core::error::MuxError;
        use tst_core::mpegts::mux::StreamKind;
        assert_eq!(kind_of_mux(&MuxError::InvalidNal), K::InvalidNal);
        assert_eq!(kind_of_mux(&MuxError::InvalidAv1Obu), K::InvalidAv1Obu);
        assert_eq!(
            kind_of_mux(&MuxError::KlvTooLarge {
                size: 70000,
                max: 65535
            }),
            K::KlvTooLarge
        );
        assert_eq!(
            kind_of_mux(&MuxError::BufferFull {
                capacity_packets: 1
            }),
            K::Backpressure
        );
        assert_eq!(
            kind_of_mux(&MuxError::AudioTooLarge { size: 2, max: 1 }),
            K::InputMalformed
        );
        assert_eq!(kind_of_mux(&MuxError::InvalidConfig("x")), K::ConfigInvalid);
        assert_eq!(
            kind_of_mux(&MuxError::InvalidStreamHandle {
                kind: StreamKind::Video,
                index: 9
            }),
            K::InvalidUsage
        );
        let e: BindingError = MuxError::InvalidNal.into();
        assert_eq!(e.detail, MuxError::InvalidNal.to_string());
    }

    #[test]
    fn demux_error_five_variants() {
        use BindingErrorKind as K;
        use tst_core::error::DemuxError;
        assert_eq!(
            kind_of_demux(&DemuxError::Unrecoverable { after_bytes: 6016 }),
            K::DemuxUnrecoverable
        );
        assert_eq!(
            kind_of_demux(&DemuxError::StrictRejection("x".into())),
            K::DemuxStrictRejection
        );
        assert_eq!(
            kind_of_demux(&DemuxError::MalformedPsi {
                pid: 0,
                reason: "r"
            }),
            K::DemuxMalformedPsi
        );
        assert_eq!(
            kind_of_demux(&DemuxError::MalformedPes {
                pid: 0,
                reason: "r"
            }),
            K::DemuxMalformedPes
        );
        assert_eq!(
            kind_of_demux(&DemuxError::SyncBufExhausted {
                observed: 5,
                max: 4
            }),
            K::DemuxSyncBufExhausted
        );
    }

    #[test]
    fn klv_decode_buckets_match_the_python_table() {
        use BindingErrorKind as K;
        use tst_core::error::KlvDecodeError as E;
        assert_eq!(
            kind_of_klv_decode(&E::Truncated {
                offset: 0,
                needed: 2,
                have: 1
            }),
            K::KlvDecodeTruncatedSet
        );
        assert_eq!(
            kind_of_klv_decode(&E::LengthOverflow { value: u64::MAX }),
            K::KlvDecodeTruncatedSet
        );
        assert_eq!(
            kind_of_klv_decode(&E::ChecksumMismatch {
                expected: 1,
                found: 2
            }),
            K::KlvDecodeChecksumMismatch
        );
        assert_eq!(
            kind_of_klv_decode(&E::Crc32Mismatch {
                expected: 1,
                found: 2
            }),
            K::KlvDecodeChecksumMismatch
        );
        assert_eq!(
            kind_of_klv_decode(&E::DuplicateTag { tag: 2, offset: 9 }),
            K::KlvDecodeDuplicateTag
        );
        assert_eq!(
            kind_of_klv_decode(&E::MissingTag65),
            K::KlvDecodeMissingRequiredTag
        );
        assert_eq!(
            kind_of_klv_decode(&E::St0102MissingRequiredTag { tag: 1 }),
            K::KlvDecodeMissingRequiredTag
        );
        assert_eq!(
            kind_of_klv_decode(&E::TrailingBytes { len: 3 }),
            K::KlvDecodeMalformedBytes
        );
        assert_eq!(
            kind_of_klv_decode(&E::NonCanonicalTag { offset: 1 }),
            K::KlvDecodeMalformedBytes
        );
    }

    #[test]
    fn klv_encode_and_codec_are_one_to_one() {
        use BindingErrorKind as K;
        use tst_core::codec::CodecParseError as C;
        use tst_core::error::KlvEncodeError as E;
        assert_eq!(
            kind_of_klv_encode(&E::RecordTooLarge),
            K::KlvEncodeRecordTooLarge
        );
        assert_eq!(
            kind_of_klv_encode(&E::OutOfRange {
                tag: 13,
                value: 91.0,
                min: -90.0,
                max: 90.0,
                hint: None
            }),
            K::KlvEncodeOutOfRange
        );
        assert_eq!(
            kind_of_klv_encode(&E::VTargetPackEmpty { target_id: 1 }),
            K::KlvEncodeVTargetPackEmpty
        );
        assert_eq!(
            kind_of_codec(&C::TruncatedRbsp {
                offset_bits: 1,
                needed_bits: 2
            }),
            K::CodecTruncatedRbsp
        );
        assert_eq!(
            kind_of_codec(&C::EngineError("x".into())),
            K::CodecEngineError
        );
        assert_eq!(
            kind_of_codec(&C::InvalidLengthSize { got: 3 }),
            K::CodecInvalidLengthSize
        );
        assert_eq!(
            kind_of_codec(&C::NalLengthOverflow {
                nal_len: 70000,
                length_size: 2
            }),
            K::CodecNalLengthOverflow
        );
        assert_eq!(
            kind_of_codec(&C::BufferTooSmall { needed: 9, have: 1 }),
            K::CodecBufferTooSmall
        );
        assert_eq!(K::CodecInvalidLengthSize.c_projection(), -1);
        assert_eq!(K::CodecNalLengthOverflow.c_projection(), -6);
    }
}
