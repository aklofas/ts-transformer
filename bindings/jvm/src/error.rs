//! JNI exception construction. One `throw_<family>` per Rust error family,
//! mirroring tst-py's `make_<family>_error` helpers. Each constructs the
//! `org.tstrans.<Family>Exception` object with its `Kind` enum value and throws
//! it. Call these, then return a Rust default from the JNI fn — the pending
//! Java exception is raised when control returns to the JVM.

use jni::JNIEnv;
use jni::objects::{JObject, JThrowable, JValue};
use tst_core::codec::CodecParseError;
use tst_core::error::{KlvDecodeError, KlvEncodeError};
use tst_pipeline::binding::{BindingError, BindingErrorKind, HandleState};

/// Variant-specific diagnostic fields forwarded to `CodecParseException`.
/// Every field is `None` except those the producing `CodecParseError` variant
/// carries — mirrors the per-variant kwarg set in tst-py's
/// `codec_parse_error_to_pyerr`.
#[derive(Default)]
pub struct CodecErrFields {
    pub offset_bits: Option<i32>,
    pub needed_bits: Option<i32>,
    pub field: Option<String>,
    pub value: Option<i32>,
    pub profile_idc: Option<i32>,
    pub sps_id: Option<i32>,
    pub vps_id: Option<i32>,
    pub offset_bytes: Option<i32>,
    pub expected: Option<i32>,
    pub found: Option<i32>,
    pub needed: Option<i32>,
    pub had: Option<i32>,
    pub layer: Option<i32>,
}

/// Throw `java.lang.IllegalStateException("{what} is closed")`.
///
/// Shared helper for every JNI site that guards a closed/consumed handle.
/// The exception class and message text are identical to what the inline sites
/// used to emit; Java tests that assert on the message will see no difference.
pub fn throw_closed(env: &mut JNIEnv, what: &str) {
    let _ = env.throw_new(
        "java/lang/IllegalStateException",
        format!("{what} is closed"),
    );
}

/// Which Java exception class a [`BindingError`] is thrown as. The kind
/// alone cannot decide it — `Closed` exists in both `SrtException.Kind`
/// and `RtpException.Kind`, `Internal` in four enums — so the throw SITE
/// names its domain, and the domain's declared subset is what
/// [`verify_kind_tables`] resolves at load. The Java constant is
/// [`BindingErrorKind::name`] (domain prefix stripped);
/// [`BindingErrorKind::variant_name`] is the prefixed identity and appears
/// only in messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    Srt,
    Rtp,
    Rtsp,
    Demux,
    Mux,
    KlvDecode,
    KlvEncode,
    Codec,
}

use BindingErrorKind as K;

/// `org.tstrans.SrtException.Kind` (spec §3.3 + A2's SRT buckets).
pub(crate) const SRT_KINDS: &[K] = &[
    K::ConfigInvalid,
    K::SrtConnectFailed,
    K::SrtAcceptFailed,
    K::SrtTimeout,
    K::Closed,
    K::Broken,
    K::SrtIo,
    K::Backpressure,
    K::TooLarge,
    K::InputMalformed,
];
/// `org.tstrans.RtpException.Kind`: the five `TransportError` projections
/// that reach an rtp shell + tst-rtp's six `ConnectError` kinds.
pub(crate) const RTP_KINDS: &[K] = &[
    K::Backpressure,
    K::Broken,
    K::Closed,
    K::TooLarge,
    K::RtpPayloadTypeParam,
    K::RtpMissingPayloadTypeParam,
    K::RtpUrl,
    K::RtpHostNotLiteral,
    K::RtpIo,
    K::RtpIfaceUnsupported,
];
/// `org.tstrans.RtspException.Kind` — ten names unchanged since v0.1.0.
pub(crate) const RTSP_KINDS: &[K] = &[
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
];
/// `org.tstrans.DemuxException.Kind` (`Internal` = the JNI event-conversion
/// failure and A2's wildcard, not a `DemuxError` variant).
pub(crate) const DEMUX_KINDS: &[K] = &[
    K::DemuxUnrecoverable,
    K::DemuxMalformedPsi,
    K::DemuxMalformedPes,
    K::DemuxSyncBufExhausted,
    K::DemuxStrictRejection,
    K::Internal,
];
/// `org.tstrans.MuxException.Kind`.
pub(crate) const MUX_KINDS: &[K] = &[
    K::InputMalformed,
    K::ConfigInvalid,
    K::InvalidUsage,
    K::Backpressure,
    K::Internal,
    K::InvalidNal,
    K::KlvTooLarge,
    K::InvalidAv1Obu,
    K::MispTime,
];
/// `org.tstrans.KlvDecodeException.Kind`.
pub(crate) const KLV_DECODE_KINDS: &[K] = &[
    K::KlvDecodeTruncatedSet,
    K::KlvDecodeBadUniversalLabel,
    K::KlvDecodeChecksumMismatch,
    K::KlvDecodeDuplicateTag,
    K::KlvDecodeMissingRequiredTag,
    K::KlvDecodeMalformedBytes,
    K::Internal,
];
/// `org.tstrans.KlvEncodeException.Kind`.
pub(crate) const KLV_ENCODE_KINDS: &[K] = &[
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
];
/// `org.tstrans.CodecParseException.Kind`.
pub(crate) const CODEC_KINDS: &[K] = &[
    K::CodecTruncatedRbsp,
    K::CodecInvalidGolomb,
    K::CodecReservedValue,
    K::CodecUnsupportedProfile,
    K::CodecDanglingSpsReference,
    K::CodecDanglingVpsReference,
    K::CodecEngineError,
    K::CodecInvalidLeb128,
    K::CodecBadSyncWord,
    K::CodecTruncated,
    K::CodecForbidden,
    K::CodecUnsupportedFreeFormat,
    K::CodecInvalidLengthSize,
    K::CodecNalLengthOverflow,
    K::CodecBufferTooSmall,
];

impl Domain {
    pub(crate) const ALL: [Domain; 8] = [
        Domain::Srt,
        Domain::Rtp,
        Domain::Rtsp,
        Domain::Demux,
        Domain::Mux,
        Domain::KlvDecode,
        Domain::KlvEncode,
        Domain::Codec,
    ];

    /// JNI class name of the domain's exception.
    pub(crate) const fn exc_class(self) -> &'static str {
        match self {
            Domain::Srt => "org/tstrans/SrtException",
            Domain::Rtp => "org/tstrans/RtpException",
            Domain::Rtsp => "org/tstrans/RtspException",
            Domain::Demux => "org/tstrans/DemuxException",
            Domain::Mux => "org/tstrans/MuxException",
            Domain::KlvDecode => "org/tstrans/KlvDecodeException",
            Domain::KlvEncode => "org/tstrans/KlvEncodeException",
            Domain::Codec => "org/tstrans/CodecParseException",
        }
    }

    /// The kinds this domain's Java `Kind` enum declares. Every entry's
    /// [`BindingErrorKind::name`] must resolve as a static field of
    /// `<exc_class>$Kind` — [`verify_kind_tables`] checks that at load.
    pub(crate) const fn kinds(self) -> &'static [K] {
        match self {
            Domain::Srt => SRT_KINDS,
            Domain::Rtp => RTP_KINDS,
            Domain::Rtsp => RTSP_KINDS,
            Domain::Demux => DEMUX_KINDS,
            Domain::Mux => MUX_KINDS,
            Domain::KlvDecode => KLV_DECODE_KINDS,
            Domain::KlvEncode => KLV_ENCODE_KINDS,
            Domain::Codec => CODEC_KINDS,
        }
    }
}

/// THE raise path for every `(Kind, String)`-constructed exception —
/// srt / rtp / rtsp / demux / mux / klv-decode:
/// `<Domain>Exception(Kind.<name()>, detail)`. (`KlvEncodeException` and
/// `CodecParseException` have wider constructors; their throwers in this
/// file call [`declared_member`] for the same checks.)
///
/// Bails if an exception is already pending. A kind the domain does not
/// declare is a programming error (a producer nobody listed) and throws a
/// `RuntimeException` naming it — loud, never a wrong-kind exception.
#[expect(dead_code, reason = "throw sites move over in B3.3-B3.5b")]
pub(crate) fn throw_binding(env: &mut JNIEnv, domain: Domain, e: &BindingError) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    let exc_class = domain.exc_class();
    let Some(member) = declared_member(env, domain, e.kind, &e.detail) else {
        return;
    };
    let kind_sig = format!("L{exc_class}$Kind;");
    if let Err(err) = throw_kinded(env, exc_class, &kind_sig, member, &e.detail) {
        let simple_name = exc_class.rsplit('/').next().unwrap_or(exc_class);
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!(
                "{simple_name} throw failed ({}): {err}",
                e.kind.variant_name()
            ),
        );
    }
}

/// The Java constant for `kind` in `domain`, or `None` after throwing a
/// `RuntimeException` naming the undeclared kind (a producer nobody
/// listed — loud, never a wrong-kind exception).
pub(crate) fn declared_member(
    env: &mut JNIEnv,
    domain: Domain,
    kind: BindingErrorKind,
    detail: &str,
) -> Option<&'static str> {
    if domain.kinds().contains(&kind) {
        return Some(kind.name());
    }
    let _ = env.throw_new(
        "java/lang/RuntimeException",
        format!(
            "tst-jni: kind {} is not declared for {}$Kind (add it to the domain's KINDS and the Java enum); detail: {detail}",
            kind.variant_name(),
            domain.exc_class()
        ),
    );
    None
}

/// The ONE [`HandleState`] → Java mapping (spec §5, JVM column):
/// `Closed` → `IllegalStateException("<what> is closed")` (the Java-side
/// `NativeHandle.ensureOpen` guard throws the same type BEFORE the native
/// runs, so a native-side `Closed` — only reachable in a close race — must
/// not differ); `Poisoned` → `IllegalStateException`; `Panicked` → the
/// `RuntimeException("native panic in tst-jni: …")` [`crate::panic::jni_catch`]
/// already produces for an uncaught panic, so a panic reads the same whether
/// `Owned::with_mut` or the outer boundary caught it.
#[expect(dead_code, reason = "call sites move over in B3.3-B3.5b")]
pub(crate) fn throw_handle_state(env: &mut JNIEnv, what: &str, state: &HandleState) {
    if env.exception_check().unwrap_or(false) {
        return;
    }
    match state {
        HandleState::Closed => throw_closed(env, what),
        HandleState::Poisoned => {
            let _ = env.throw_new(
                "java/lang/IllegalStateException",
                format!(
                    "{what} is poisoned: a previous native call panicked while holding its lock"
                ),
            );
        }
        HandleState::Panicked { detail } => {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                format!("native panic in tst-jni: {detail}"),
            );
        }
        // `HandleState` is #[non_exhaustive]: a future state is a closed
        // handle as far as the Java caller can act on it.
        other => {
            let _ = env.throw_new(
                "java/lang/IllegalStateException",
                format!("{what} is unusable: {other:?}"),
            );
        }
    }
}

/// Load-time check (called once from `NativeLoader.load()` right after
/// `System.load`): resolve every declared kind name of every domain
/// against its Java enum. A missing member fails HERE, with both sides
/// named, instead of at the first throw as a `RuntimeException("…throw
/// failed (X)")`. `GetStaticFieldID` leaves a `NoSuchFieldError` pending
/// on a miss; it is cleared and replaced so the message carries the
/// Rust variant too.
pub(crate) fn verify_kind_tables(env: &mut JNIEnv) {
    for domain in Domain::ALL {
        let kind_class = format!("{}$Kind", domain.exc_class());
        let kind_sig = format!("L{kind_class};");
        for kind in domain.kinds() {
            let resolved = env.get_static_field(&kind_class, kind.name(), &kind_sig);
            if resolved.is_err() || env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
                let _ = env.throw_new(
                    "java/lang/IllegalStateException",
                    format!(
                        "tst-jni kind table mismatch: {kind_class} has no member {} (BindingErrorKind::{kind:?}); the JAR and libtstjni were built from different sources",
                        kind.name()
                    ),
                );
                return;
            }
        }
    }
}

/// Construct + throw `org.tstrans.DemuxException(Kind.<kind>, message)`.
/// `kind` MUST be one of the `DemuxException.Kind` enum constant names
/// (SCREAMING_SNAKE_CASE), matching the Rust `DemuxError` variants 1:1.
pub fn throw_demux(env: &mut JNIEnv, kind: &str, message: &str) {
    throw_family(
        env,
        "org/tstrans/DemuxException",
        "Lorg/tstrans/DemuxException$Kind;",
        kind,
        message,
    );
}

/// Construct + throw `org.tstrans.MuxException(Kind.<kind>, message)`.
/// `kind` MUST be one of the `MuxException.Kind` enum constant names
/// (SCREAMING_SNAKE_CASE), matching the 5-variant `MuxErrorKind` buckets.
pub fn throw_mux(env: &mut JNIEnv, kind: &str, message: &str) {
    throw_family(
        env,
        "org/tstrans/MuxException",
        "Lorg/tstrans/MuxException$Kind;",
        kind,
        message,
    );
}

/// Construct + throw `org.tstrans.KlvDecodeException(Kind.<kind>, message)`.
/// `kind` MUST be one of the `KlvDecodeException.Kind` constant names
/// (SCREAMING_SNAKE_CASE). The ratchet greps for `throw_klv_decode(env, "<CONST>", ...)`.
pub fn throw_klv_decode(env: &mut JNIEnv, kind: &str, message: &str) {
    throw_family(
        env,
        "org/tstrans/KlvDecodeException",
        "Lorg/tstrans/KlvDecodeException$Kind;",
        kind,
        message,
    );
}

/// Construct + throw `org.tstrans.KlvEncodeException(Kind.<kind>, tag, message)`.
/// `tag` = `None` → uses the `(Kind, String)` ctor; `Some(t)` → uses
/// `(Kind, Long, String)`. The ratchet greps for `throw_klv_encode(env, "<CONST>", ...)`.
pub fn throw_klv_encode(env: &mut JNIEnv, kind: &str, tag: Option<u64>, message: &str) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    if let Err(e) = throw_klv_encode_inner(env, kind, tag, message) {
        // Fallback: a plain RuntimeException so the failure is never silent.
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!("KlvEncodeException throw failed ({kind}): {e}"),
        );
    }
}

fn throw_klv_encode_inner(
    env: &mut JNIEnv,
    kind: &str,
    tag: Option<u64>,
    message: &str,
) -> jni::errors::Result<()> {
    let kind_sig = "Lorg/tstrans/KlvEncodeException$Kind;";
    let kind_val = env
        .get_static_field("org/tstrans/KlvEncodeException$Kind", kind, kind_sig)?
        .l()?;
    let msg = env.new_string(message)?;
    let exc = match tag {
        Some(t) => {
            let boxed = env.new_object("java/lang/Long", "(J)V", &[JValue::Long(t as i64)])?;
            env.new_object(
                "org/tstrans/KlvEncodeException",
                format!("({kind_sig}Ljava/lang/Long;Ljava/lang/String;)V"),
                &[
                    JValue::Object(&kind_val),
                    JValue::Object(&boxed),
                    JValue::Object(&msg),
                ],
            )?
        }
        None => env.new_object(
            "org/tstrans/KlvEncodeException",
            format!("({kind_sig}Ljava/lang/String;)V"),
            &[JValue::Object(&kind_val), JValue::Object(&msg)],
        )?,
    };
    env.throw(JThrowable::from(exc))
}

/// Map + throw a Rust `KlvDecodeError`. All 7 Kind literals appear inline
/// (satisfies the error-mapping ratchet). Used by the per-set JNI fns (Tasks 1–4).
pub fn map_klv_decode_error(env: &mut JNIEnv, e: &KlvDecodeError) {
    let msg = e.to_string();
    match e {
        KlvDecodeError::Truncated { .. }
        | KlvDecodeError::MalformedLength { .. }
        | KlvDecodeError::LengthOverflow { .. } => throw_klv_decode(env, "TRUNCATED_SET", &msg),
        KlvDecodeError::UnexpectedUniversalLabel { .. } => {
            throw_klv_decode(env, "BAD_UNIVERSAL_LABEL", &msg)
        }
        KlvDecodeError::ChecksumMismatch { .. } | KlvDecodeError::Crc32Mismatch { .. } => {
            throw_klv_decode(env, "CHECKSUM_MISMATCH", &msg)
        }
        KlvDecodeError::DuplicateTag { .. } => throw_klv_decode(env, "DUPLICATE_TAG", &msg),
        KlvDecodeError::Tag2NotFirst
        | KlvDecodeError::Tag1NotLast
        | KlvDecodeError::MissingTag65
        | KlvDecodeError::St0102MissingRequiredTag { .. }
        | KlvDecodeError::St0903MissingRequiredTag { .. } => {
            throw_klv_decode(env, "MISSING_REQUIRED_TAG", &msg)
        }
        KlvDecodeError::MalformedTag { .. }
        | KlvDecodeError::NonCanonicalLength { .. }
        | KlvDecodeError::NonCanonicalTag { .. }
        | KlvDecodeError::TrailingBytes { .. }
        | KlvDecodeError::BadTimeStampPackLength { .. }
        | KlvDecodeError::ReservedBitsInvalid { .. }
        | KlvDecodeError::St0903InvalidVTargetPack { .. }
        | KlvDecodeError::FieldError(_) => throw_klv_decode(env, "MALFORMED_BYTES", &msg),
        _ => throw_klv_decode(env, "INTERNAL", &msg),
    }
}

/// Map + throw a Rust `KlvEncodeError`. All 11 Kind literals appear inline
/// (satisfies the error-mapping ratchet). Used by the per-set JNI fns (Tasks 1–4).
/// The forward-compat wildcard arm aliases to `BUFFER_TOO_SMALL` (matching
/// tst-py's `klv_encode_error_to_pyerr`), not `INTERNAL`.
pub fn map_klv_encode_error(env: &mut JNIEnv, e: &KlvEncodeError) {
    let msg = e.to_string();
    match e {
        KlvEncodeError::BufferTooSmall { .. } => {
            throw_klv_encode(env, "BUFFER_TOO_SMALL", None, &msg)
        }
        KlvEncodeError::RecordTooLarge => throw_klv_encode(env, "RECORD_TOO_LARGE", None, &msg),
        KlvEncodeError::OutOfRange { tag, .. } => {
            throw_klv_encode(env, "OUT_OF_RANGE", Some(u64::from(*tag)), &msg)
        }
        KlvEncodeError::StringTooLong { tag, .. } => {
            throw_klv_encode(env, "STRING_TOO_LONG", Some(u64::from(*tag)), &msg)
        }
        KlvEncodeError::UnsupportedImapbLength { .. } => {
            throw_klv_encode(env, "UNSUPPORTED_IMAPB_LENGTH", None, &msg)
        }
        KlvEncodeError::InvalidImapbParams { .. } => {
            throw_klv_encode(env, "INVALID_IMAPB_PARAMS", None, &msg)
        }
        KlvEncodeError::MissingMandatoryItem { tag, .. } => {
            throw_klv_encode(env, "MISSING_MANDATORY_ITEM", Some(u64::from(*tag)), &msg)
        }
        KlvEncodeError::ReservedTagInUnknown { tag } => {
            throw_klv_encode(env, "RESERVED_TAG_IN_UNKNOWN", Some(u64::from(*tag)), &msg)
        }
        KlvEncodeError::VTargetPackEmpty { target_id } => {
            throw_klv_encode(env, "VTARGET_PACK_EMPTY", Some(*target_id), &msg)
        }
        KlvEncodeError::DuplicateTargetId { target_id } => {
            // Hoist the boxed tag so the `throw_klv_encode(env, "<CONST>", ...)`
            // call stays on one line — required by both rustfmt's width and the
            // error-mapping ratchet's per-constant grep (a brace-less arm with
            // this longer CONST would otherwise split the call across lines and
            // hide the constant from the grep).
            let t = Some(*target_id);
            throw_klv_encode(env, "DUPLICATE_TARGET_ID", t, &msg)
        }
        KlvEncodeError::ForbiddenStandaloneOffset { tag } => {
            let t = Some(u64::from(*tag));
            throw_klv_encode(env, "FORBIDDEN_STANDALONE_OFFSET", t, &msg)
        }
        _ => throw_klv_encode(env, "BUFFER_TOO_SMALL", None, &msg),
    }
}

/// Construct + throw `org.tstrans.CodecParseException`.
/// `kind` MUST be one of the `CodecParseException.Kind` constant names
/// (SCREAMING_SNAKE_CASE). The ratchet greps for `throw_codec(env, "<CONST>", ...)`
/// — note `kind` is the 2nd argument (after `env`), so the literal sits where
/// the ratchet expects it. `message` (LAST arg) is the exception's
/// `getMessage()` text — call sites pass the Rust `Display` string.
pub fn throw_codec(
    env: &mut JNIEnv,
    kind: &str,
    codec: &str,
    fields: &CodecErrFields,
    message: &str,
) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    if let Err(e) = throw_codec_inner(env, kind, codec, fields, message) {
        // Fallback: a plain RuntimeException so the failure is never silent.
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!("CodecParseException throw failed ({kind}): {e}"),
        );
    }
}

/// Box an `Option<i32>` into a `java.lang.Integer` (or null) JNI argument.
fn boxed_int<'local>(
    env: &mut JNIEnv<'local>,
    v: Option<i32>,
) -> jni::errors::Result<JObject<'local>> {
    match v {
        Some(n) => env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(n)]),
        None => Ok(JObject::null()),
    }
}

fn throw_codec_inner(
    env: &mut JNIEnv,
    kind: &str,
    codec: &str,
    fields: &CodecErrFields,
    message: &str,
) -> jni::errors::Result<()> {
    // 17 local refs are live at the `new_object` call (kind_val + codec_str +
    // field_str + 12 boxed ints + msg + exc), over the JNI-guaranteed 16-slot
    // floor — reserve headroom up front, matching the KLV-builder house pattern.
    env.ensure_local_capacity(20)?;
    let kind_sig = "Lorg/tstrans/CodecParseException$Kind;";
    let kind_val = env
        .get_static_field("org/tstrans/CodecParseException$Kind", kind, kind_sig)?
        .l()?;
    let codec_str = env.new_string(codec)?;
    let field_str = match &fields.field {
        Some(f) => env.new_string(f)?.into(),
        None => JObject::null(),
    };
    let offset_bits = boxed_int(env, fields.offset_bits)?;
    let needed_bits = boxed_int(env, fields.needed_bits)?;
    let value = boxed_int(env, fields.value)?;
    let profile_idc = boxed_int(env, fields.profile_idc)?;
    let sps_id = boxed_int(env, fields.sps_id)?;
    let vps_id = boxed_int(env, fields.vps_id)?;
    let offset_bytes = boxed_int(env, fields.offset_bytes)?;
    let expected = boxed_int(env, fields.expected)?;
    let found = boxed_int(env, fields.found)?;
    let needed = boxed_int(env, fields.needed)?;
    let had = boxed_int(env, fields.had)?;
    let layer = boxed_int(env, fields.layer)?;

    // Canonical 16-arg ctor: (Kind, String codec, String message, then the
    // 13 nullable diagnostic fields in declaration order). The message slot
    // carries the Rust `Display` string forwarded by the call site (mirrors
    // tst-py's `format!("{err}")` and `throw_klv_decode`/`throw_demux`).
    let msg = env.new_string(message)?;
    let ctor_sig = "(Lorg/tstrans/CodecParseException$Kind;\
Ljava/lang/String;Ljava/lang/String;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/String;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/Integer;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/Integer;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/Integer;\
Ljava/lang/Integer;)V";
    let exc = env.new_object(
        "org/tstrans/CodecParseException",
        ctor_sig,
        &[
            JValue::Object(&kind_val),
            JValue::Object(&codec_str),
            JValue::Object(&msg),
            JValue::Object(&offset_bits),
            JValue::Object(&needed_bits),
            JValue::Object(&field_str),
            JValue::Object(&value),
            JValue::Object(&profile_idc),
            JValue::Object(&sps_id),
            JValue::Object(&vps_id),
            JValue::Object(&offset_bytes),
            JValue::Object(&expected),
            JValue::Object(&found),
            JValue::Object(&needed),
            JValue::Object(&had),
            JValue::Object(&layer),
        ],
    )?;
    env.throw(JThrowable::from(exc))
}

/// Map + throw a Rust `CodecParseError`. All 12 Kind literals appear inline as
/// the 2nd argument to `throw_codec` (satisfies the error-mapping ratchet).
/// `codec` is a short lowercase codec name (e.g. `"h264"`). Mirrors tst-py's
/// `codec_parse_error_to_pyerr` variant-for-variant; the wildcard arm routes
/// any future marked-non-exhaustive variant to `ENGINE_ERROR`.
pub fn map_codec_parse_error(env: &mut JNIEnv, e: &CodecParseError, codec: &str) {
    // The exception message is the Rust `Display` string (mirrors tst-py's
    // `format!("{err}")`); forwarded to every `throw_codec` call below.
    let msg = e.to_string();
    // NOTE: each arm binds the per-variant fields to `f` first, then makes the
    // `throw_codec(env, "<KIND>", codec, &f, &msg)` call on ONE line so the
    // error-mapping ratchet (a line-oriented grep for
    // `throw_codec\s*\(\s*[^,]*,\s*"<KIND>"`) sees `env, "<KIND>"` together.
    match e {
        CodecParseError::TruncatedRbsp {
            offset_bits,
            needed_bits,
        } => {
            let f = CodecErrFields {
                offset_bits: Some(*offset_bits as i32),
                needed_bits: Some(*needed_bits as i32),
                ..Default::default()
            };
            throw_codec(env, "TRUNCATED_RBSP", codec, &f, &msg)
        }
        CodecParseError::InvalidGolomb { offset_bits } => {
            let f = CodecErrFields {
                offset_bits: Some(*offset_bits as i32),
                ..Default::default()
            };
            throw_codec(env, "INVALID_GOLOMB", codec, &f, &msg)
        }
        CodecParseError::ReservedValue { field, value } => {
            let f = CodecErrFields {
                field: Some((*field).to_string()),
                value: Some(*value as i32),
                ..Default::default()
            };
            throw_codec(env, "RESERVED_VALUE", codec, &f, &msg)
        }
        CodecParseError::UnsupportedProfile { profile_idc } => {
            let f = CodecErrFields {
                profile_idc: Some(i32::from(*profile_idc)),
                ..Default::default()
            };
            throw_codec(env, "UNSUPPORTED_PROFILE", codec, &f, &msg)
        }
        CodecParseError::DanglingSpsReference { sps_id } => {
            let f = CodecErrFields {
                sps_id: Some(i32::from(*sps_id)),
                ..Default::default()
            };
            throw_codec(env, "DANGLING_SPS_REFERENCE", codec, &f, &msg)
        }
        CodecParseError::DanglingVpsReference { vps_id } => {
            let f = CodecErrFields {
                vps_id: Some(i32::from(*vps_id)),
                ..Default::default()
            };
            throw_codec(env, "DANGLING_VPS_REFERENCE", codec, &f, &msg)
        }
        CodecParseError::EngineError(_) => {
            throw_codec(env, "ENGINE_ERROR", codec, &CodecErrFields::default(), &msg)
        }
        CodecParseError::InvalidLeb128 { offset_bytes } => {
            let f = CodecErrFields {
                offset_bytes: Some(*offset_bytes as i32),
                ..Default::default()
            };
            throw_codec(env, "INVALID_LEB128", codec, &f, &msg)
        }
        CodecParseError::BadSyncWord { expected, found } => {
            let f = CodecErrFields {
                expected: Some(i32::from(*expected)),
                found: Some(i32::from(*found)),
                ..Default::default()
            };
            throw_codec(env, "BAD_SYNC_WORD", codec, &f, &msg)
        }
        CodecParseError::Truncated { needed, had } => {
            let f = CodecErrFields {
                needed: Some(*needed as i32),
                had: Some(*had as i32),
                ..Default::default()
            };
            throw_codec(env, "TRUNCATED", codec, &f, &msg)
        }
        CodecParseError::Forbidden { field } => {
            let f = CodecErrFields {
                field: Some((*field).to_string()),
                ..Default::default()
            };
            throw_codec(env, "FORBIDDEN", codec, &f, &msg)
        }
        CodecParseError::UnsupportedFreeFormat { layer } => {
            let f = CodecErrFields {
                layer: Some(i32::from(*layer)),
                ..Default::default()
            };
            throw_codec(env, "UNSUPPORTED_FREE_FORMAT", codec, &f, &msg)
        }
        // Catch-all for marked-non-exhaustive additions not yet mapped:
        _ => throw_codec(env, "ENGINE_ERROR", codec, &CodecErrFields::default(), &msg),
    }
}

/// Shared builder: looks up `Kind.<kind>` static field, calls the
/// `(<kind_sig>, String)` constructor, throws the result.
pub(crate) fn throw_kinded(
    env: &mut JNIEnv,
    exc_class: &str,
    kind_sig: &str,
    kind: &str,
    message: &str,
) -> jni::errors::Result<()> {
    let kind_class = format!("{exc_class}$Kind");
    let kind_val = env.get_static_field(&kind_class, kind, kind_sig)?.l()?;
    let msg = env.new_string(message)?;
    let ctor_sig = format!("({kind_sig}Ljava/lang/String;)V");
    let exc: JObject = env.new_object(
        exc_class,
        &ctor_sig,
        &[JValue::Object(&kind_val), JValue::Object(&msg)],
    )?;
    env.throw(jni::objects::JThrowable::from(exc))
}

/// Shared per-family wrapper around [`throw_kinded`]: bails if an exception is
/// already pending, then falls back to a plain `RuntimeException` naming
/// `exc_class`'s simple name if construction itself fails. Every
/// `throw_<family>` one-liner (`throw_demux`/`throw_mux`/`throw_klv_decode`
/// here, plus `throw_srt` in `srt::errors` and `throw_rtp`/`throw_rtsp` in
/// `rtp::errors`) calls this — the per-family fn names stay because the jvm
/// error-mapping ratchet greps for `throw_<family>(env, "<KIND>", ...)`.
pub(crate) fn throw_family(
    env: &mut JNIEnv,
    exc_class: &str,
    kind_sig: &str,
    kind: &str,
    message: &str,
) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    if let Err(e) = throw_kinded(env, exc_class, kind_sig, kind, message) {
        // Fallback: a plain RuntimeException so the failure is never silent.
        let simple_name = exc_class.rsplit('/').next().unwrap_or(exc_class);
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!("{simple_name} throw failed ({kind}): {e}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::{BrokenCause, TransportError};

    /// Every kind a `TransportError` can project to is declared for BOTH
    /// transport domains, so `throw_binding` never hits its "undeclared
    /// kind" fallback on the hot path. (`Domain::kinds` is what the
    /// load-time check resolves against the Java enums.)
    #[test]
    fn transport_error_kinds_are_declared_for_srt_and_rtp() {
        let variants = [
            TransportError::Backpressure {
                msg: "q".into(),
                errno_code: None,
            },
            TransportError::Broken {
                msg: "b".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            },
            TransportError::Closed,
            TransportError::ExplicitClose,
            TransportError::TooLarge { len: 2, max: 1 },
        ];
        for v in variants {
            let kind = BindingError::from(v.clone()).kind;
            assert!(
                SRT_KINDS.contains(&kind),
                "{v:?} → {} missing from SRT_KINDS",
                kind.variant_name()
            );
            assert!(
                RTP_KINDS.contains(&kind),
                "{v:?} → {} missing from RTP_KINDS",
                kind.variant_name()
            );
        }
        assert_eq!(
            BindingError::from(TransportError::ExplicitClose)
                .kind
                .name(),
            "CLOSED"
        );
        assert_eq!(
            BindingError::from(TransportError::ExplicitClose).detail,
            "cancelled from another thread"
        );
    }

    /// The eight domains' declared sets have the sizes the Java enums will
    /// have after Task B3.6 and contain no duplicate MEMBER (two kinds with
    /// the same `name()` in one domain would be unresolvable by name).
    #[test]
    fn domain_kind_sets_are_deduplicated_and_sized() {
        for (d, n) in [
            (Domain::Srt, 10),
            (Domain::Rtp, 10),
            (Domain::Rtsp, 10),
            (Domain::Demux, 6),
            (Domain::Mux, 9),
            (Domain::KlvDecode, 7),
            (Domain::KlvEncode, 11),
            (Domain::Codec, 15),
        ] {
            let members: std::collections::BTreeSet<&str> =
                d.kinds().iter().map(|k| k.name()).collect();
            assert_eq!(
                members.len(),
                d.kinds().len(),
                "{d:?} has a duplicate member"
            );
            assert_eq!(members.len(), n, "{d:?}: {members:?}");
        }
        assert_eq!(Domain::ALL.len(), 8);
    }

    /// The demux / mux / klv / codec classifiers land inside their domains
    /// (Task B3.5b routes them through `throw_binding`).
    #[test]
    fn core_family_classifiers_are_declared() {
        use tst_core::error::{DemuxError, KlvDecodeError, MuxError};
        use tst_pipeline::binding::kind::{kind_of_demux, kind_of_klv_decode, kind_of_mux};
        assert!(MUX_KINDS.contains(&kind_of_mux(&MuxError::InvalidNal)));
        assert_eq!(kind_of_mux(&MuxError::InvalidNal).name(), "INVALID_NAL");
        assert!(DEMUX_KINDS.contains(&kind_of_demux(&DemuxError::StrictRejection(String::new()))));
        assert!(
            KLV_DECODE_KINDS.contains(&kind_of_klv_decode(&KlvDecodeError::Truncated {
                offset: 0,
                needed: 2,
                have: 1,
            }))
        );
    }
}
