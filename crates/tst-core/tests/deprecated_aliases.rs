//! The old names this crate still accepts compile and name the new types,
//! at the defining module path.

#![allow(deprecated)]

#[test]
fn stream_kind_keeps_its_old_name_at_the_mux_path() {
    fn old(_: Option<tst_core::mpegts::mux::StreamKind>) {}
    // The old name is the new type: the new spelling is accepted where the old is named.
    let k: Option<tst_core::mpegts::mux::MuxStreamKind> =
        Some(tst_core::mpegts::mux::MuxStreamKind::Video);
    old(k);
    let v: tst_core::mpegts::mux::StreamKind = tst_core::mpegts::mux::StreamKind::Klv;
    assert_eq!(v, tst_core::mpegts::mux::MuxStreamKind::Klv);
}
