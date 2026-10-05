#![no_main]
//! Fuzz target — AC-3 / AAC-LATM syncframe parsing and ADTS resync path.
//!
//! Gives `parse_syncframe` (AC-3) and `validate_latm_sync` (AAC-LATM)
//! libFuzzer coverage; the `audio_frame_iter` target only exercises the
//! strict ADTS iterator.
//!
//! # Input layout
//!
//! ```text
//! [0]      mode selector: byte % 3
//!            0 → AC-3   parse_syncframe(rest)       — panic-freedom
//!            1 → LATM   validate_latm_sync(rest)    — panic-freedom
//!            2 → ADTS   frames_with_resync(rest)    — resync scan path
//! [1..]    codec payload bytes
//! ```

use libfuzzer_sys::fuzz_target;
use tst_core::codec;

const MAX_FRAMES: usize = 1000;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let body = &data[1..];
    match data[0] % 3 {
        0 => {
            // AC-3: parse syncinfo + first-bsi fields. Returns Err on any
            // invalid sync word / reserved field / bad table value — no panic.
            let _ = codec::ac3::parse_syncframe(body);
        }
        1 => {
            // AAC-LATM: validate the LOAS sync word and length-fits check.
            // Returns Err(LatmFramingKind) on malformed input — no panic.
            let _ = codec::aac::latm::validate_latm_sync(body);
        }
        _ => {
            // ADTS frames_with_resync — exercises the forward-scan resync
            // path: on a parse error the iterator scans for the next
            // plausible 0xFFF syncword rather than terminating. This path
            // is absent from audio_frame_iter (which uses the strict
            // iterator). Capped at MAX_FRAMES to bound single-input runtime.
            let mut count = 0;
            for r in codec::aac::frames_with_resync(body) {
                let _ = r;
                count += 1;
                if count >= MAX_FRAMES {
                    break;
                }
            }
        }
    }
});
