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

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::binding::owned::{CloseFailure, HandleState};
use crate::demux_receiver::{DemuxReceiverError, DemuxReceiverErrorSource};
#[cfg(feature = "std")]
use crate::mux_publisher::MuxPublisherError;
use crate::mux_sender::{MuxSenderError, MuxSenderErrorSource};
use crate::raw_receiver::{RawReceiverError, RawReceiverErrorSource};
use crate::raw_sender::{RawSenderError, RawSenderErrorSource};
use crate::receiver::{ReceiverError, ReceiverErrorSource};
use crate::sender::{SenderError, SenderErrorSource, TsFramingError};
use crate::shell_error::ShellErrorKind;
use tst_core::codec::CodecParseError;
use tst_core::error::{
    DemuxError, KlvDecodeError, KlvEncodeError, KlvFieldError, MuxError, MuxErrorKind,
};
use tst_core::transport::{BrokenCause, TransportError};

/// One row per kind: `Variant = discriminant => "VARIANT_NAME", "NAME", c_projection;`
///
/// The row format is what the `--print-kinds` bin and the unit tests pin:
/// VARIANT_NAME must be SCREAMING_SNAKE of the variant, NAME (what the
/// bindings resolve) must be VARIANT_NAME minus exactly one domain prefix
/// (or VARIANT_NAME itself), c_projection must be a TST_E
/// code in -48..=-1 and must equal the discriminant for C-numbered kinds.
macro_rules! kinds {
    ( $( $(#[doc = $doc:literal])* $variant:ident = $code:literal => $name:literal, $member:literal, $proj:literal ; )+ ) => {
        /// The one error-kind table shared by every binding.
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
            $( $(#[doc = $doc])* $variant = $code, )+
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

            /// The kind's name as the bindings resolve it (the kind rule):
            /// [`Self::variant_name`] with the domain prefix removed
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
            pub const fn c_projection(&self) -> i32 {
                match self { $( Self::$variant => $proj, )+ }
            }

            /// The kind's description: the doc text of its table row. The
            /// generated Python and Java kind enums render it on each member.
            pub fn doc(&self) -> &'static str {
                match self { $( Self::$variant => concat!($($doc, "\n"),*), )+ }
            }
        }
    };
}

kinds! {
    /// Configuration rejected before anything was built: an invalid config, URL, option or address.
    ///
    /// `ShellErrorKind::ConfigInvalid`, `MuxErrorKind::ConfigInvalid`, SRT address/option/URL errors.
    ConfigInvalid = -1 => "CONFIG_INVALID", "CONFIG_INVALID", -1;
    /// A pushed video access unit carried no Annex-B start code, so it is not a NAL.
    ///
    /// `MuxError::InvalidNal`.
    InvalidNal = -2 => "INVALID_NAL", "INVALID_NAL", -2;
    /// The input is malformed for this call: framing was lost, or a payload was over its size cap.
    ///
    /// `ShellErrorKind::InputMalformed`, TS framing, over-size audio/subtitle/data pushes.
    InputMalformed = -3 => "INPUT_MALFORMED", "INPUT_MALFORMED", -3;
    /// Transient refusal: the transport or muxer is alive but cannot take or produce the call now (full send queue or muxer buffer, or a receive deadline that expired). Retry; the handle stays open.
    ///
    /// `TransportError::Backpressure`, `MuxError::BufferFull`, SRT `QueueFull`.
    Backpressure = -4 => "BACKPRESSURE", "BACKPRESSURE", -4;
    /// A KLV record exceeded the PES length cap.
    ///
    /// `MuxError::KlvTooLarge`.
    KlvTooLarge = -5 => "KLV_TOO_LARGE", "KLV_TOO_LARGE", -5;
    /// The payload exceeds the transport's per-send ceiling.
    ///
    /// `TransportError::TooLarge`, SRT `PayloadTooLarge` / `BufferTooSmall`.
    TooLarge = -6 => "TOO_LARGE", "TOO_LARGE", -6;
    /// The handle or transport was closed: by `close()` on this object, by a cancel or close from another thread while the call was parked (detail "cancelled from another thread"), or the call came after the handle was closed or consumed.
    ///
    /// `TransportError::{Closed on a sender, ExplicitClose}`, `HandleState::Closed`, `TcpError::Closed`, SRT `ListenerClosed` / `SocketClosed`.
    Closed = -7 => "CLOSED", "CLOSED", -7;
    /// The transport is dead (a send or receive I/O failure, never a cancel); reopen it.
    ///
    /// `TransportError::Broken`, SRT `ConnectionBroken`.
    Broken = -8 => "BROKEN", "BROKEN", -8;
    /// API misuse on a successfully-built muxer: a stream handle from another muxer, an ambiguous-target shorthand on a multi-stream muxer, an unknown program reference, or an out-of-range descriptor or index.
    ///
    /// `MuxErrorKind::InvalidUsage`.
    InvalidUsage = -9 => "INVALID_USAGE", "INVALID_USAGE", -9;
    /// Library-internal failure: an invariant tripped that should not occur in well-formed use (file an issue with reproduction bytes), or a poisoned handle lock.
    Internal = -10 => "INTERNAL", "INTERNAL", -10;
    /// A panic caught at the binding boundary (`HandleState::Panicked`).
    PanicCaught = -11 => "PANIC_CAUGHT", "PANIC_CAUGHT", -11;
    /// The receive loop reached end of stream: the peer closed cleanly. The receiver is dead; later calls report `CLOSED`. Iterating receivers end the iteration instead of raising it.
    ///
    /// `ShellErrorKind::EndOfStream`; the C ABI returns it as `TST_E_END_OF_STREAM` (-12).
    EndOfStream = -12 => "END_OF_STREAM", "END_OF_STREAM", -12;
    /// Transient: value not available right now (C managed-stats getters mid-reconnect).
    NotAvailable = -13 => "NOT_AVAILABLE", "NOT_AVAILABLE", -13;
    /// Persistent: key never observed (C per-PID getters).
    NotFound = -14 => "NOT_FOUND", "NOT_FOUND", -14;
    /// The AV1 OBU stream could not be framed: it is not a well-formed OBU stream.
    ///
    /// `MuxError::InvalidAv1Obu`.
    InvalidAv1Obu = -44 => "INVALID_AV1_OBU", "INVALID_AV1_OBU", -44;
    /// The MISP ST 0604 timestamp SEI could not be built or inserted for this codec or carriage (the `push_video_misp_to` family).
    ///
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
    /// RTSP server lifecycle misuse: started twice, not started, or already shut down.
    ///
    /// `AlreadyStarted` / `NotStarted` / `Shutdown`.
    RtspServer = -24 => "RTSP_SERVER", "SERVER", -24;
    /// RTSP mount failure: mount path, multicast group, duplicate mount or mount config.
    ///
    /// `MountError`.
    RtspMount = -25 => "RTSP_MOUNT", "MOUNT", -25;
    /// UDP socket I/O failure (bind, connect or socket option).
    ///
    /// `UdpError::Io`.
    UdpIo = -26 => "UDP_IO", "IO", -26;
    /// The UDP configuration was rejected.
    ///
    /// `UdpError::InvalidConfig`.
    UdpInvalidConfig = -27 => "UDP_INVALID_CONFIG", "INVALID_CONFIG", -27;
    /// TCP socket I/O failure.
    ///
    /// `TcpError::Io`.
    TcpIo = -30 => "TCP_IO", "IO", -30;
    /// The TCP configuration was rejected.
    ///
    /// `TcpError::InvalidConfig`.
    TcpInvalidConfig = -31 => "TCP_INVALID_CONFIG", "INVALID_CONFIG", -31;
    /// The TCP connect did not complete within the connect timeout.
    ///
    /// `TcpError::ConnectTimeout`.
    TcpConnectTimeout = -32 => "TCP_CONNECT_TIMEOUT", "CONNECT_TIMEOUT", -32;
    /// TLS setup or handshake failed on a TCP connection.
    ///
    /// `TcpError::Tls`.
    TcpTls = -33 => "TCP_TLS", "TLS", -33;
    /// Segment or playlist file I/O failed.
    ///
    /// `HlsError::Io`.
    HlsIo = -34 => "HLS_IO", "IO", -34;
    /// The builder's configuration was rejected at `build()`.
    ///
    /// `HlsError::InvalidConfig`.
    HlsInvalidConfig = -35 => "HLS_INVALID_CONFIG", "INVALID_CONFIG", -35;
    /// The publisher was already finished and takes no more data.
    ///
    /// `HlsError::Finished`.
    HlsFinished = -36 => "HLS_FINISHED", "FINISHED", -36;
    /// TLS setup failed (unreadable certificate or key, handshake).
    ///
    /// `HlsError::Tls`.
    HlsTls = -37 => "HLS_TLS", "TLS", -37;
    /// A librist call failed.
    ///
    /// `RistError::Ffi`.
    RistFfi = -38 => "RIST_FFI", "FFI", -38;
    /// The RIST configuration was rejected.
    ///
    /// `RistError::InvalidConfig`.
    RistInvalidConfig = -39 => "RIST_INVALID_CONFIG", "INVALID_CONFIG", -39;
    /// Encryption was requested on a build without RIST encryption support.
    ///
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
    /// The `udp://` URL could not be parsed.
    ///
    /// `UdpError::Url`.
    UdpUrl = -53 => "UDP_URL", "URL", -27;
    /// The `tcp://` or `tcps://` URL could not be parsed.
    ///
    /// `TcpError::Url`.
    TcpUrl = -54 => "TCP_URL", "URL", -31;
    /// TLS was requested on a build without the `tls` feature.
    ///
    /// `TcpError::TlsDisabled`.
    TcpTlsDisabled = -55 => "TCP_TLS_DISABLED", "TLS_DISABLED", -33;
    /// `from_url` could not parse the HLS URL.
    ///
    /// `HlsUrlError` (`HlsPublisherBuilder::from_url`).
    HlsUrl = -56 => "HLS_URL", "URL", -35;
    /// The built-in HTTP server could not bind.
    ///
    /// `HlsError::BindFailed`.
    HlsBindFailed = -57 => "HLS_BIND_FAILED", "BIND_FAILED", -34;
    /// `push_ts` input is not a whole multiple of 188 bytes.
    ///
    /// `HlsError::UnalignedPushTs`.
    HlsUnalignedPushTs = -58 => "HLS_UNALIGNED_PUSH_TS", "UNALIGNED_PUSH_TS", -35;
    /// `enable_tls` was called on a build without the `tls` feature.
    ///
    /// `HlsError::TlsDisabled`.
    HlsTlsDisabled = -59 => "HLS_TLS_DISABLED", "TLS_DISABLED", -37;
    /// The `rist://` URL could not be parsed.
    ///
    /// `RistError::Url`.
    RistUrl = -60 => "RIST_URL", "URL", -39;
    /// librist could not create its context.
    ///
    /// `RistError::ContextCreateFailed`.
    RistContextCreateFailed = -61 => "RIST_CONTEXT_CREATE_FAILED", "CONTEXT_CREATE_FAILED", -38;
    /// librist could not create the peer.
    ///
    /// `RistError::PeerCreateFailed`.
    RistPeerCreateFailed = -62 => "RIST_PEER_CREATE_FAILED", "PEER_CREATE_FAILED", -38;
    /// The URL carries `?pt=` (elementary RTP): use the H.264 receiver, not the MP2T transport.
    ///
    /// tst-rtp `ConnectError::PayloadTypeParam`.
    RtpPayloadTypeParam = -63 => "RTP_PAYLOAD_TYPE_PARAM", "PAYLOAD_TYPE_PARAM", -15;
    /// The H.264 receiver requires a dynamic payload type in `?pt=` on the URL.
    ///
    /// tst-rtp `ConnectError::MissingPayloadTypeParam`.
    RtpMissingPayloadTypeParam = -64 => "RTP_MISSING_PAYLOAD_TYPE_PARAM", "MISSING_PAYLOAD_TYPE_PARAM", -15;
    /// The `rtp://` or `rtsp://` URL could not be parsed.
    ///
    /// tst-rtp `ConnectError::Url`.
    RtpUrl = -65 => "RTP_URL", "URL", -15;
    /// The host is not a literal IPv4 or IPv6 address. The RTP transport does no DNS resolution: pre-resolve it and pass the literal.
    ///
    /// tst-rtp `ConnectError::HostNotLiteral`.
    RtpHostNotLiteral = -66 => "RTP_HOST_NOT_LITERAL", "HOST_NOT_LITERAL", -15;
    /// UDP socket error: an OS-level bind, connect or setsockopt failure.
    ///
    /// tst-rtp `ConnectError::Io`.
    RtpIo = -67 => "RTP_IO", "IO", -15;
    /// The multicast interface is unsupported here, typically because the platform needs a different form (IPv6 needs a scope-id integer).
    ///
    /// tst-rtp `ConnectError::IfaceUnsupported`.
    RtpIfaceUnsupported = -68 => "RTP_IFACE_UNSUPPORTED", "IFACE_UNSUPPORTED", -15;
    /// One sync-search window held no packet boundary; the scanned bytes are discarded and the next feed starts a fresh search.
    ///
    /// `DemuxError::Unrecoverable`.
    DemuxUnrecoverable = -69 => "DEMUX_UNRECOVERABLE", "UNRECOVERABLE", -3;
    /// A strict-mode policy escalated a non-conformance to a fatal error; the message names the specific issue.
    ///
    /// `DemuxError::StrictRejection`.
    DemuxStrictRejection = -70 => "DEMUX_STRICT_REJECTION", "STRICT_REJECTION", -3;
    /// A PES header declared a length too short to contain its own claimed flags, so the reassembler cannot make forward progress.
    ///
    /// `DemuxError::MalformedPes`.
    DemuxMalformedPes = -72 => "DEMUX_MALFORMED_PES", "MALFORMED_PES", -3;
    /// The pre-sync buffer filled without finding the four confirming sync bytes; feed in smaller chunks or raise `sync_buf_cap`.
    ///
    /// `DemuxError::SyncBufExhausted`.
    DemuxSyncBufExhausted = -73 => "DEMUX_SYNC_BUF_EXHAUSTED", "SYNC_BUF_EXHAUSTED", -6;
    /// KLV set truncated, or a BER length malformed or overflowing.
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
    /// The output buffer cannot hold the encoded record.
    ///
    /// `KlvEncodeError::BufferTooSmall`.
    KlvEncodeBufferTooSmall = -80 => "KLV_ENCODE_BUFFER_TOO_SMALL", "BUFFER_TOO_SMALL", -10;
    /// The encoded record exceeds the KLV size limit.
    ///
    /// `KlvEncodeError::RecordTooLarge`.
    KlvEncodeRecordTooLarge = -81 => "KLV_ENCODE_RECORD_TOO_LARGE", "RECORD_TOO_LARGE", -10;
    /// A value is outside the range the spec declares for its item.
    ///
    /// `KlvEncodeError::OutOfRange`.
    KlvEncodeOutOfRange = -82 => "KLV_ENCODE_OUT_OF_RANGE", "OUT_OF_RANGE", -10;
    /// A string value is longer than its item allows.
    ///
    /// `KlvEncodeError::StringTooLong`.
    KlvEncodeStringTooLong = -83 => "KLV_ENCODE_STRING_TOO_LONG", "STRING_TOO_LONG", -10;
    /// The IMAPB mapping length is outside the supported range 1 to 8 bytes.
    ///
    /// `KlvEncodeError::UnsupportedImapbLength`.
    KlvEncodeUnsupportedImapbLength = -84 => "KLV_ENCODE_UNSUPPORTED_IMAPB_LENGTH", "UNSUPPORTED_IMAPB_LENGTH", -10;
    /// The IMAPB parameters violate the ST 1201.5 precondition that min is below max.
    ///
    /// `KlvEncodeError::InvalidImapbParams`.
    KlvEncodeInvalidImapbParams = -85 => "KLV_ENCODE_INVALID_IMAPB_PARAMS", "INVALID_IMAPB_PARAMS", -10;
    /// A mandatory item is missing under a strict-compliance encoder.
    ///
    /// `KlvEncodeError::MissingMandatoryItem`.
    KlvEncodeMissingMandatoryItem = -86 => "KLV_ENCODE_MISSING_MANDATORY_ITEM", "MISSING_MANDATORY_ITEM", -10;
    /// An unknown-tag entry used a tag the typed set reserves.
    ///
    /// `KlvEncodeError::ReservedTagInUnknown`.
    KlvEncodeReservedTagInUnknown = -87 => "KLV_ENCODE_RESERVED_TAG_IN_UNKNOWN", "RESERVED_TAG_IN_UNKNOWN", -10;
    /// An ST 0903 VTarget Pack carried no items after its target id; the error's tag is the target id.
    ///
    /// `KlvEncodeError::VTargetPackEmpty`.
    KlvEncodeVTargetPackEmpty = -88 => "KLV_ENCODE_V_TARGET_PACK_EMPTY", "V_TARGET_PACK_EMPTY", -10;
    /// An ST 0903 vTargetSeries holds two VTarget Packs with the same target id.
    ///
    /// `KlvEncodeError::DuplicateTargetId`.
    KlvEncodeDuplicateTargetId = -89 => "KLV_ENCODE_DUPLICATE_TARGET_ID", "DUPLICATE_TARGET_ID", -10;
    /// A standalone ST 0903 VMTI set carries a parent-relative offset tag, which the standalone form forbids.
    ///
    /// `KlvEncodeError::ForbiddenStandaloneOffset`.
    KlvEncodeForbiddenStandaloneOffset = -90 => "KLV_ENCODE_FORBIDDEN_STANDALONE_OFFSET", "FORBIDDEN_STANDALONE_OFFSET", -10;
    /// The bitstream cursor walked past the end of the input.
    ///
    /// `CodecParseError::TruncatedRbsp`.
    CodecTruncatedRbsp = -91 => "CODEC_TRUNCATED_RBSP", "TRUNCATED_RBSP", -10;
    /// Malformed Exp-Golomb code (a leading-zero run over 32, or a missing terminator bit).
    ///
    /// `CodecParseError::InvalidGolomb`.
    CodecInvalidGolomb = -92 => "CODEC_INVALID_GOLOMB", "INVALID_GOLOMB", -10;
    /// A field carries a spec-reserved value that affects framing or downstream parsing.
    ///
    /// `CodecParseError::ReservedValue`.
    CodecReservedValue = -93 => "CODEC_RESERVED_VALUE", "RESERVED_VALUE", -10;
    /// A profile whose framing semantics the parser cannot extract fields for.
    ///
    /// `CodecParseError::UnsupportedProfile`.
    CodecUnsupportedProfile = -94 => "CODEC_UNSUPPORTED_PROFILE", "UNSUPPORTED_PROFILE", -10;
    /// The underlying engine failed in a way that maps to no finer kind; a variant a newer library adds also lands here.
    ///
    /// `CodecParseError::EngineError`.
    CodecEngineError = -97 => "CODEC_ENGINE_ERROR", "ENGINE_ERROR", -10;
    /// A LEB128 value (AV1 OBU size) ran past 8 bytes or set the continuation bit on the 8th.
    ///
    /// `CodecParseError::InvalidLeb128`.
    CodecInvalidLeb128 = -98 => "CODEC_INVALID_LEB128", "INVALID_LEB128", -10;
    /// An audio frame buffer does not start with the codec's sync word.
    ///
    /// `CodecParseError::BadSyncWord`.
    CodecBadSyncWord = -99 => "CODEC_BAD_SYNC_WORD", "BAD_SYNC_WORD", -10;
    /// A frame parser ran out of bytes while reading a header or frame body (byte-oriented; `TRUNCATED_RBSP` is the bit-oriented video case).
    ///
    /// `CodecParseError::Truncated`.
    CodecTruncated = -100 => "CODEC_TRUNCATED", "TRUNCATED", -10;
    /// A field carries a bit pattern the spec marks as never valid.
    ///
    /// `CodecParseError::Forbidden`.
    CodecForbidden = -101 => "CODEC_FORBIDDEN", "FORBIDDEN", -10;
    /// An MPEG audio header signals free-format (bitrate index 0), whose frame length this parser does not discover.
    ///
    /// `CodecParseError::UnsupportedFreeFormat`.
    CodecUnsupportedFreeFormat = -102 => "CODEC_UNSUPPORTED_FREE_FORMAT", "UNSUPPORTED_FREE_FORMAT", -10;
    /// The AVCC/HVCC length-prefix size is not 1, 2 or 4 bytes.
    ///
    /// `CodecParseError::InvalidLengthSize` (C: -1, `codec_framing.rs:44`).
    CodecInvalidLengthSize = -103 => "CODEC_INVALID_LENGTH_SIZE", "INVALID_LENGTH_SIZE", -1;
    /// A NAL length does not fit the configured length-prefix size.
    ///
    /// `CodecParseError::NalLengthOverflow` (C: -6, `codec_framing.rs:45`).
    CodecNalLengthOverflow = -104 => "CODEC_NAL_LENGTH_OVERFLOW", "NAL_LENGTH_OVERFLOW", -6;
    /// The caller's output buffer cannot hold the converted stream.
    ///
    /// `CodecParseError::BufferTooSmall` (C: -4, the two-call size-query idiom).
    CodecBufferTooSmall = -105 => "CODEC_BUFFER_TOO_SMALL", "BUFFER_TOO_SMALL", -4;
}

impl BindingErrorKind {
    /// The discriminant (`*self as i32`); the frozen `TST_E_*` number for the
    /// C-numbered kinds, ≤ −49 otherwise.
    pub const fn c_code(&self) -> i32 {
        *self as i32
    }

    /// `true` when the discriminant is a frozen C code (`-48..=-1`).
    pub const fn is_c_frozen(&self) -> bool {
        self.c_code() >= -48
    }
}

/// A binding error domain: one exception class per domain in each host
/// language (`SrtError` in Python, `SrtException` in the JVM, ...).
///
/// [`Domain::kinds`] is the one membership table: the Python raise path,
/// the JVM throw path, the generated Python and Java kind enums and the
/// kind-equivalence TSV all read it. Its order is the host enums' member
/// order.
///
/// **Stability: Provisional** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Domain {
    /// Host classes: `SrtError` / `SrtErrorKind`, `SrtException`.
    Srt,
    /// Host classes: `RtpError` / `RtpErrorKind`, `RtpException`.
    Rtp,
    /// Host classes: `RtspError` / `RtspErrorKind`, `RtspException`.
    Rtsp,
    /// Host classes: `UdpError` / `UdpErrorKind` (Python only).
    Udp,
    /// Host classes: `TcpError` / `TcpErrorKind` (Python only).
    Tcp,
    /// Host classes: `HlsError` / `HlsErrorKind`, `HlsException`.
    Hls,
    /// Host classes: `RistError` / `RistErrorKind` (Python only).
    Rist,
    /// Host classes: `MuxError` / `MuxErrorKind`, `MuxException`.
    Mux,
    /// Host classes: `DemuxError` / `DemuxErrorKind`, `DemuxException`.
    Demux,
    /// Host classes: `KlvError` / `KlvErrorKind`, `KlvDecodeException`.
    KlvDecode,
    /// Host classes: `KlvEncodeError` / `KlvEncodeErrorKind`, `KlvEncodeException`.
    KlvEncode,
    /// Host classes: `CodecError` / `CodecErrorKind`, `CodecParseException`.
    Codec,
}

impl Domain {
    /// Every domain, in declaration order.
    pub const ALL: &'static [Domain] = &[
        Domain::Srt,
        Domain::Rtp,
        Domain::Rtsp,
        Domain::Udp,
        Domain::Tcp,
        Domain::Hls,
        Domain::Rist,
        Domain::Mux,
        Domain::Demux,
        Domain::KlvDecode,
        Domain::KlvEncode,
        Domain::Codec,
    ];

    /// The kinds this domain's host enums carry, in host member order.
    pub fn kinds(self) -> &'static [BindingErrorKind] {
        use BindingErrorKind as K;
        match self {
            Domain::Srt => &[
                K::SrtConnectFailed,
                K::SrtAcceptFailed,
                K::Backpressure,
                K::SrtTimeout,
                K::Closed,
                K::Broken,
                K::ConfigInvalid,
                K::SrtIo,
                K::TooLarge,
                K::InputMalformed,
                K::EndOfStream,
            ],
            Domain::Rtp => &[
                K::Broken,
                K::TooLarge,
                K::Closed,
                K::Backpressure,
                K::RtpPayloadTypeParam,
                K::RtpMissingPayloadTypeParam,
                K::RtpUrl,
                K::RtpHostNotLiteral,
                K::RtpIo,
                K::RtpIfaceUnsupported,
                K::EndOfStream,
            ],
            Domain::Rtsp => &[
                K::RtspProtocol,
                K::RtspAuthFailed,
                K::RtspAuthRequired,
                K::RtspNotFound,
                K::RtspUnsupportedTransport,
                K::RtspTls,
                K::RtspIo,
                K::RtspTimeout,
                K::RtspServer,
                K::RtspMount,
                K::Closed,
            ],
            Domain::Udp => &[
                K::UdpUrl,
                K::UdpIo,
                K::TooLarge,
                K::Closed,
                K::UdpInvalidConfig,
                K::Backpressure,
                K::Broken,
                K::InputMalformed,
                K::EndOfStream,
                K::ConfigInvalid,
            ],
            Domain::Tcp => &[
                K::TcpUrl,
                K::TcpIo,
                K::TooLarge,
                K::Closed,
                K::TcpConnectTimeout,
                K::TcpInvalidConfig,
                K::TcpTls,
                K::TcpTlsDisabled,
                K::Backpressure,
                K::Broken,
                K::InputMalformed,
                K::EndOfStream,
                K::ConfigInvalid,
            ],
            Domain::Hls => &[
                K::HlsUrl,
                K::HlsIo,
                K::HlsBindFailed,
                K::HlsInvalidConfig,
                K::HlsUnalignedPushTs,
                K::HlsFinished,
                K::HlsTlsDisabled,
                K::HlsTls,
                K::Internal,
                K::Closed,
            ],
            Domain::Rist => &[
                K::RistUrl,
                K::RistFfi,
                K::TooLarge,
                K::Closed,
                K::RistInvalidConfig,
                K::RistEncryptionDisabled,
                K::RistContextCreateFailed,
                K::RistPeerCreateFailed,
                K::Backpressure,
                K::Broken,
                K::InputMalformed,
                K::EndOfStream,
                K::ConfigInvalid,
            ],
            Domain::Mux => &[
                K::InputMalformed,
                K::ConfigInvalid,
                K::InvalidUsage,
                K::Backpressure,
                K::Internal,
                K::InvalidNal,
                K::KlvTooLarge,
                K::InvalidAv1Obu,
                K::MispTime,
            ],
            Domain::Demux => &[
                K::DemuxUnrecoverable,
                K::DemuxStrictRejection,
                K::DemuxMalformedPes,
                K::DemuxSyncBufExhausted,
            ],
            Domain::KlvDecode => &[
                K::KlvDecodeBadUniversalLabel,
                K::KlvDecodeTruncatedSet,
                K::KlvDecodeChecksumMismatch,
                K::KlvDecodeDuplicateTag,
                K::KlvDecodeMissingRequiredTag,
                K::KlvDecodeMalformedBytes,
                K::Internal,
            ],
            Domain::KlvEncode => &[
                K::KlvEncodeBufferTooSmall,
                K::KlvEncodeRecordTooLarge,
                K::KlvEncodeOutOfRange,
                K::KlvEncodeStringTooLong,
                K::KlvEncodeUnsupportedImapbLength,
                K::KlvEncodeInvalidImapbParams,
                K::KlvEncodeMissingMandatoryItem,
                K::KlvEncodeReservedTagInUnknown,
                K::KlvEncodeVTargetPackEmpty,
                K::KlvEncodeDuplicateTargetId,
                K::KlvEncodeForbiddenStandaloneOffset,
            ],
            Domain::Codec => &[
                K::CodecTruncatedRbsp,
                K::CodecInvalidGolomb,
                K::CodecReservedValue,
                K::CodecUnsupportedProfile,
                K::CodecEngineError,
                K::CodecInvalidLeb128,
                K::CodecBadSyncWord,
                K::CodecTruncated,
                K::CodecForbidden,
                K::CodecUnsupportedFreeFormat,
                K::CodecInvalidLengthSize,
                K::CodecNalLengthOverflow,
                K::CodecBufferTooSmall,
            ],
        }
    }

    /// The Python exception class (`tstrans.exceptions`).
    pub fn python_error_class(self) -> &'static str {
        match self {
            Domain::Srt => "SrtError",
            Domain::Rtp => "RtpError",
            Domain::Rtsp => "RtspError",
            Domain::Udp => "UdpError",
            Domain::Tcp => "TcpError",
            Domain::Hls => "HlsError",
            Domain::Rist => "RistError",
            Domain::Mux => "MuxError",
            Domain::Demux => "DemuxError",
            Domain::KlvDecode => "KlvError",
            Domain::KlvEncode => "KlvEncodeError",
            Domain::Codec => "CodecError",
        }
    }

    /// The Python kind enum (`tstrans.exceptions`).
    pub fn python_kind_enum(self) -> &'static str {
        match self {
            Domain::Srt => "SrtErrorKind",
            Domain::Rtp => "RtpErrorKind",
            Domain::Rtsp => "RtspErrorKind",
            Domain::Udp => "UdpErrorKind",
            Domain::Tcp => "TcpErrorKind",
            Domain::Hls => "HlsErrorKind",
            Domain::Rist => "RistErrorKind",
            Domain::Mux => "MuxErrorKind",
            Domain::Demux => "DemuxErrorKind",
            Domain::KlvDecode => "KlvErrorKind",
            Domain::KlvEncode => "KlvEncodeErrorKind",
            Domain::Codec => "CodecErrorKind",
        }
    }

    /// The JNI class name of the JVM exception, or `None` for a domain the
    /// JVM binding does not expose.
    pub fn java_exception_class(self) -> Option<&'static str> {
        match self {
            Domain::Srt => Some("org/tstrans/SrtException"),
            Domain::Rtp => Some("org/tstrans/RtpException"),
            Domain::Rtsp => Some("org/tstrans/RtspException"),
            Domain::Hls => Some("org/tstrans/HlsException"),
            Domain::Mux => Some("org/tstrans/MuxException"),
            Domain::Demux => Some("org/tstrans/DemuxException"),
            Domain::KlvDecode => Some("org/tstrans/KlvDecodeException"),
            Domain::KlvEncode => Some("org/tstrans/KlvEncodeException"),
            Domain::Codec => Some("org/tstrans/CodecParseException"),
            Domain::Udp => Some("org/tstrans/UdpException"),
            Domain::Tcp => Some("org/tstrans/TcpException"),
            Domain::Rist => Some("org/tstrans/RistException"),
        }
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
    /// Structured detail; see [`Diagnostics`].
    pub diagnostics: Diagnostics,
}

impl BindingError {
    /// A kind with its detail message and no diagnostics.
    pub fn new(kind: BindingErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            diagnostics: Diagnostics::default(),
        }
    }

    /// Replace the diagnostics.
    #[must_use]
    pub fn with_diagnostics(self, diagnostics: Diagnostics) -> Self {
        Self {
            diagnostics,
            ..self
        }
    }
}

/// What a failure carried besides its kind and message, for a binding that
/// wants more than the message: the input-consumption flag a send error
/// carries, the transport's native error number, the clean-EOF cause, and
/// the PID / tag / offset / per-field values of mux, demux, KLV and codec
/// errors. Every field is optional; [`Default`] is "nothing known".
///
/// **Stability: Provisional** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Whether the failed send consumed its input (`MuxSenderError` / `SenderError`).
    pub input_consumed: Option<bool>,
    /// The transport's native error number (`TransportError::{Backpressure, Broken}`).
    pub native_code: Option<i32>,
    /// Why a broken transport broke (`TransportError::Broken`).
    pub broken_cause: Option<BrokenCause>,
    /// The PID of a mux or demux failure that names one.
    pub pid: Option<u16>,
    /// The KLV tag, or the VTarget id, of a KLV decode or encode failure.
    pub tag: Option<u64>,
    /// The byte offset of a KLV decode failure.
    pub offset: Option<u64>,
    /// A codec parse failure's per-variant fields, by their host attribute name.
    pub attrs: Vec<(&'static str, DiagValue)>,
}

/// A per-field value of a codec error (`attrs`): an integer, or a static name
/// (a syntax-element name such as `"profile_idc"`).
///
/// **Stability: Provisional** — see the
/// [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagValue {
    /// An integer field.
    Int(i64),
    /// A static name.
    Str(&'static str),
}

/// A `usize` as a diagnostic integer, saturating.
fn int_usize(x: usize) -> u64 {
    u64::try_from(x).unwrap_or(u64::MAX)
}

/// The PID a mux error names, if its variant has a `pid` field.
fn mux_diagnostics(e: &MuxError) -> Diagnostics {
    let pid = match e {
        MuxError::SubtitlePidUsedAsPcrPid { pid }
        | MuxError::KlvPidUsedAsPcrPid { pid }
        | MuxError::DuplicatePmtPid { pid, .. }
        | MuxError::DuplicatePidAcrossPrograms { pid, .. }
        | MuxError::DataPidUsedAsPcrPid { pid } => Some(*pid),
        _ => None,
    };
    Diagnostics {
        pid,
        ..Diagnostics::default()
    }
}

fn demux_diagnostics(e: &DemuxError) -> Diagnostics {
    let pid = match e {
        DemuxError::MalformedPes { pid, .. } => Some(*pid),
        _ => None,
    };
    Diagnostics {
        pid,
        ..Diagnostics::default()
    }
}

fn klv_decode_diagnostics(e: &KlvDecodeError) -> Diagnostics {
    let (tag, offset) = match e {
        KlvDecodeError::DuplicateTag { tag, offset } => {
            (Some(u64::from(*tag)), Some(int_usize(*offset)))
        }
        KlvDecodeError::St0102MissingRequiredTag { tag }
        | KlvDecodeError::St0903MissingRequiredTag { tag } => (Some(u64::from(*tag)), None),
        KlvDecodeError::Truncated { offset, .. }
        | KlvDecodeError::MalformedLength { offset }
        | KlvDecodeError::MalformedTag { offset }
        | KlvDecodeError::NonCanonicalLength { offset }
        | KlvDecodeError::NonCanonicalTag { offset }
        | KlvDecodeError::St0903InvalidVTargetPack { offset, .. } => {
            (None, Some(int_usize(*offset)))
        }
        _ => (None, None),
    };
    Diagnostics {
        tag,
        offset,
        ..Diagnostics::default()
    }
}

/// The tag a KLV encode failure names: the KLV tag, or the VTarget id.
fn klv_encode_diagnostics(e: &KlvEncodeError) -> Diagnostics {
    let tag = match e {
        KlvEncodeError::OutOfRange { tag, .. }
        | KlvEncodeError::StringTooLong { tag, .. }
        | KlvEncodeError::ReservedTagInUnknown { tag }
        | KlvEncodeError::ForbiddenStandaloneOffset { tag } => Some(u64::from(*tag)),
        KlvEncodeError::MissingMandatoryItem { tag, .. } => Some(u64::from(*tag)),
        KlvEncodeError::VTargetPackEmpty { target_id }
        | KlvEncodeError::DuplicateTargetId { target_id } => Some(*target_id),
        _ => None,
    };
    Diagnostics {
        tag,
        ..Diagnostics::default()
    }
}

/// A codec failure's fields under the names the hosts expose them by.
fn codec_diagnostics(e: &CodecParseError) -> Diagnostics {
    use DiagValue::{Int, Str};
    let attrs: Vec<(&'static str, DiagValue)> = match e {
        CodecParseError::TruncatedRbsp {
            offset_bits,
            needed_bits,
        } => vec![
            ("offset_bits", Int(i64::from(*offset_bits))),
            ("needed_bits", Int(i64::from(*needed_bits))),
        ],
        CodecParseError::InvalidGolomb { offset_bits } => {
            vec![("offset_bits", Int(i64::from(*offset_bits)))]
        }
        CodecParseError::ReservedValue { field, value } => {
            vec![("field", Str(field)), ("value", Int(i64::from(*value)))]
        }
        CodecParseError::UnsupportedProfile { profile_idc } => {
            vec![("profile_idc", Int(i64::from(*profile_idc)))]
        }
        CodecParseError::InvalidLeb128 { offset_bytes } => {
            vec![("offset_bytes", Int(i64::from(*offset_bytes)))]
        }
        CodecParseError::BadSyncWord { expected, found } => vec![
            ("expected", Int(i64::from(*expected))),
            ("found", Int(i64::from(*found))),
        ],
        CodecParseError::Truncated { needed, had } => vec![
            ("needed", Int(i64::from(*needed))),
            ("had", Int(i64::from(*had))),
        ],
        CodecParseError::Forbidden { field } => vec![("field", Str(field))],
        CodecParseError::UnsupportedFreeFormat { layer } => {
            vec![("layer", Int(i64::from(*layer)))]
        }
        CodecParseError::InvalidLengthSize { got } => vec![("got", Int(i64::from(*got)))],
        CodecParseError::NalLengthOverflow {
            nal_len,
            length_size,
        } => vec![
            ("nal_len", Int(i64::from(*nal_len))),
            ("length_size", Int(i64::from(*length_size))),
        ],
        CodecParseError::BufferTooSmall { needed, have } => vec![
            ("needed", Int(i64::try_from(*needed).unwrap_or(i64::MAX))),
            ("have", Int(i64::try_from(*have).unwrap_or(i64::MAX))),
        ],
        _ => Vec::new(),
    };
    Diagnostics {
        attrs,
        ..Diagnostics::default()
    }
}

/// A transport failure's native error number and clean-EOF cause.
fn transport_diagnostics(e: &TransportError) -> Diagnostics {
    match e {
        TransportError::Backpressure { errno_code, .. } => Diagnostics {
            native_code: *errno_code,
            ..Diagnostics::default()
        },
        TransportError::Broken {
            errno_code, cause, ..
        } => Diagnostics {
            native_code: *errno_code,
            broken_cause: Some(*cause),
            ..Diagnostics::default()
        },
        _ => Diagnostics::default(),
    }
}

impl core::fmt::Display for BindingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl core::error::Error for BindingError {}

/// The five `MuxErrorKind` buckets, 1:1, no wildcard: a new bucket is a
/// compile error here until it is given a binding kind.
fn map_mux_kind(k: MuxErrorKind) -> BindingErrorKind {
    match k {
        MuxErrorKind::InputMalformed => BindingErrorKind::InputMalformed,
        MuxErrorKind::ConfigInvalid => BindingErrorKind::ConfigInvalid,
        MuxErrorKind::InvalidUsage => BindingErrorKind::InvalidUsage,
        MuxErrorKind::Backpressure => BindingErrorKind::Backpressure,
        MuxErrorKind::Internal => BindingErrorKind::Internal,
    }
}

/// The four `MuxError` variants C already numbers precisely keep their
/// own kinds; everything else folds to `MuxError::kind()`, which the
/// compiler pins inside tst-core (its match has no wildcard) and
/// `map_mux_kind` maps without one. The fallback arm is total:
/// `MuxError::kind()` classifies every variant in tst-core.
pub fn kind_of_mux(e: &MuxError) -> BindingErrorKind {
    match e {
        MuxError::InvalidNal => BindingErrorKind::InvalidNal,
        MuxError::InvalidAv1Obu => BindingErrorKind::InvalidAv1Obu,
        MuxError::MispTime(_) => BindingErrorKind::MispTime,
        MuxError::KlvTooLarge { .. } => BindingErrorKind::KlvTooLarge,
        _ => map_mux_kind(e.kind()),
    }
}

impl From<MuxError> for BindingError {
    fn from(e: MuxError) -> Self {
        BindingError::new(kind_of_mux(&e), e.to_string()).with_diagnostics(mux_diagnostics(&e))
    }
}

/// Every `DemuxError`, through its exhaustive kind.
fn map_demux(e: &DemuxError) -> BindingErrorKind {
    use tst_core::error::DemuxErrorKind as D;
    match e.kind() {
        D::Unrecoverable => BindingErrorKind::DemuxUnrecoverable,
        D::StrictRejection => BindingErrorKind::DemuxStrictRejection,
        D::MalformedPes => BindingErrorKind::DemuxMalformedPes,
        D::SyncBufExhausted => BindingErrorKind::DemuxSyncBufExhausted,
    }
}

/// The kind of a `DemuxError` (bindings classify by reference without
/// consuming the error).
pub fn kind_of_demux(e: &DemuxError) -> BindingErrorKind {
    map_demux(e)
}

impl From<DemuxError> for BindingError {
    fn from(e: DemuxError) -> Self {
        BindingError::new(map_demux(&e), e.to_string()).with_diagnostics(demux_diagnostics(&e))
    }
}

/// The six KLV-decode buckets, byte-for-byte the routing at
/// `bindings/python/src/klv.rs:97-121` / `bindings/jvm/src/error.rs:138-163`.
fn map_klv_decode(e: &KlvDecodeError) -> BindingErrorKind {
    use tst_core::error::KlvDecodeErrorKind as D;
    match e.kind() {
        D::Truncated => BindingErrorKind::KlvDecodeTruncatedSet,
        D::BadUniversalLabel => BindingErrorKind::KlvDecodeBadUniversalLabel,
        D::ChecksumMismatch => BindingErrorKind::KlvDecodeChecksumMismatch,
        D::DuplicateTag => BindingErrorKind::KlvDecodeDuplicateTag,
        D::MissingRequiredTag => BindingErrorKind::KlvDecodeMissingRequiredTag,
        D::Malformed => BindingErrorKind::KlvDecodeMalformedBytes,
    }
}

/// The kind of a `KlvDecodeError` (bindings classify by reference without
/// consuming the error).
pub fn kind_of_klv_decode(e: &KlvDecodeError) -> BindingErrorKind {
    map_klv_decode(e)
}

impl From<KlvDecodeError> for BindingError {
    fn from(e: KlvDecodeError) -> Self {
        BindingError::new(map_klv_decode(&e), e.to_string())
            .with_diagnostics(klv_decode_diagnostics(&e))
    }
}

/// `KlvFieldError` on its own (the SRT umbrella carries one; the Python
/// klv module raises it directly): `TruncatedField` is a truncated set,
/// every other field failure is malformed bytes
/// (`bindings/python/src/klv.rs:140-141`, table rows 67 + 72).
fn map_klv_field(e: &KlvFieldError) -> BindingErrorKind {
    use tst_core::error::KlvFieldErrorKind as F;
    match e.kind() {
        F::Truncated => BindingErrorKind::KlvDecodeTruncatedSet,
        F::Malformed => BindingErrorKind::KlvDecodeMalformedBytes,
    }
}

/// The kind of a `KlvFieldError` (bindings classify by reference without
/// consuming the error).
pub fn kind_of_klv_field(e: &KlvFieldError) -> BindingErrorKind {
    map_klv_field(e)
}

impl From<KlvFieldError> for BindingError {
    fn from(e: KlvFieldError) -> Self {
        BindingError::new(map_klv_field(&e), e.to_string())
    }
}

/// Every `KlvEncodeError`, 1:1 through its exhaustive kind.
fn map_klv_encode(e: &KlvEncodeError) -> BindingErrorKind {
    use tst_core::error::KlvEncodeErrorKind as E;
    match e.kind() {
        E::BufferTooSmall => BindingErrorKind::KlvEncodeBufferTooSmall,
        E::RecordTooLarge => BindingErrorKind::KlvEncodeRecordTooLarge,
        E::OutOfRange => BindingErrorKind::KlvEncodeOutOfRange,
        E::StringTooLong => BindingErrorKind::KlvEncodeStringTooLong,
        E::UnsupportedImapbLength => BindingErrorKind::KlvEncodeUnsupportedImapbLength,
        E::InvalidImapbParams => BindingErrorKind::KlvEncodeInvalidImapbParams,
        E::MissingMandatoryItem => BindingErrorKind::KlvEncodeMissingMandatoryItem,
        E::ReservedTagInUnknown => BindingErrorKind::KlvEncodeReservedTagInUnknown,
        E::VTargetPackEmpty => BindingErrorKind::KlvEncodeVTargetPackEmpty,
        E::DuplicateTargetId => BindingErrorKind::KlvEncodeDuplicateTargetId,
        E::ForbiddenStandaloneOffset => BindingErrorKind::KlvEncodeForbiddenStandaloneOffset,
    }
}

/// The kind of a `KlvEncodeError` (bindings classify by reference without
/// consuming the error).
pub fn kind_of_klv_encode(e: &KlvEncodeError) -> BindingErrorKind {
    map_klv_encode(e)
}

impl From<KlvEncodeError> for BindingError {
    fn from(e: KlvEncodeError) -> Self {
        BindingError::new(map_klv_encode(&e), e.to_string())
            .with_diagnostics(klv_encode_diagnostics(&e))
    }
}

/// Every `CodecParseError`, 1:1 through its exhaustive kind.
fn map_codec(e: &CodecParseError) -> BindingErrorKind {
    use tst_core::codec::CodecParseErrorKind as C;
    match e.kind() {
        C::TruncatedRbsp => BindingErrorKind::CodecTruncatedRbsp,
        C::InvalidGolomb => BindingErrorKind::CodecInvalidGolomb,
        C::ReservedValue => BindingErrorKind::CodecReservedValue,
        C::UnsupportedProfile => BindingErrorKind::CodecUnsupportedProfile,
        C::EngineError => BindingErrorKind::CodecEngineError,
        C::InvalidLeb128 => BindingErrorKind::CodecInvalidLeb128,
        C::BadSyncWord => BindingErrorKind::CodecBadSyncWord,
        C::Truncated => BindingErrorKind::CodecTruncated,
        C::Forbidden => BindingErrorKind::CodecForbidden,
        C::UnsupportedFreeFormat => BindingErrorKind::CodecUnsupportedFreeFormat,
        C::InvalidLengthSize => BindingErrorKind::CodecInvalidLengthSize,
        C::NalLengthOverflow => BindingErrorKind::CodecNalLengthOverflow,
        C::BufferTooSmall => BindingErrorKind::CodecBufferTooSmall,
    }
}

/// The kind of a `CodecParseError` (bindings classify by reference without
/// consuming the error).
pub fn kind_of_codec(e: &CodecParseError) -> BindingErrorKind {
    map_codec(e)
}

impl From<CodecParseError> for BindingError {
    fn from(e: CodecParseError) -> Self {
        BindingError::new(map_codec(&e), e.to_string()).with_diagnostics(codec_diagnostics(&e))
    }
}

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

/// [`Owned::close`](crate::binding::Owned::close)'s failure, so every binding
/// surfaces a failed close the same way instead of hand-rolling it.
///
/// The bound is `E: Display`, not `E: Into<BindingError>`, because that is
/// what makes the impl usable for the shells: `Close::Error` is bounded
/// `Debug + Display`, and all nine in-tree `Close` impls set it to
/// `Infallible`, which has no `Into<BindingError>`. `Inner` therefore carries
/// the shell's own message under [`BindingErrorKind::Internal`] — no table row
/// describes "a shell's close failed", and none can while `Inner` is
/// unconstructible through the shells. A caller whose `E` does have a kind of
/// its own converts it first: `match cf { CloseFailure::Inner(e) => e.into(),
/// other => other.into() }`.
impl<E: core::fmt::Display> From<CloseFailure<E>> for BindingError {
    fn from(e: CloseFailure<E>) -> Self {
        match e {
            CloseFailure::Inner(inner) => {
                BindingError::new(BindingErrorKind::Internal, inner.to_string())
            }
            CloseFailure::Panicked { detail } => {
                BindingError::new(BindingErrorKind::PanicCaught, detail)
            }
        }
    }
}

/// `ShellErrorKind` is a projection of the table. Exhaustive:
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

/// The kind of a transport failure and the detail the binding surfaces.
/// The kind comes from the exhaustive `TransportError::kind()`; the detail
/// is a second match whose fallback is not a classification.
fn map_transport(e: &TransportError) -> (BindingErrorKind, String) {
    use tst_core::transport::TransportErrorKind as T;
    let kind = match e.kind() {
        T::Backpressure => BindingErrorKind::Backpressure,
        T::Broken => BindingErrorKind::Broken,
        T::Closed | T::ExplicitClose => BindingErrorKind::Closed,
        T::TooLarge => BindingErrorKind::TooLarge,
    };
    let detail = match e {
        TransportError::Backpressure { msg, .. } | TransportError::Broken { msg, .. } => {
            msg.clone()
        }
        TransportError::Closed => String::from("transport closed"),
        TransportError::ExplicitClose => String::from("cancelled from another thread"),
        _ => e.to_string(),
    };
    (kind, detail)
}

impl From<TransportError> for BindingError {
    fn from(e: TransportError) -> Self {
        let (kind, detail) = map_transport(&e);
        BindingError::new(kind, detail).with_diagnostics(transport_diagnostics(&e))
    }
}

/// The binding kind of a transport failure seen by a shell: on the
/// receive direction `TransportError::Closed` is the PEER closing
/// (`EndOfStream`); a caller's cancel arrives as `ExplicitClose` (`Closed`).
pub(crate) fn kind_of_transport(
    e: &TransportError,
    d: crate::shell_error::Direction,
) -> BindingErrorKind {
    use tst_core::transport::TransportErrorKind as T;
    match (e.kind(), d) {
        (T::Closed, crate::shell_error::Direction::Recv) => BindingErrorKind::EndOfStream,
        _ => map_transport(e).0,
    }
}

/// The coarse shell vocabulary of a binding kind: `ShellErrorKind` is a
/// projection of this table. Exhaustive, so a new kind is a compile error
/// until it is coarsened.
pub(crate) fn coarsen(k: BindingErrorKind) -> ShellErrorKind {
    use BindingErrorKind as K;
    use ShellErrorKind as S;
    match k {
        K::ConfigInvalid | K::InvalidUsage => S::ConfigInvalid,
        K::InputMalformed
        | K::InvalidNal
        | K::KlvTooLarge
        | K::InvalidAv1Obu
        | K::MispTime
        | K::MispTimeMalformed
        | K::TooLarge
        | K::WrongType
        | K::DemuxUnrecoverable
        | K::DemuxStrictRejection
        | K::DemuxMalformedPes
        | K::DemuxSyncBufExhausted
        | K::KlvDecodeTruncatedSet
        | K::KlvDecodeBadUniversalLabel
        | K::KlvDecodeChecksumMismatch
        | K::KlvDecodeDuplicateTag
        | K::KlvDecodeMissingRequiredTag
        | K::KlvDecodeMalformedBytes
        | K::KlvEncodeBufferTooSmall
        | K::KlvEncodeRecordTooLarge
        | K::KlvEncodeOutOfRange
        | K::KlvEncodeStringTooLong
        | K::KlvEncodeUnsupportedImapbLength
        | K::KlvEncodeInvalidImapbParams
        | K::KlvEncodeMissingMandatoryItem
        | K::KlvEncodeReservedTagInUnknown
        | K::KlvEncodeVTargetPackEmpty
        | K::KlvEncodeDuplicateTargetId
        | K::KlvEncodeForbiddenStandaloneOffset
        | K::CodecTruncatedRbsp
        | K::CodecInvalidGolomb
        | K::CodecReservedValue
        | K::CodecUnsupportedProfile
        | K::CodecEngineError
        | K::CodecInvalidLeb128
        | K::CodecBadSyncWord
        | K::CodecTruncated
        | K::CodecForbidden
        | K::CodecUnsupportedFreeFormat
        | K::CodecInvalidLengthSize
        | K::CodecNalLengthOverflow
        | K::CodecBufferTooSmall => S::InputMalformed,
        K::Backpressure => S::Backpressure,
        K::Closed => S::Closed,
        K::EndOfStream => S::EndOfStream,
        K::Broken
        | K::Internal
        | K::PanicCaught
        | K::NotAvailable
        | K::NotFound
        | K::RtspProtocol
        | K::RtspAuthFailed
        | K::RtspAuthRequired
        | K::RtspNotFound
        | K::RtspUnsupportedTransport
        | K::RtspTls
        | K::RtspIo
        | K::RtspTimeout
        | K::RtspServer
        | K::RtspMount
        | K::UdpIo
        | K::UdpInvalidConfig
        | K::UdpUrl
        | K::TcpIo
        | K::TcpInvalidConfig
        | K::TcpConnectTimeout
        | K::TcpTls
        | K::TcpUrl
        | K::TcpTlsDisabled
        | K::HlsIo
        | K::HlsInvalidConfig
        | K::HlsFinished
        | K::HlsTls
        | K::HlsUrl
        | K::HlsBindFailed
        | K::HlsUnalignedPushTs
        | K::HlsTlsDisabled
        | K::RistFfi
        | K::RistInvalidConfig
        | K::RistEncryptionDisabled
        | K::RistUrl
        | K::RistContextCreateFailed
        | K::RistPeerCreateFailed
        | K::SrtConnectFailed
        | K::SrtAcceptFailed
        | K::SrtTimeout
        | K::SrtIo
        | K::RtpPayloadTypeParam
        | K::RtpMissingPayloadTypeParam
        | K::RtpUrl
        | K::RtpHostNotLiteral
        | K::RtpIo
        | K::RtpIfaceUnsupported => S::TransportBroken,
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

// Shell-error structs project by `source` — the same split Python and the
// JVM apply — so `MuxSender::send_video(bad NAL)` is INVALID_NAL, not the
// coarser INPUT_MALFORMED.
// The source enums are in-crate, so these matches are exhaustive.

impl From<MuxSenderError> for BindingError {
    fn from(e: MuxSenderError) -> Self {
        let input_consumed = e.input_consumed;
        let mut b: BindingError = match e.source {
            MuxSenderErrorSource::Mux(m) => m.into(),
            MuxSenderErrorSource::Transport(t) => t.into(),
        };
        b.diagnostics.input_consumed = input_consumed;
        b
    }
}

impl From<SenderError> for BindingError {
    fn from(e: SenderError) -> Self {
        let input_consumed = e.input_consumed;
        let mut b: BindingError = match e.source {
            SenderErrorSource::Framing(f) => f.into(),
            SenderErrorSource::Transport(t) => t.into(),
        };
        b.diagnostics.input_consumed = input_consumed;
        b
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
/// `HlsError` in practice — its `From<HlsError>` lives in tst-hls, so this
/// impl needs only `E: Into<BindingError>`). Projected by `source`: a muxer
/// rejection is the mux kind (not `HlsErrorKind.INVALID_CONFIG`), the
/// sink's error is its own kind, `Closed` (shell consumed via `finish`) is
/// `Closed` (not `FINISHED`), `LockPoisoned` is `Internal`. In-crate enum → exhaustive.
#[cfg(feature = "std")]
impl<E> From<MuxPublisherError<E>> for BindingError
where
    E: Into<BindingError> + core::error::Error + Send + Sync + 'static,
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

    #[test]
    fn every_domain_lists_each_kind_once_and_every_table_kind_belongs_somewhere_or_is_hostless() {
        use std::collections::HashSet;
        for d in Domain::ALL {
            let mut seen = HashSet::new();
            for k in d.kinds() {
                assert!(seen.insert(*k), "{d:?} lists {k:?} twice");
            }
            // A member name must be unique inside one host enum.
            let names: HashSet<_> = d.kinds().iter().map(|k| k.name()).collect();
            assert_eq!(
                names.len(),
                d.kinds().len(),
                "{d:?} has two kinds with one member name"
            );
        }
        let hostless = [
            BindingErrorKind::PanicCaught,
            BindingErrorKind::NotAvailable,
            BindingErrorKind::NotFound,
            BindingErrorKind::MispTimeMalformed,
            BindingErrorKind::WrongType,
        ];
        for k in BindingErrorKind::ALL {
            let in_some = Domain::ALL.iter().any(|d| d.kinds().contains(k));
            assert!(
                in_some || hostless.contains(k),
                "{k:?} is in no domain and not a C-only kind"
            );
        }
    }

    #[test]
    fn every_kind_has_a_doc_and_names_are_host_identifiers() {
        for k in BindingErrorKind::ALL {
            assert!(!k.doc().trim().is_empty(), "{k:?} has no doc text");
            assert!(
                k.name()
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            );
        }
        assert_eq!(
            Domain::Udp.java_exception_class(),
            Some("org/tstrans/UdpException")
        );
        assert_eq!(
            Domain::Tcp.java_exception_class(),
            Some("org/tstrans/TcpException")
        );
        assert_eq!(
            Domain::Rist.java_exception_class(),
            Some("org/tstrans/RistException")
        );
        assert_eq!(
            Domain::Codec.java_exception_class(),
            Some("org/tstrans/CodecParseException")
        );
        assert_eq!(Domain::KlvDecode.python_kind_enum(), "KlvErrorKind");
    }

    #[test]
    fn diagnostics_carry_what_the_source_error_knew() {
        use tst_core::codec::CodecParseError;
        let b = BindingError::from(CodecParseError::TruncatedRbsp {
            offset_bits: 12,
            needed_bits: 4,
        });
        assert_eq!(b.kind, BindingErrorKind::CodecTruncatedRbsp);
        assert_eq!(
            b.diagnostics.attrs,
            vec![
                ("offset_bits", DiagValue::Int(12)),
                ("needed_bits", DiagValue::Int(4))
            ]
        );
        let t = BindingError::from(TransportError::Broken {
            msg: "reset".into(),
            errno_code: Some(104),
            cause: BrokenCause::CleanEof,
        });
        assert_eq!(t.diagnostics.native_code, Some(104));
        assert_eq!(t.diagnostics.broken_cause, Some(BrokenCause::CleanEof));
        let k = BindingError::from(KlvDecodeError::DuplicateTag { tag: 5, offset: 30 });
        assert_eq!(
            (k.diagnostics.tag, k.diagnostics.offset),
            (Some(5), Some(30))
        );
    }

    #[test]
    fn send_errors_carry_input_consumed_and_the_source_detail() {
        let e = MuxSenderError::from(broken()).with_input_consumed(true);
        let b = BindingError::from(e);
        assert_eq!(b.diagnostics.input_consumed, Some(true));
        assert_eq!(b.diagnostics.native_code, Some(2));
        let e = SenderError::from(broken());
        let b = BindingError::from(e);
        assert_eq!(b.diagnostics.input_consumed, None);
        assert_eq!(b.diagnostics.native_code, Some(2));
    }

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

    /// `Owned::close`'s failure mode, for the bindings that surface it.
    /// Every in-tree `Close` impl has `Error = Infallible`, so `Inner` is
    /// unconstructible through the shells — the bound is `E: Display`
    /// (`Close::Error` always is), which is what makes the impl callable for
    /// them at all.
    #[test]
    fn close_failure_maps_panic_and_inner() {
        use crate::binding::owned::CloseFailure;
        let p: BindingError = CloseFailure::<std::convert::Infallible>::Panicked {
            detail: "close panicked".into(),
        }
        .into();
        assert_eq!(
            (p.kind, p.detail.as_str()),
            (BindingErrorKind::PanicCaught, "close panicked")
        );
        let i: BindingError = CloseFailure::Inner(String::from("final flush failed")).into();
        assert_eq!(
            (i.kind, i.detail.as_str()),
            (BindingErrorKind::Internal, "final flush failed")
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
    #[cfg(feature = "std")]
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
        // C codes that are deliberately NOT discriminants, in two groups.
        //
        // Reserved and never produced — no `TstError` producer exists:
        //   -28 UdpPayloadTooLarge, -29 UdpIfaceUnsupported,
        //   -40 RistPayloadTooLarge, -42 RistRecvTimeout, -43 RistIo.
        //
        // Real, produced codes that this table represents as finer kinds
        // instead of one variant — they survive only as `c_projection()` fold
        // targets, which is what keeps the C ABI unchanged:
        //   -15 RtpTransport  <- the six RTP_* kinds,
        //   -48 KlvDecode     <- the six KLV_DECODE_* kinds.
        for not_a_discriminant in [-15, -28, -29, -40, -42, -43, -48] {
            assert!(
                BindingErrorKind::ALL
                    .iter()
                    .all(|k| k.c_code() != not_a_discriminant),
                "{not_a_discriminant} must not be a discriminant"
            );
        }
        // …and the two fold targets really are reachable through projection.
        for folded in [-15, -48] {
            assert!(
                BindingErrorKind::ALL
                    .iter()
                    .any(|k| k.c_projection() == folded),
                "{folded} must still be emitted by C via c_projection()"
            );
        }
    }

    #[test]
    fn new_kinds_are_numbered_from_minus_49_in_table_order_and_project_onto_frozen_codes() {
        let new: Vec<_> = BindingErrorKind::ALL
            .iter()
            .filter(|k| !k.is_c_frozen())
            .collect();
        assert_eq!(new.len(), 54);
        // Codes of removed kinds are never reused: -71 (DemuxMalformedPsi),
        // -95 and -96 (CodecDangling{Sps,Vps}Reference) stay as gaps.
        const RETIRED: [i32; 3] = [-71, -95, -96];
        let mut expected = -49;
        for k in &new {
            while RETIRED.contains(&expected) {
                expected -= 1;
            }
            assert_eq!(k.c_code(), expected, "{} out of order", k.name());
            expected -= 1;
            assert!(
                (-48..=-1).contains(&k.c_projection()),
                "{} projects outside TST_E",
                k.name()
            );
        }
        assert_eq!(BindingErrorKind::ALL.len(), 95);
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

    /// `KlvFieldError` has its own two-bucket rule (table rows 67 + 72):
    /// a truncated field reads as a truncated set, every other field
    /// failure as malformed bytes.
    #[test]
    fn klv_field_splits_truncation_from_everything_else() {
        use BindingErrorKind as K;
        use tst_core::error::KlvFieldError as F;
        assert_eq!(
            kind_of_klv_field(&F::TruncatedField { tag: 13 }),
            K::KlvDecodeTruncatedSet
        );
        assert_eq!(
            kind_of_klv_field(&F::InvalidUtf8 { tag: 2 }),
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

    /// Every shell error's kind is the coarsening of the binding kind its
    /// source projects to: the shells and the bindings classify through one
    /// table. Covers every `MuxError`, `TransportError` (both directions)
    /// and `DemuxError` variant.
    #[test]
    fn shell_error_kind_is_the_coarsening_of_the_binding_kind() {
        use crate::shell_error::ShellError;
        use crate::{DemuxReceiverError, MuxSenderError, ReceiverError};
        use tst_test_helpers::errors::{every_demux_error, every_mux_error, every_transport_error};

        let muxes = every_mux_error();
        assert_eq!(muxes.len(), 38);
        for m in muxes {
            let e = MuxSenderError::from(m.clone());
            let shell = e.kind();
            assert_eq!(shell, coarsen(BindingError::from(e).kind), "{m:?}");
        }
        for t in every_transport_error() {
            let e = MuxSenderError::from(t.clone());
            let shell = e.kind();
            assert_eq!(shell, coarsen(BindingError::from(e).kind), "send {t:?}");
            let e = ReceiverError::from(t.clone());
            let shell = e.kind();
            assert_eq!(shell, coarsen(BindingError::from(e).kind), "recv {t:?}");
        }
        for d in every_demux_error() {
            let e = DemuxReceiverError::from(d.clone());
            let shell = e.kind();
            assert_eq!(shell, coarsen(BindingError::from(e).kind), "{d:?}");
        }
    }

    #[test]
    fn coarsen_round_trips_shell_kinds() {
        for s in [
            ShellErrorKind::ConfigInvalid,
            ShellErrorKind::InputMalformed,
            ShellErrorKind::Backpressure,
            ShellErrorKind::TransportBroken,
            ShellErrorKind::Closed,
            ShellErrorKind::EndOfStream,
        ] {
            assert_eq!(coarsen(BindingErrorKind::from(s)), s);
        }
    }

    /// `coarsen` is total by construction (no wildcard arm); this pins that
    /// the table reaches every shell kind.
    #[test]
    fn coarsen_is_total() {
        use std::collections::HashSet;
        let reached: HashSet<_> = BindingErrorKind::ALL.iter().map(|k| coarsen(*k)).collect();
        assert_eq!(reached.len(), 6, "every ShellErrorKind has a source kind");
    }

    #[test]
    fn kind_docs_render_safely_into_javadoc_and_python_comments() {
        for k in BindingErrorKind::ALL {
            for line in k.doc().lines() {
                let l = line.trim();
                assert!(!l.contains("*/"), "{k:?}: `*/` ends a Javadoc comment");
                assert!(!l.contains('@'), "{k:?}: `@` starts a Javadoc tag");
                assert!(
                    !l.contains('<') && !l.contains('&'),
                    "{k:?}: HTML in Javadoc"
                );
                assert!(
                    !l.starts_with('#'),
                    "{k:?}: a leading `#` reads as a Markdown heading in the generated docs"
                );
            }
        }
    }
}
