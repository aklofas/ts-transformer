//! Shared JNI marshalling helpers used across the klv (and potentially other)
//! binding modules. Extracted from `mpegts/mod.rs` where `wrap_heap_byte_buffer`
//! originated; the klv module re-uses it for KLV byte payloads.

use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::jlong;
use tst_core::error::KlvFieldError as RustKlvFieldError;
use tst_core::klv::OwnedRawField;
use tst_core::transport::SocketStats;

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
/// memory, safe to retain past the next call / after `close()`. Used by klv
/// Tasks 1–4.
pub fn wrap_heap_byte_buffer<'local>(
    env: &mut JNIEnv<'local>,
    bytes: &[u8],
) -> Result<JObject<'local>, ()> {
    let arr = env.byte_array_from_slice(bytes).map_err(|_| ())?;
    env.call_static_method(
        "java/nio/ByteBuffer",
        "wrap",
        "([B)Ljava/nio/ByteBuffer;",
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
/// `RustKlvFieldError`s. Used by klv Tasks 2–4.
///
/// An ST 0601 record's `field_errors` can exceed the default 16-slot JNI
/// local-ref table, so each iteration's element refs (the kind constant, the
/// message string, the record) are minted inside a per-entry
/// `with_local_frame` and reclaimed at the end of the iteration — only the
/// long-lived `list` ref (created in the outer frame) survives. Mirrors the
/// forward-note on `build_pid_list` in `mpegts/mod.rs`.
pub fn build_field_errors<'local>(
    env: &mut JNIEnv<'local>,
    errs: &[RustKlvFieldError],
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for fe in errs {
        // 8 slots: comfortably covers the 3 element refs + JNI scratch per entry.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let (kind_name, tag) = field_error_kind_and_tag(fe);
            let kind_obj = env
                .get_static_field(
                    "org/tstrans/klv/KlvFieldErrorKind",
                    kind_name,
                    "Lorg/tstrans/klv/KlvFieldErrorKind;",
                )?
                .l()?;
            let msg = env.new_string(fe.to_string())?;
            let obj = env.new_object(
                "org/tstrans/klv/KlvFieldError",
                "(Lorg/tstrans/klv/KlvFieldErrorKind;JLjava/lang/String;)V",
                &[
                    JValue::Object(&kind_obj),
                    JValue::Long(i64::from(tag)),
                    JValue::Object(&msg),
                ],
            )?;
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&obj)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

/// Build a `java.util.List<KlvUnknownField>` (an `ArrayList`) from a slice of
/// [`OwnedRawField`]s. Each entry is a heap `ByteBuffer` copy. Used by klv
/// Tasks 2–4 (call sites pass `&record.unknown`).
///
/// As with `build_field_errors`, an ST 0601 record's `unknown` list can exceed
/// the default 16-slot JNI local-ref table, so each iteration's element refs
/// (the byte array + ByteBuffer + record) are minted inside a per-entry
/// `with_local_frame` and reclaimed at the end of the iteration — only the
/// long-lived `list` ref survives.
pub fn build_unknown_list<'local>(
    env: &mut JNIEnv<'local>,
    fields: &[OwnedRawField],
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for f in fields {
        // 8 slots: covers the byte[] + ByteBuffer + record refs + JNI scratch.
        env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            let arr = env.byte_array_from_slice(&f.value)?;
            let buf = env
                .call_static_method(
                    "java/nio/ByteBuffer",
                    "wrap",
                    "([B)Ljava/nio/ByteBuffer;",
                    &[JValue::Object(&arr)],
                )?
                .l()?;
            let obj = env.new_object(
                "org/tstrans/klv/KlvUnknownField",
                "(JLjava/nio/ByteBuffer;)V",
                &[JValue::Long(i64::from(f.tag)), JValue::Object(&buf)],
            )?;
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
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
    env: &mut JNIEnv<'local>,
    values: impl Iterator<Item = i64>,
) -> jni::errors::Result<JObject<'local>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[])?;
    for v in values {
        // Box the long into java.lang.Long, then add to the list.
        env.with_local_frame(4, |env| -> jni::errors::Result<()> {
            let boxed = env.new_object("java/lang/Long", "(J)V", &[JValue::Long(v)])?;
            env.call_method(
                &list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[JValue::Object(&boxed)],
            )?;
            Ok(())
        })?;
    }
    Ok(list)
}

/// Read a `java.util.List<Long>` into a `Vec<i64>`. Sibling of
/// [`build_long_list`] for the read (encode) direction — used by klv WP-C
/// (`controlCommandVerification`/`activeWavelengths`/`SdccFlpField.precedingTags`).
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
/// width (`u32`/`u64`, via `checked_u32`/`checked_u64`) themselves.
pub fn read_long_list(env: &mut JNIEnv, list: &JObject) -> jni::errors::Result<Vec<i64>> {
    let size = env.call_method(list, "size", "()I", &[])?.i()?;
    let mut out = Vec::with_capacity(size.max(0) as usize);
    for i in 0..size {
        let v = env.with_local_frame(4, |inner_env| -> jni::errors::Result<i64> {
            let item = inner_env
                .call_method(list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                .l()?;
            inner_env.call_method(&item, "longValue", "()J", &[])?.j()
        })?;
        out.push(v);
    }
    Ok(out)
}

/// Read a `java.util.List<KlvUnknownField>` back into a `Vec<OwnedRawField>`,
/// dropping any entry whose tag collides with a typed tag (typed wins). Used by klv
/// Tasks 2–4 (call sites assign the result back to `record.unknown`).
pub fn read_unknown_list(
    env: &mut JNIEnv,
    list: &JObject,
    is_typed: impl Fn(u32) -> bool,
) -> jni::errors::Result<Vec<OwnedRawField>> {
    let size = env.call_method(list, "size", "()I", &[])?.i()?;
    let mut out = Vec::new();
    for i in 0..size {
        // Per-item local frame, like every other list reader in this module:
        // each entry mints ~5 refs (List.get, the ByteBuffer, its duplicate,
        // the byte[] behind read_byte_buffer) and nothing below returns a
        // ref, so the frame can reclaim them all before the next entry.
        let field =
            env.with_local_frame(8, |env| -> jni::errors::Result<Option<OwnedRawField>> {
                let item = env
                    .call_method(list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?
                    .l()?;
                let tag_long = env.call_method(&item, "tag", "()J", &[])?.j()?;
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
                    .call_method(&item, "value", "()Ljava/nio/ByteBuffer;", &[])?
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
pub fn read_byte_buffer(env: &mut JNIEnv, buf: &JObject) -> jni::errors::Result<Vec<u8>> {
    // duplicate() gives us an independent position/limit view on the same data,
    // so we can call get([B) without advancing the caller's position.
    let dup = env
        .call_method(buf, "duplicate", "()Ljava/nio/ByteBuffer;", &[])?
        .l()?;
    let remaining = env.call_method(&dup, "remaining", "()I", &[])?.i()?;
    let arr = env.new_byte_array(remaining)?;
    env.call_method(
        &dup,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
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
pub fn require_non_null(env: &mut JNIEnv, obj: &JObject, what: &str) -> jni::errors::Result<()> {
    if obj.is_null() {
        let _ = env.throw_new(
            "java/lang/NullPointerException",
            format!("{what} must not be null"),
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
pub fn checked_u8(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<u8> {
    if !(0..=255).contains(&value) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be 0..=255, got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u8)
}

/// Range-check a Java `int`/`long` value against the u16 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_u16(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<u16> {
    if !(0..=65535).contains(&value) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be 0..=65535, got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u16)
}

/// Range-check a Java `long` value against the u32 range, then narrow.
/// Throws `IllegalArgumentException` and returns `Err(JavaException)` on overflow.
pub fn checked_u32(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<u32> {
    if !(0..=u32::MAX as i64).contains(&value) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be 0..=4294967295, got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u32)
}

/// Convert a Java `long` (carrying an unsigned ST 0903 u64 value) to `u64`,
/// rejecting only a negative `i64` (which is a caller bug — Java has no
/// unsigned primitive, so large values arrive as a bit-pattern `long`).
pub fn checked_u64(env: &mut JNIEnv, value: i64, field: &str) -> jni::errors::Result<u64> {
    if value < 0 {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            format!("{field} must be >= 0 (unsigned long), got {value}"),
        );
        return Err(jni::errors::Error::JavaException);
    }
    Ok(value as u64)
}

// -----------------------------------------------------------------------
// Shared nullable accessor helpers (encode path)
// -----------------------------------------------------------------------

/// Read a nullable `Integer` accessor. Returns `None` for null.
pub fn read_nullable_int(
    env: &mut JNIEnv,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<i32>> {
    let obj = env
        .call_method(rec, name, "()Ljava/lang/Integer;", &[])?
        .l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let v = env.call_method(&obj, "intValue", "()I", &[])?.i()?;
    Ok(Some(v))
}

/// Read a nullable `Long` accessor. Returns `None` for null.
pub fn read_nullable_long(
    env: &mut JNIEnv,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<i64>> {
    let obj = env.call_method(rec, name, "()Ljava/lang/Long;", &[])?.l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let v = env.call_method(&obj, "longValue", "()J", &[])?.j()?;
    Ok(Some(v))
}

/// Read a nullable `Double` accessor. Returns `None` for null.
pub fn read_nullable_double(
    env: &mut JNIEnv,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<f64>> {
    let obj = env
        .call_method(rec, name, "()Ljava/lang/Double;", &[])?
        .l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let v = env.call_method(&obj, "doubleValue", "()D", &[])?.d()?;
    Ok(Some(v))
}

/// Read a nullable `String` accessor. Returns `None` for null.
pub fn read_nullable_string(
    env: &mut JNIEnv,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<String>> {
    let obj = env
        .call_method(rec, name, "()Ljava/lang/String;", &[])?
        .l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let j_str: &jni::objects::JString = (&obj).into();
    let s: String = env.get_string(j_str).map(Into::into)?;
    Ok(Some(s))
}

/// Read a nullable `ByteBuffer` accessor using `read_byte_buffer` (honours
/// position/limit; works for heap and direct buffers). Returns `None` for null.
pub fn read_nullable_byte_buffer(
    env: &mut JNIEnv,
    rec: &JObject,
    name: &str,
) -> jni::errors::Result<Option<Vec<u8>>> {
    let obj = env
        .call_method(rec, name, "()Ljava/nio/ByteBuffer;", &[])?
        .l()?;
    if obj.is_null() {
        return Ok(None);
    }
    let bytes = read_byte_buffer(env, &obj)?;
    Ok(Some(bytes))
}

/// Join `host:port`, bracketing bare IPv6 literals so `Socket::connect_with` /
/// `Listener::bind_with` accept them.
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Read a Java `byte[]` argument, or throw `RuntimeException` + return `None`.
pub fn read_bytes(env: &mut JNIEnv, arr: &JByteArray) -> Option<Vec<u8>> {
    match env.convert_byte_array(arr) {
        Ok(b) => Some(b),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            None
        }
    }
}

/// Fetch an `org.tstrans.<package>.<class>.<name>` enum constant via
/// `get_static_field`.
pub fn enum_const<'local>(
    env: &mut JNIEnv<'local>,
    package: &str,
    class: &str,
    name: &str,
) -> Result<JObject<'local>, ()> {
    let class_path = format!("org/tstrans/{package}/{class}");
    let descriptor = format!("Lorg/tstrans/{package}/{class};");
    env.get_static_field(&class_path, name, &descriptor)
        .map_err(|_| ())?
        .l()
        .map_err(|_| ())
}

/// Build a `SocketStats` Java record at `class_path` (e.g.
/// `org/tstrans/srt/SocketStats` or `org/tstrans/rtp/SocketStats`) from the
/// Rust `tst_core::transport::SocketStats`. Field order matches every
/// `SocketStats` record ctor exactly (16 longs, in declaration order): rttUs,
/// sendBandwidthBps, recvBandwidthBps, linkBandwidthBps, bytesSent,
/// packetsSent, bytesReceived, packetsReceived, bytesLostRecv,
/// packetsLostRecv, packetsLostSend, packetsRetransmitted,
/// packetsDroppedSend, packetsDroppedRecv, sendBufferPackets,
/// recvBufferPackets. All counters widen to `i64` (Java has no unsigned
/// types — the bit pattern is reinterpreted; documented on the Java records).
pub fn build_socket_stats<'local>(
    env: &mut JNIEnv<'local>,
    class_path: &str,
    s: &SocketStats,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    let sig = "(JJJJJJJJJJJJJJJJ)V"; // 16 longs
    env.new_object(
        class_path,
        sig,
        &[
            JValue::Long(i64::from(s.rtt_us)),
            JValue::Long(s.send_bandwidth_bps as i64),
            JValue::Long(s.recv_bandwidth_bps as i64),
            JValue::Long(s.link_bandwidth_bps as i64),
            JValue::Long(s.bytes_sent as i64),
            JValue::Long(s.packets_sent as i64),
            JValue::Long(s.bytes_received as i64),
            JValue::Long(s.packets_received as i64),
            JValue::Long(s.bytes_lost_recv as i64),
            JValue::Long(s.packets_lost_recv as i64),
            JValue::Long(s.packets_lost_send as i64),
            JValue::Long(s.packets_retransmitted as i64),
            JValue::Long(s.packets_dropped_send as i64),
            JValue::Long(s.packets_dropped_recv as i64),
            JValue::Long(i64::from(s.send_buffer_packets)),
            JValue::Long(i64::from(s.recv_buffer_packets)),
        ],
    )
}
