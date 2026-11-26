#![no_main]
use libfuzzer_sys::fuzz_target;
use tst_core::klv::st1204;

fuzz_target!(|data: &[u8]| {
    // What `decode` returns, `validate` accepts, and the checked encode
    // writes bytes that decode to the same identifier.
    if let Ok(id) = st1204::decode(data) {
        assert_eq!(
            id.validate(),
            Ok(()),
            "decode returned an identifier validate refuses"
        );
        let bytes =
            st1204::encode_checked(&id).expect("encode_checked refused a decoded identifier");
        assert_eq!(st1204::decode(&bytes).as_ref(), Ok(&id));
    }
});
