#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_core::klv::st0601::{UasDatalinkLs, decode, decode_unchecked, patch};

fuzz_target!(|data: &[u8]| {
    // Identity property: an input that decodes cleanly patched with NO
    // edits must round-trip byte-identically (verbatim TLV copy,
    // preserved outer-length bytes + trailing bytes, checksum recompute
    // reproducing the already-valid value).
    if decode(data).is_ok() {
        let out = patch(data, &UasDatalinkLs::default())
            .expect("patch must succeed on a decodable input");
        assert_eq!(out.as_slice(), data);
    }

    // Edit property: a successful patch must stay decodable and carry
    // the edit; patch must never panic on arbitrary input.
    let edits = UasDatalinkLs {
        frame_center_lat_deg: Some(12.5),
        ..UasDatalinkLs::default()
    };
    if let Ok(out) = patch(data, &edits) {
        let re = decode_unchecked(&out).expect("patched output must decode");

        // Diagnostic: tag 23 was re-encoded by our own encoder from a
        // valid value, so the patched decode must carry no field error
        // for it. Checked before the presence assert below so a failure
        // prints the underlying field errors instead of a bare "must be
        // present". The filter relies on the `KlvFieldError` "tag {tag}:"
        // message prefix (the enum exposes no tag() accessor); other
        // tags' field errors are allowed — patch preserves malformed
        // non-edited TLVs verbatim.
        let tag23_errors: Vec<String> = re
            .field_errors
            .iter()
            .map(|e| e.to_string())
            .filter(|m| m.starts_with("tag 23:"))
            .collect();
        assert!(
            tag23_errors.is_empty(),
            "patched tag 23 must decode cleanly: {tag23_errors:?}"
        );

        let lat = re.frame_center_lat_deg.expect("edited tag must be present");
        assert!((lat - 12.5).abs() < 1e-4);

        // Other-fields-survive property: every typed field except the
        // edited one must match between the original and patched
        // decodes. Two exclusions are required for soundness:
        //  - `frame_center_lat_deg` is the edit itself;
        //  - `field_errors` may legitimately differ: patch re-encoding
        //    a MALFORMED original tag 23 heals its field error, so
        //    comparing the sets would fire on CORRECT behavior.
        // `field_errors` is cleared on BOTH sides rather than copied
        // across: an `OutOfRange` entry carries the offending value, and
        // the decoder reports a reserved IMAPB special and a non-finite
        // IMAPB decode as `value: NaN`, so a copied list never compares
        // equal to itself. The typed fields themselves are NaN-free
        // (integer scaling / IMAPB `Value` decode, always finite).
        if let Ok(mut orig) = decode_unchecked(data) {
            let mut masked = re.clone();
            masked.frame_center_lat_deg = orig.frame_center_lat_deg;
            masked.field_errors.clear();
            orig.field_errors.clear();
            assert_eq!(masked, orig, "non-edited fields must survive patch");
        }
    }
});
