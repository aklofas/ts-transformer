//! Stream-handle pack/unpack and accessor tests.
//!
//! Covers `AudioStreamHandle`, `SubtitleStreamHandle`, `VideoStreamHandle`,
//! `KlvStreamHandle` round-trips, `from_raw` identity, trait impls, and the
//! single-program `video_stream_handle` index helper.

use super::*;

#[test]
fn audio_stream_handle_pack_unpack_round_trip() {
    let h = AudioStreamHandle::pack(2, 5);
    assert_eq!(h.unpack(), (2, 5));
}

#[test]
fn audio_stream_handle_from_raw() {
    let h = AudioStreamHandle::pack(3, 7);
    let raw: u32 = h.raw();
    let h2 = AudioStreamHandle::from_raw(raw);
    assert_eq!(h, h2);
}

#[test]
fn subtitle_stream_handle_pack_unpack_round_trip() {
    let h = SubtitleStreamHandle::pack(2, 5);
    assert_eq!(h.unpack(), (2, 5));
}

#[test]
fn subtitle_stream_handle_from_raw() {
    let h = SubtitleStreamHandle::pack(3, 7);
    let raw: u32 = h.raw();
    let h2 = SubtitleStreamHandle::from_raw(raw);
    assert_eq!(h, h2);
}

#[test]
fn handle_types_are_copy_eq_hash() {
    // Compile-time assertion: handles must be Copy + Eq + Hash so
    // consumers can stash them in HashMaps / HashSets and pass them
    // around freely.
    fn assert_copy<T: Copy>() {}
    fn assert_eq_hash<T: Eq + std::hash::Hash>() {}
    assert_copy::<VideoStreamHandle>();
    assert_copy::<KlvStreamHandle>();
    assert_eq_hash::<VideoStreamHandle>();
    assert_eq_hash::<KlvStreamHandle>();
}

#[test]
fn handle_debug_includes_kind_and_index() {
    let v = VideoStreamHandle::pack(0, 2);
    let k = KlvStreamHandle::pack(0, 0);
    // Don't lock the exact format, just sanity-check it carries both bits.
    assert!(format!("{v:?}").contains("Video"));
    assert!(format!("{v:?}").contains('2'));
    assert!(format!("{k:?}").contains("Klv"));
    assert!(format!("{k:?}").contains('0'));
}

#[test]
fn handles_single_stream_returns_one_each() {
    let cfg = MuxerConfig::default();
    let mux = Muxer::new(cfg).unwrap();
    let vs = mux.video_handles();
    let ks = mux.klv_handles();
    assert_eq!(vs.len(), 1);
    assert_eq!(ks.len(), 1);
    assert_eq!(mux.video_stream_handle(0), Some(vs[0]));
    assert_eq!(mux.klv_stream_handle(0), Some(ks[0]));
}

#[test]
fn handles_out_of_range_returns_none() {
    let mux = Muxer::new(MuxerConfig::default()).unwrap();
    assert_eq!(mux.video_stream_handle(1), None);
    assert_eq!(mux.klv_stream_handle(1), None);
}
