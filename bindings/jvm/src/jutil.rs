//! Shared JNI marshalling helpers used across the klv (and potentially other)
//! binding modules. Extracted from `mpegts/mod.rs` where `wrap_heap_byte_buffer`
//! originated; the klv module re-uses it for KLV byte payloads.

use jni::Env;
use jni::descriptors::Desc;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JClass, JObject, JValue};
use jni::refs::{Auto, IntoAuto as _};
use jni::signature::RuntimeFieldSignature;
use jni::strings::{JNIStr, JNIString};
use jni::sys::jlong;
use tst_core::error::KlvFieldError as RustKlvFieldError;
use tst_core::klv::OwnedRawField;

/// A class named by its internal name (`org/tstrans/...`), resolved with a
/// direct JNI `FindClass`, for every by-name lookup this binding makes.
///
/// jni 0.22 resolves a bare name through `LoaderContext::None`: it tries the
/// thread's context class loader with `Class.forName` first and falls back to
/// `FindClass` only on `NoClassDefFound`. That can pick another loader than
/// the one that declared the calling native, which is the loader `FindClass`
/// uses and the one 0.21 used. It also costs about five JNI calls and a
/// local-frame push/pop per lookup, where `FindClass` is one call (plus the
/// `DeleteLocalRef` of the result, as in 0.21).
pub(crate) struct FindClass<S>(pub S);

// SAFETY: `lookup` returns the class `FindClass` resolved for exactly the
// requested name, never another class.
unsafe impl<'local, S: AsRef<JNIStr>> Desc<'local, JClass<'local>> for FindClass<S> {
    type Output = Auto<'local, JClass<'local>>;

    fn lookup(self, env: &mut Env<'local>) -> jni::errors::Result<Self::Output> {
        Ok(env.find_class(self.0)?.auto())
    }
}

/// Decode a packed stream-handle `jlong` into a typed stream handle.
///
/// Rejects any value outside the packed-`u32` layout (negative, above `u32::MAX`,
/// or a bit-pattern that `decode` considers invalid) and returns `None`. Used by
/// every targeted-push native (muxer and MuxSender) to avoid repeating the
/// `u32::try_from(raw).ok().and_then(|r| H::try_from_raw(r).ok())` chain.
pub fn decode_stream_handle<H, E>(
    raw: jlong,
    decode: impl FnOnce(u32) -> Result<H, E>,
) -> Option<H> {
    u32::try_from(raw).ok().and_then(|r| decode(r).ok())
}

/// Copy `bytes` into a fresh Java `byte[]` and wrap it as a heap `ByteBuffer`
/// (`java.nio.ByteBuffer.wrap`). The returned buffer is backed by JVM-owned
/// memory, safe to retain past the next call / after `close()`. Used across
/// the per-set KLV modules.
pub fn wrap_heap_byte_buffer<'local>(
    env: &mut Env<'local>,
    bytes: &[u8],
) -> Result<JObject<'local>, ()> {
    let arr = env.byte_array_from_slice(bytes).map_err(|_| ())?;
    env.call_static_method(
        FindClass(jni_str!("java/nio/ByteBuffer")),
        jni_str!("wrap"),
        jni_sig!("([B)Ljava/nio/ByteBuffer;"),
        &[JValue::Object(&arr)],
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}

/// Map a `RustKlvFieldError` variant to a `(KlvFieldErrorKind name, tag)` pair.
/// Mirrors tst-py's `convert_field_error` arm-for-arm; wildcard → `INVALID_LENGTH`.
fn field_error_kind_and_tag(fe: &RustKlvFieldError) -> (&'static str, u32) {
    match fe {
        RustKlvFieldError::OutOfRange { tag, .. } => ("OUT_OF_RANGE", *tag),
        RustKlvFieldError::InvalidUtf8 { tag } => ("INVALID_UTF8", *tag),
        RustKlvFieldError::InvalidLength { tag, .. } => ("INVALID_LENGTH", *tag),
        RustKlvFieldError::InvalidUtf16 { tag } => ("INVALID_UTF16", *tag),
        RustKlvFieldError::InvalidCodepoint { tag, .. } => ("INVALID_CODEPOINT", *tag),
        RustKlvFieldError::TruncatedField { tag } => ("TRUNCATED_FIELD", *tag),
        RustKlvFieldError::UnsupportedImapbLength { .. } => ("UNSUPPORTED_IMAPB_LENGTH", 0),
        RustKlvFieldError::InvalidImapbParams { .. } => ("INVALID_IMAPB_PARAMS", 0),
        _ => ("INVALID_LENGTH", 0),
    }
}

/// Build a `java.util.List<KlvFieldError>` (an `ArrayList`) from a slice of
/// `RustKlvFieldError`s. Used by the per-set KLV modules.
///
/// An ST 0601 record's `field_errors` can exceed the default 16-slot JNI
/// local-ref table, so each iteration's element refs (the kind constant, the
/// message string, the record) are minted inside a per-entry
/// `with_local_frame` and reclaimed at the end of the iteration — only the
/// long-lived `list` ref (created in the outer frame) survives. Mirrors the
/// forward-note on `build_pid_list` in `mpegts/mod.rs`.
pub fn build_field_errors<'local>(
    env: &mut Env<'local>,
    errs: &[RustKlvFieldError],
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object(
        FindClass(jni_str!("java/util/ArrayList")),
        jni_sig!("()V"),
        &[],
    )?;
    for fe in errs {
        // 8 slots: comfortably covers the 3 element refs + JNI scratch per entry.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let (kind_name, tag) = field_error_kind_and_tag(fe);
            let kind_obj = env
                .get_static_field(
                    FindClass(jni_str!("org/tstrans/klv/KlvFieldErrorKind")),
                    JNIString::from(kind_name),
                    jni_sig!("Lorg/tstrans/klv/KlvFieldErrorKind;"),
                )?
                .l()?;
            let msg = env.new_string(fe.to_string())?;
            let obj = env.new_object(
                FindClass(jni_str!("org/tstrans/klv/KlvFieldError")),
                jni_sig!("(Lorg/tstrans/klv/KlvFieldErrorKind;JLjava/lang/String;)V"),
                &[
                    JValue::Object(&kind_obj),
                    JValue::Long(i64::from(tag)),
                    JValue::Object(&msg),
                ],
            )?;
            env.call_method(
                &list,
                jni_str!("add"),
                jni_sig!("(Ljava/lang/Object;)Z"),
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

/// Build a `java.util.List<KlvUnknownField>` (an `ArrayList`) from a slice of
/// [`OwnedRawField`]s. Each entry is a heap `ByteBuffer` copy. Used by the
/// per-set KLV modules (call sites pass `&record.unknown`).
///
/// As with `build_field_errors`, an ST 0601 record's `unknown` list can exceed
/// the default 16-slot JNI local-ref table, so each iteration's element refs
/// (the byte array + ByteBuffer + record) are minted inside a per-entry
/// `with_local_frame` and reclaimed at the end of the iteration — only the
/// long-lived `list` ref survives.
pub fn build_unknown_list<'local>(
    env: &mut Env<'local>,
    fields: &[OwnedRawField],
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object(
        FindClass(jni_str!("java/util/ArrayList")),
        jni_sig!("()V"),
        &[],
    )?;
    for f in fields {
        // 8 slots: covers the byte[] + ByteBuffer + record refs + JNI scratch.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let arr = env.byte_array_from_slice(&f.value)?;
            let buf = env
                .call_static_method(
                    FindClass(jni_str!("java/nio/ByteBuffer")),
                    jni_str!("wrap"),
                    jni_sig!("([B)Ljava/nio/ByteBuffer;"),
                    &[JValue::Object(&arr)],
                )?
                .l()?;
            let obj = env.new_object(
                FindClass(jni_str!("org/tstrans/klv/KlvUnknownField")),
                jni_sig!("(JLjava/nio/ByteBuffer;)V"),
                &[JValue::Long(i64::from(f.tag)), JValue::Object(&buf)],
            )?;
            env.call_method(
                &list,
                jni_str!("add"),
                jni_sig!("(Ljava/lang/Object;)Z"),
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

/// Build a `java.util.List<Long>` (an `ArrayList`) from an iterator of `i64`
/// values. Used to expose `sentinel_tags` to callers.
pub fn build_long_list<'local>(
    env: &mut Env<'local>,
    values: impl Iterator<Item = i64>,
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object(
        FindClass(jni_str!("java/util/ArrayList")),
        jni_sig!("()V"),
        &[],
    )?;
    for v in values {
        // Box the long into java.lang.Long, then add to the list.
        env.with_local_frame(4, |env| -> jni::errors::Result<()> {
            let boxed = env.new_object(
                FindClass(jni_str!("java/lang/Long")),
                jni_sig!("(J)V"),
                &[JValue::Long(v)],
            )?;
            env.call_method(
                &list,
                jni_str!("add"),
                jni_sig!("(Ljava/lang/Object;)Z"),
                &[JValue::Object(&boxed)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

/// Read a `java.util.List<Long>` into a `Vec<i64>`. Sibling of
/// [`build_long_list`] for the read (encode) direction — used by the ST 0601
/// pack fields (`controlCommandVerification`/`activeWavelengths`/`SdccFlpField.precedingTags`).
///
/// Each item is read inside its own 4-slot local frame (the `.get(i)` call
/// mints one local ref; `longValue()` returns a primitive and mints none),
/// mirroring `build_long_list`'s per-item frame and the VTargetPack per-item
/// `with_local_frame` idiom elsewhere in this codebase. Without this, a
/// caller-constructed list longer than the ambient frame's spare capacity
/// (e.g. one built inside another 16-slot per-item frame, as
/// `SdccFlpField.precedingTags` is) exhausts the JNI local-ref table and
/// aborts the JVM rather than throwing a catchable Java exception — the bug
/// this helper closes. Callers narrow the raw `i64` values to their target
/// width (`u32`, via `checked_u32`; a `u64` takes the bit pattern, `n as u64`) themselves.
pub fn read_long_list(env: &mut Env, list: &JObject) -> jni::errors::Result<Vec<i64>> {
    let size = env
        .call_method(list, jni_str!("size"), jni_sig!("()I"), &[])?
        .i()?;
    let mut out = Vec::with_capacity(size.max(0) as usize);
    for i in 0..size {
        let v = env.with_local_frame(4, |inner_env| -> jni::errors::Result<i64> {
            let item = inner_env
                .call_method(
                    list,
                    jni_str!("get"),
                    jni_sig!("(I)Ljava/lang/Object;"),
                    &[JValue::Int(i)],
                )?
                .l()?;
            inner_env
                .call_method(&item, jni_str!("longValue"), jni_sig!("()J"), &[])?
                .j()
        })?;
        out.push(v);
    }
    Ok(out)
}

/// Read a `java.util.List<KlvUnknownField>` back into a `Vec<OwnedRawField>`,
/// dropping any entry whose tag collides with a typed tag (typed wins). Used by
/// the per-set KLV modules (call sites assign the result back to `record.unknown`).
pub fn read_unknown_list(
    env: &mut Env,
    list: &JObject,
    is_typed: impl Fn(u32) -> bool,
) -> jni::errors::Result<Vec<OwnedRawField>> {
    let size = env
        .call_method(list, jni_str!("size"), jni_sig!("()I"), &[])?
        .i()?;
    let mut out = Vec::with_capacity(size.max(0) as usize);
    for i in 0..size {
        // Per-item local frame, like every other list reader in this module:
        // each entry mints ~5 refs (List.get, the ByteBuffer, its duplicate,
        // the byte[] behind read_byte_buffer) and nothing below returns a
        // ref, so the frame can reclaim them all before the next entry.
        let field =
            env.with_local_frame(8, |env| -> jni::errors::Result<Option<OwnedRawField>> {
                let item = env
                    .call_method(
                        list,
                        jni_str!("get"),
                        jni_sig!("(I)Ljava/lang/Object;"),
                        &[JValue::Int(i)],
                    )?
                    .l()?;
                let tag_long = env
                    .call_method(&item, jni_str!("tag"), jni_sig!("()J"), &[])?
                    .j()?;
                // KLV BER-OID tags are u32; a Java `long` tag outside 0..=u32::MAX is
                // out-of-range for a real field. Fail-closed (skip) rather than
                // truncating it into a different tag value.
                let Ok(tag) = u32::try_from(tag_long) else {
                    return Ok(None);
                };
                if is_typed(tag) {
                    return Ok(None); // typed field wins; drop this unknown entry
                }
                let buf_obj = env
                    .call_method(
                        &item,
                        jni_str!("value"),
                        jni_sig!("()Ljava/nio/ByteBuffer;"),
                        &[],
                    )?
                    .l()?;
                // Use read_byte_buffer to honour position/limit and support direct buffers.
                let value = read_byte_buffer(env, &buf_obj)?;
                Ok(Some(OwnedRawField { tag, value }))
            })?;
        if let Some(field) = field {
            out.push(field);
        }
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// ByteBuffer helper
// -----------------------------------------------------------------------

/// Read a `ByteBuffer`'s REMAINING bytes (honouring position/limit), without
/// mutating the caller's buffer (operates on a duplicate). Works for heap AND
/// direct buffers.
pub fn read_byte_buffer(env: &mut Env, buf: &JObject) -> jni::errors::Result<Vec<u8>> {
    // duplicate() gives us an independent position/limit view on the same data,
    // so we can call get([B) without advancing the caller's position.
    let dup = env
        .call_method(
            buf,
            jni_str!("duplicate"),
            jni_sig!("()Ljava/nio/ByteBuffer;"),
            &[],
        )?
        .l()?;
    let remaining = env
        .call_method(&dup, jni_str!("remaining"), jni_sig!("()I"), &[])?
        .i()?;
    // `ByteBuffer.remaining()` is never negative.
    let arr = env.new_byte_array(remaining.max(0) as usize)?;
    env.call_method(
        &dup,
        jni_str!("get"),
        jni_sig!("([B)Ljava/nio/ByteBuffer;"),
        &[JValue::Object(&arr)],
    )?;
    env.convert_byte_array(JByteArray::from(arr))
}

// -----------------------------------------------------------------------
// Null-argument guard
// -----------------------------------------------------------------------

/// Guard a JNI object argument that must be non-null: throw
/// `NullPointerException` naming `what` and return `Err(JavaException)` if
/// `obj` is null.
///
/// Needed because jni's own `non_null!` receiver guard rejects a null with a
/// plain Rust-side `Err(NullPtr)` and NO pending Java exception — a native
/// that maps reader errors to a bare `return null` therefore turns a
/// caller's null argument into a silent null result with no diagnostic at
/// all. Call this FIRST, before any accessor call on the object.
///
/// The `throw_new` result is deliberately ignored (the module-wide idiom —
/// every throw site in these bindings does the same): throwing
/// `java/lang/NullPointerException` can only fail if the class lookup on a
/// core JDK class fails or an exception is already pending, and this guard
/// runs first in the entry point so nothing is pending. If the JNI env is
/// that broken, a fallback throw would fail through the identical
/// mechanism; `panic::jni_catch` remains the outer backstop.
pub fn require_non_null(env: &mut Env, obj: &JObject, what: &str) -> jni::errors::Result<()> {
    if obj.is_null() {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/NullPointerException")),
            JNIString::from(format!("{what} must not be null")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(())
}

// -----------------------------------------------------------------------
// Checked narrowing helpers (encode path)
// -----------------------------------------------------------------------

/// Range-check a Java `int`/`long` value against the u8 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow
/// (matches tst-py's `extract::<u8>()` fail-loud semantics).
pub fn checked_u8(env: &mut Env, value: i64, field: &str) -> jni::errors::Result<u8> {
    if !(0..=255).contains(&value) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("{field} must be 0..=255, got {value}")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u8)
}

/// Range-check a Java `int`/`long` value against the u16 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_u16(env: &mut Env, value: i64, field: &str) -> jni::errors::Result<u16> {
    if !(0..=65535).contains(&value) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("{field} must be 0..=65535, got {value}")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u16)
}

/// Range-check a Java `int` value against the i8 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_i8(env: &mut Env, value: i64, field: &str) -> jni::errors::Result<i8> {
    if !(-128..=127).contains(&value) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("{field} must be -128..=127, got {value}")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as i8)
}

/// Range-check a Java `int` value against the i16 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_i16(env: &mut Env, value: i64, field: &str) -> jni::errors::Result<i16> {
    if !(-32768..=32767).contains(&value) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("{field} must be -32768..=32767, got {value}")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as i16)
}

/// Range-check a Java `long` value against the u32 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_u32(env: &mut Env, value: i64, field: &str) -> jni::errors::Result<u32> {
    if !(0..=u32::MAX as i64).contains(&value) {
        let _ = env.throw_new(
            FindClass(jni_str!("java/lang/IllegalArgumentException")),
            JNIString::from(format!("{field} must be 0..=4294967295, got {value}")),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u32)
}

/// Throws `IllegalArgumentException` for a required record component that is
/// null, and returns the error that carries it.
pub fn null_field(env: &mut Env, field: &str) -> jni::errors::Error {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
        JNIString::from(format!("{field} must not be null")),
    );
    jni::errors::Error::JavaException
}

// -----------------------------------------------------------------------
// Shared nullable accessor helpers (encode path)
// -----------------------------------------------------------------------

/// Read a nullable `Integer` accessor. Returns `None` for null.
pub fn read_nullable_int(
    env: &mut Env,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<i32>> {
    let obj = env
        .call_method(
            rec,
            JNIString::from(name),
            jni_sig!("()Ljava/lang/Integer;"),
            &[],
        )?
        .l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let v = env
        .call_method(&obj, jni_str!("intValue"), jni_sig!("()I"), &[])?
        .i()?;
    Ok(Some(v))
}

/// Join `host:port` in the form `Socket::connect_with` /
/// `Listener::bind_with` resolve: [`tst_srt::addr::join_host_port`].
pub fn join_host_port(host: &str, port: u16) -> String {
    tst_srt::addr::join_host_port(host, port)
}

/// Read a Java `byte[]` argument, or throw `RuntimeException` + return `None`.
pub fn read_bytes(env: &mut Env, arr: &JByteArray) -> Option<Vec<u8>> {
    match env.convert_byte_array(arr) {
        Ok(b) => Some(b),
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            None
        }
    }
}

/// Fetch an `org.tstrans.<package>.<class>.<name>` enum constant via
/// `get_static_field`.
pub fn enum_const<'local>(
    env: &mut Env<'local>,
    package: &str,
    class: &str,
    name: &str,
) -> Result<JObject<'local>, ()> {
    let class_path = format!("org/tstrans/{package}/{class}");
    let descriptor = format!("Lorg/tstrans/{package}/{class};");
    env.get_static_field(
        FindClass(JNIString::from(&class_path)),
        JNIString::from(name),
        RuntimeFieldSignature::from_str(&descriptor)
            .map_err(|_| ())?
            .field_signature(),
    )
    .map_err(|_| ())?
    .l()
    .map_err(|_| ())
}
