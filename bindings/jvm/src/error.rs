//! JNI exception construction. One `throw_<family>` per Rust error family,
//! mirroring tst-py's `make_<family>_error` helpers. Each constructs the
//! `org.tstrans.<Family>Exception` object with its `Kind` enum value and throws
//! it. Call these, then return a Rust default from the JNI fn — the pending
//! Java exception is raised when control returns to the JVM.

use jni::JNIEnv;
use jni::objects::{JObject, JThrowable, JValue};
use tst_core::codec::CodecParseError;
use tst_core::error::{KlvDecodeError, KlvEncodeError};
use tst_pipeline::binding::kind::{kind_of_klv_decode, kind_of_klv_field};
use tst_pipeline::binding::{BindingError, BindingErrorKind, DiagValue, Diagnostics, HandleState};

/// Variant-specific diagnostic fields forwarded to `CodecParseException`.
/// Every field is `None` except those the producing `CodecParseError` variant
/// carries; [`codec_fields`] fills it from the error's diagnostics.
#[derive(Default)]
pub struct CodecErrFields {
    pub offset_bits: Option<i32>,
    pub needed_bits: Option<i32>,
    pub field: Option<String>,
    pub value: Option<i32>,
    pub profile_idc: Option<i32>,
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

// Which Java exception class a [`BindingError`] is thrown as. The kind
// alone cannot decide it — `Closed` exists in both `SrtException.Kind`
// and `RtpException.Kind`, `Internal` in four enums — so the throw SITE
// names its domain, and the domain's declared subset
// ([`Domain::kinds`]) is what [`verify_kind_tables`] resolves at load. The
// Java constant is [`BindingErrorKind::name`] (domain prefix stripped);
// [`BindingErrorKind::variant_name`] is the prefixed identity and appears
// only in messages.
pub(crate) use tst_pipeline::binding::Domain;

/// JNI class of the domain's exception. A domain the JVM binding does not
/// expose (`java_exception_class` is `None`) falls back to `RuntimeException`.
pub(crate) fn exc_class(domain: Domain) -> &'static str {
    domain
        .java_exception_class()
        .unwrap_or("java/lang/RuntimeException")
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
pub(crate) fn throw_binding(env: &mut JNIEnv, domain: Domain, e: &BindingError) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    if e.kind == BindingErrorKind::Internal && !domain.kinds().contains(&e.kind) {
        // As Python raises RuntimeError: an internal failure on a domain
        // whose Kind enum has no INTERNAL member (demux) is not a domain error.
        let _ = env.throw_new("java/lang/RuntimeException", e.detail.as_str());
        return;
    }
    let class = exc_class(domain);
    let Some(member) = declared_member(env, domain, e.kind, &e.detail) else {
        return;
    };
    let kind_sig = format!("L{class}$Kind;");
    if let Err(err) = throw_kinded(env, class, &kind_sig, member, &e.detail) {
        let simple_name = class.rsplit('/').next().unwrap_or(class);
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
    // Callers that reach this directly (the `KlvEncodeException` /
    // `CodecParseException` throwers) have no `throw_binding`
    // wrapper above them, so guard here too: never clobber a pending exception.
    if env.exception_check().unwrap_or(false) {
        return None;
    }
    let _ = env.throw_new(
        "java/lang/RuntimeException",
        format!(
            "tst-jni: kind {} is not declared for {}$Kind (add it to `Domain::kinds` in kind.rs and regenerate the Java enum); detail: {detail}",
            kind.variant_name(),
            exc_class(domain)
        ),
    );
    None
}

/// The ONE [`HandleState`] → Java mapping:
/// `Closed` → `IllegalStateException("<what> is closed")` (the Java-side
/// `NativeHandle.ensureOpen` guard throws the same type BEFORE the native
/// runs, so a native-side `Closed` — only reachable in a close race — must
/// not differ); `Poisoned` → `IllegalStateException`; `Panicked` → the
/// `RuntimeException("native panic in tst-jni: …")` [`crate::panic::jni_catch`]
/// already produces for an uncaught panic, so a panic reads the same whether
/// `Owned::with_mut` or the outer boundary caught it.
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
///
/// `get_static_field_id`, NOT `get_static_field`: the latter READS the field
/// and hands back a `JValueOwned::Object` — a JNI local ref jni-rs does not
/// auto-delete. This loop visits every member of the nine JVM domains in one frame,
/// well past the 16-slot default local-reference capacity, and `-Xcheck:jni`
/// does not report local-ref accumulation. The field ID
/// lookup validates the member name AND its signature identically while
/// minting no object reference at all.
pub(crate) fn verify_kind_tables(env: &mut JNIEnv) {
    for domain in Domain::ALL
        .iter()
        .copied()
        .filter(|d| d.java_exception_class().is_some())
    {
        let kind_class = format!("{}$Kind", exc_class(domain));
        let kind_sig = format!("L{kind_class};");
        for kind in domain.kinds() {
            let resolved = env.get_static_field_id(&kind_class, kind.name(), &kind_sig);
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
pub fn throw_demux(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Demux, &BindingError::new(kind, message));
}

/// Construct + throw `org.tstrans.MuxException(Kind.<kind>, message)`.
/// `kind` MUST be one of the `MuxException.Kind` enum constant names
/// (SCREAMING_SNAKE_CASE), matching the 5-variant `MuxErrorKind` buckets.
pub fn throw_mux(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Mux, &BindingError::new(kind, message));
}

/// Construct + throw `org.tstrans.KlvDecodeException(Kind.<kind>, message)`.
/// The kind is resolved to its Java member by [`declared_member`].
pub fn throw_klv_decode(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::KlvDecode, &BindingError::new(kind, message));
}

/// Raw-member variant for the `jni-test-hooks` probes ONLY
/// (`klv/mod.rs::nRaiseDecodeForTest`): the test names a Java member directly,
/// so there is no `BindingErrorKind` to resolve. Never used on a real error
/// path — those all go through [`throw_klv_decode`].
pub fn throw_klv_decode_raw(env: &mut JNIEnv, member: &str, message: &str) {
    throw_family(
        env,
        "org/tstrans/KlvDecodeException",
        "Lorg/tstrans/KlvDecodeException$Kind;",
        member,
        message,
    );
}

/// [`throw_klv_decode_raw`]'s encode twin (`klv/mod.rs::nRaiseEncodeForTest`).
/// `tag` = `None` → the `(Kind, String)` ctor; `Some(t)` → `(Kind, Long, String)`.
pub fn throw_klv_encode_raw(env: &mut JNIEnv, member: &str, tag: Option<u64>, message: &str) {
    if env.exception_check().unwrap_or(false) {
        return;
    }
    if let Err(e) = throw_klv_encode_inner(env, member, tag, message) {
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!("KlvEncodeException throw failed ({member}): {e}"),
        );
    }
}

/// Construct + throw `org.tstrans.KlvEncodeException(Kind, [Long tag,] String)`.
/// `tag` = `None` → the `(Kind, String)` ctor; `Some(t)` → `(Kind, Long, String)`.
pub fn throw_klv_encode(env: &mut JNIEnv, kind: BindingErrorKind, tag: Option<u64>, message: &str) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    // The wider (Kind, Long, String) ctor keeps its own builder; the kind check
    // and member resolution are the shared ones.
    let Some(member) = declared_member(env, Domain::KlvEncode, kind, message) else {
        return;
    };
    if let Err(e) = throw_klv_encode_inner(env, member, tag, message) {
        // Fallback: a plain RuntimeException so the failure is never silent.
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!(
                "KlvEncodeException throw failed ({}): {e}",
                kind.variant_name()
            ),
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

/// Map + throw a Rust `KlvDecodeError` through the shared classifier — one source of
/// truth for the buckets, shared with C and Python (the hand table this used to
/// carry is gone). `FieldError` routes through `kind_of_klv_field` so a field
/// error keeps its own classification rather than collapsing into the
/// enclosing-set bucket.
pub fn map_klv_decode_error(env: &mut JNIEnv, e: &KlvDecodeError) {
    let kind = match e {
        KlvDecodeError::FieldError(f) => kind_of_klv_field(f),
        other => kind_of_klv_decode(other),
    };
    throw_klv_decode(env, kind, &e.to_string());
}

/// Map + throw a Rust `KlvEncodeError`. The kind and the exception's `tag`
/// (a KLV tag, or a VTarget id) come from the binding envelope the error
/// converts to. Used by the per-set JNI fns.
pub fn map_klv_encode_error(env: &mut JNIEnv, e: KlvEncodeError) {
    let b = BindingError::from(e);
    throw_klv_encode(env, b.kind, b.diagnostics.tag, &b.detail);
}

/// Construct + throw `org.tstrans.CodecParseException`.
/// The kind is resolved to its Java member by [`declared_member`].
/// `message` (LAST arg) is the exception's
/// `getMessage()` text — call sites pass the Rust `Display` string.
pub fn throw_codec(
    env: &mut JNIEnv,
    kind: BindingErrorKind,
    codec: &str,
    fields: &CodecErrFields,
    message: &str,
) {
    if env.exception_check().unwrap_or(false) {
        return; // don't clobber an already-pending exception
    }
    // The wider fielded ctor keeps its own builder; the kind check and member
    // resolution are the shared ones.
    let Some(member) = declared_member(env, Domain::Codec, kind, message) else {
        return;
    };
    if let Err(e) = throw_codec_inner(env, member, codec, fields, message) {
        // Fallback: a plain RuntimeException so the failure is never silent.
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            format!(
                "CodecParseException throw failed ({}): {e}",
                kind.variant_name()
            ),
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
    let offset_bytes = boxed_int(env, fields.offset_bytes)?;
    let expected = boxed_int(env, fields.expected)?;
    let found = boxed_int(env, fields.found)?;
    let needed = boxed_int(env, fields.needed)?;
    let had = boxed_int(env, fields.had)?;
    let layer = boxed_int(env, fields.layer)?;

    // Canonical 14-arg ctor: (Kind, String codec, String message, then the
    // 11 nullable diagnostic fields in declaration order). The message slot
    // carries the Rust `Display` string forwarded by the call site (mirrors
    // tst-py's `format!("{err}")` and `throw_klv_decode`/`throw_demux`).
    let msg = env.new_string(message)?;
    let ctor_sig = "(Lorg/tstrans/CodecParseException$Kind;\
Ljava/lang/String;Ljava/lang/String;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/String;\
Ljava/lang/Integer;Ljava/lang/Integer;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/Integer;\
Ljava/lang/Integer;Ljava/lang/Integer;Ljava/lang/Integer;)V";
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

/// The exception's integer and string fields from a codec failure's
/// per-field values. A name that is not one of the eleven fields is skipped.
/// The exception spells two of the source names differently: `got` is
/// `value` and `have` is `had`.
fn codec_fields(d: &Diagnostics) -> CodecErrFields {
    let mut f = CodecErrFields::default();
    for (name, v) in &d.attrs {
        let int = match v {
            DiagValue::Int(i) => Some(i32::try_from(*i).unwrap_or(i32::MAX)),
            DiagValue::Str(_) => None,
        };
        match *name {
            "offset_bits" => f.offset_bits = int,
            "needed_bits" => f.needed_bits = int,
            "field" => {
                if let DiagValue::Str(s) = v {
                    f.field = Some((*s).to_string());
                }
            }
            "value" | "got" => f.value = int,
            "profile_idc" => f.profile_idc = int,
            "offset_bytes" => f.offset_bytes = int,
            "expected" => f.expected = int,
            "found" => f.found = int,
            "needed" => f.needed = int,
            "had" | "have" => f.had = int,
            "layer" => f.layer = int,
            _ => {}
        }
    }
    f
}

/// Map + throw a Rust `CodecParseError`. The kind and the exception's fields
/// come from the binding envelope the error converts to. `codec` is a short
/// lowercase codec name (e.g. `"h264"`).
pub fn map_codec_parse_error(env: &mut JNIEnv, e: &CodecParseError, codec: &str) {
    let b = BindingError::from(e.clone());
    throw_codec(env, b.kind, codec, &codec_fields(&b.diagnostics), &b.detail);
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
                Domain::Srt.kinds().contains(&kind),
                "{v:?} → {} missing from Domain::Srt.kinds()",
                kind.variant_name()
            );
            assert!(
                Domain::Rtp.kinds().contains(&kind),
                "{v:?} → {} missing from Domain::Rtp.kinds()",
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

    /// The JVM domains' declared sets have the sizes the Java enums
    /// declare and contain no duplicate MEMBER (two kinds with
    /// the same `name()` in one domain would be unresolvable by name).
    #[test]
    fn domain_kind_sets_are_deduplicated_and_sized() {
        for (d, n) in [
            (Domain::Srt, 11),
            (Domain::Rtp, 11),
            (Domain::Rtsp, 11), // + CLOSED (a cancelled control call, a consumed data plane)
            (Domain::Demux, 4),
            (Domain::Mux, 9),
            (Domain::KlvDecode, 7),
            (Domain::KlvEncode, 11),
            (Domain::Codec, 13),
            (Domain::Hls, 10),
            (Domain::Udp, 10),
            (Domain::Tcp, 13),
            (Domain::Rist, 13),
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
        assert_eq!(
            Domain::ALL
                .iter()
                .filter(|d| d.java_exception_class().is_some())
                .count(),
            12
        );
    }

    /// A receiver shell's clean end of stream projects to a kind the SRT
    /// domain DECLARES, so `throw_recv_transport` can raise it instead of
    /// hitting `throw_binding`'s undeclared-kind guard. The projection is
    /// the shared exhaustive `From<ShellErrorKind>`, the same one C uses to
    /// reach `TST_E_END_OF_STREAM` (-12) — this pins that the three bindings
    /// agree on the kind, which a JVM-only loopback cannot demonstrate (no
    /// JVM caller sets `SRTO_SENDER`, so no JVM peer hangs up cleanly).
    #[test]
    fn end_of_stream_is_declared_for_srt() {
        use tst_pipeline::ShellErrorKind;
        let kind = BindingErrorKind::from(ShellErrorKind::EndOfStream);
        assert_eq!(kind, BindingErrorKind::EndOfStream);
        assert_eq!(kind.name(), "END_OF_STREAM");
        assert_eq!(
            kind.c_projection(),
            -12,
            "must equal C's TST_E_END_OF_STREAM"
        );
        assert!(
            Domain::Srt.kinds().contains(&kind),
            "SrtException.Kind must declare END_OF_STREAM"
        );
    }

    /// The demux / mux / klv / codec classifiers land inside their domains
    /// (routed through `throw_binding`).
    #[test]
    fn core_family_classifiers_are_declared() {
        use tst_core::error::{DemuxError, KlvDecodeError, MuxError};
        use tst_pipeline::binding::kind::{kind_of_demux, kind_of_klv_decode, kind_of_mux};
        assert!(
            Domain::Mux
                .kinds()
                .contains(&kind_of_mux(&MuxError::InvalidNal))
        );
        assert_eq!(kind_of_mux(&MuxError::InvalidNal).name(), "INVALID_NAL");
        assert!(
            Domain::Demux
                .kinds()
                .contains(&kind_of_demux(&DemuxError::StrictRejection(String::new())))
        );
        assert!(Domain::KlvDecode.kinds().contains(&kind_of_klv_decode(
            &KlvDecodeError::Truncated {
                offset: 0,
                needed: 2,
                have: 1,
            }
        )));
    }
}
