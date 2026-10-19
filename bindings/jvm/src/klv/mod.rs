//! JNI surface for `org.tstrans.klv` — typed KLV decode/encode for
//! ST 0601 / 0102 / 0605 / 0903. Per-set entry points live in submodules;
//! this module houses the test-only forced-throw
//! helpers that let `KlvErrorModelTest` exercise the error-mapping wiring
//! before the real decode/encode entry points exist.

pub mod st0102;
pub mod st0601;
pub mod st0605;
pub mod st0805;
pub mod st0806;
pub mod st0903;
pub mod st1204;

use crate::panic::jni_native;
use jni::objects::JString;

jni_native! {
    /// Test-only: `org.tstrans.klv.Klv.nRaiseDecodeForTest(kind)`.
    /// Throws a `KlvDecodeException` with the given Kind name, allowing
    /// `KlvErrorModelTest` to verify the full error-mapping path.
    fn Java_org_tstrans_klv_Klv_nRaiseDecodeForTest<'local>(
        env,
        _class,
        kind: JString<'local>,
    ) = () => {
        let k: String = kind.try_to_string(env).unwrap_or_default();
        crate::error::throw_klv_decode_raw(env, &k, "forced decode error for test");
    }
}

jni_native! {
    /// Test-only: `org.tstrans.klv.Klv.nRaiseEncodeForTest(kind)`.
    /// Throws a `KlvEncodeException` with the given Kind name (no tag), allowing
    /// `KlvErrorModelTest` to verify the full error-mapping path.
    fn Java_org_tstrans_klv_Klv_nRaiseEncodeForTest<'local>(
        env,
        _class,
        kind: JString<'local>,
    ) = () => {
        let k: String = kind.try_to_string(env).unwrap_or_default();
        crate::error::throw_klv_encode_raw(env, &k, None, "forced encode error for test");
    }
}
