//! `st0601::patch`: edits that replay a sentinel or an IMAP special, a new
//! time stamp written first, the bytes `patch` and `encode_to_vec` give one
//! item, and the cost of many distinct tags.

use std::time::{Duration, Instant};

use tst_core::klv::ImapbSpecial;
use tst_core::klv::checksum::checksum_running_sum_16;
use tst_core::klv::st0601::{
    UasDatalinkLs, decode, decode_strict_compliance, decode_unchecked, encode_to_vec, patch,
};

const UL: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
];

/// `(tag, value)` of each top-level item of an ST 0601 set (one-byte tags).
fn items(set: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let (body_len, mut p) = if set[16] < 0x80 {
        (set[16] as usize, 17)
    } else {
        let n = (set[16] & 0x7F) as usize;
        let len = set[17..17 + n]
            .iter()
            .fold(0usize, |a, &b| (a << 8) | b as usize);
        (len, 17 + n)
    };
    let end = p + body_len;
    let mut out = Vec::new();
    while p < end {
        let tag = set[p];
        assert!(tag < 0x80, "a test set uses one-byte tags");
        let len = set[p + 1] as usize;
        assert!(len < 0x80, "a test set uses short lengths");
        out.push((tag, set[p + 2..p + 2 + len].to_vec()));
        p += 2 + len;
    }
    out
}

fn tags(set: &[u8]) -> Vec<u8> {
    items(set).into_iter().map(|(t, _)| t).collect()
}

/// A set of the given `(tag, value)` items; a final checksum when `checksum`.
fn set_of(body_items: &[(u8, &[u8])], checksum: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for (tag, value) in body_items {
        body.push(*tag);
        body.push(value.len() as u8);
        body.extend_from_slice(value);
    }
    if checksum {
        body.extend_from_slice(&[0x01, 0x02, 0x00, 0x00]);
    }
    let mut out = UL.to_vec();
    assert!(body.len() < 0x80);
    out.push(body.len() as u8);
    out.extend_from_slice(&body);
    if checksum {
        let n = out.len();
        let sum = checksum_running_sum_16(&out[..n - 2]);
        out[n - 2] = (sum >> 8) as u8;
        out[n - 1] = sum as u8;
    }
    out
}

const MISSION: (u8, &[u8]) = (3, b"M1");
const LAT: (u8, &[u8]) = (13, &[0x55, 0x95, 0xB6, 0x6D]);
const VERSION: (u8, &[u8]) = (65, &[19]);

fn timestamp_edit() -> UasDatalinkLs {
    UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        ..UasDatalinkLs::default()
    }
}

#[test]
fn patch_applies_sentinel_edit() {
    let raw = encode_to_vec(&UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        frame_center_lat_deg: Some(10.0),
        ..UasDatalinkLs::default()
    })
    .unwrap();
    let edits = UasDatalinkLs {
        sentinel_tags: vec![23],
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    assert_ne!(out, raw);
    let back = decode(&out).unwrap();
    assert_eq!(back.frame_center_lat_deg, None);
    assert_eq!(back.sentinel_tags, vec![23]);
    // The item is rewritten in place as the row's INT_MIN sentinel.
    let at = tags(&raw).iter().position(|&t| t == 23).unwrap();
    assert_eq!(items(&out)[at], (23, vec![0x80, 0, 0, 0]));
}

#[test]
fn patch_inserts_absent_special_edit() {
    let raw = set_of(&[MISSION, VERSION], true);
    let edits = UasDatalinkLs {
        imapb_specials: vec![(104, ImapbSpecial::AboveMax)],
        ..UasDatalinkLs::default()
    };
    let out = patch(&raw, &edits).unwrap();
    assert_eq!(tags(&out), [3, 65, 104, 1]);
    assert_eq!(items(&out)[2], (104, vec![0xE1, 0, 0]));
    let back = decode(&out).unwrap();
    assert_eq!(back.imapb_specials, vec![(104, ImapbSpecial::AboveMax)]);
}

#[test]
fn patch_inserts_absent_timestamp_first() {
    let raw = set_of(&[MISSION, LAT, VERSION], true);
    let out = patch(&raw, &timestamp_edit()).unwrap();
    assert_eq!(tags(&out), [2, 3, 13, 65, 1]);
    let _strict = decode_strict_compliance(&out).unwrap();

    let raw = set_of(&[VERSION], false);
    assert_eq!(tags(&patch(&raw, &timestamp_edit()).unwrap()), [2, 65]);

    let raw = set_of(&[MISSION, LAT, VERSION], true);
    let edits = UasDatalinkLs {
        frame_center_lat_deg: Some(10.0),
        ..timestamp_edit()
    };
    assert_eq!(tags(&patch(&raw, &edits).unwrap()), [2, 3, 13, 65, 23, 1]);
}

#[test]
fn patch_keeps_an_existing_item_2_where_it_is() {
    let old_time: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 1];
    let raw = set_of(&[MISSION, (2, old_time), VERSION], true);
    let out = patch(&raw, &timestamp_edit()).unwrap();
    assert_eq!(tags(&out), [3, 2, 65, 1]);
    assert_eq!(
        items(&out)[1].1,
        1_700_000_000_000_000u64.to_be_bytes().to_vec()
    );
}

#[test]
fn patch_with_a_mid_body_checksum_and_no_item_2() {
    // A non-conformant set whose checksum is not its last item: the new
    // time stamp goes first, and the checksum (copied where it was) covers
    // the bytes before it in the new set.
    let mut body = vec![MISSION.0, MISSION.1.len() as u8];
    body.extend_from_slice(MISSION.1);
    body.extend_from_slice(&[0x01, 0x02, 0xAA, 0xBB]);
    body.extend_from_slice(&[65, 1, 19]);
    let mut raw = UL.to_vec();
    raw.push(body.len() as u8);
    raw.extend_from_slice(&body);

    let out = patch(&raw, &timestamp_edit()).unwrap();
    assert_eq!(tags(&out), [2, 3, 1, 65]);
    let at = out
        .windows(2)
        .position(|w| w == [0x01, 0x02])
        .expect("the checksum item")
        + 2;
    let sum = checksum_running_sum_16(&out[..at]);
    assert_eq!(&out[at..at + 2], &sum.to_be_bytes());
    let _read = decode_unchecked(&out).unwrap();
}

/// Time of the fastest of three runs.
fn best_of_three(f: impl Fn()) -> Duration {
    (0..3)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

/// A set of `n` distinct unknown items: zero-length, three-byte tags.
fn many_unknown_tags(n: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(n as usize * 4);
    for i in 0..n {
        let tag = 0x4000 + i;
        body.extend_from_slice(&[
            0x80 | ((tag >> 14) & 0x7F) as u8,
            0x80 | ((tag >> 7) & 0x7F) as u8,
            (tag & 0x7F) as u8,
            0,
        ]);
    }
    let mut out = UL.to_vec();
    let len = body.len();
    out.extend_from_slice(&[0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]);
    out.extend_from_slice(&body);
    out
}

#[test]
fn patch_many_distinct_unknown_tags_is_linear() {
    // Four times the tags takes about four times as long when the walk is
    // linear; a walk that compares each tag with every tag seen so far
    // takes about sixteen times as long.
    let small = many_unknown_tags(1 << 15);
    let large = many_unknown_tags(1 << 17);
    let edits = UasDatalinkLs::default();
    let t_small = best_of_three(|| {
        patch(&small, &edits).unwrap();
    });
    let t_large = best_of_three(|| {
        patch(&large, &edits).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64();
    assert!(
        ratio < 8.0,
        "2^17 tags took {t_large:?}, 2^15 took {t_small:?}: ratio {ratio:.1}"
    );
}

/// The value bytes of `tag` in `set`, if the set carries it once.
fn item_value(set: &[u8], tag: u8) -> Option<Vec<u8>> {
    let mut found: Vec<Vec<u8>> = items(set)
        .into_iter()
        .filter(|(t, _)| *t == tag)
        .map(|(_, v)| v)
        .collect();
    assert!(found.len() <= 1, "tag {tag} written {} times", found.len());
    found.pop()
}

/// One typed field of a record.
type FieldOf = fn(&mut UasDatalinkLs) -> &mut Option<f64>;

#[test]
fn patch_and_encode_agree_for_every_replay_class() {
    // For each way a record can hold an item (a value, a sentinel entry, an
    // IMAP special entry, both, two entries, an entry the encoder ignores),
    // `patch` with the record as the edit writes the item as
    // `encode_to_vec` writes it, and `encoded_len` is the written length.
    let specials = [
        ImapbSpecial::BelowMin,
        ImapbSpecial::AboveMax,
        ImapbSpecial::PositiveInfinity,
        ImapbSpecial::NegativeInfinity,
        ImapbSpecial::PositiveQuietNaN { nan_id: 5 },
        ImapbSpecial::NegativeQuietNaN { nan_id: 0 },
        ImapbSpecial::PositiveSignalingNaN { signal: 1 },
        ImapbSpecial::NegativeSignalingNaN { signal: 2 },
        ImapbSpecial::UserDefined { signal: 3 },
        ImapbSpecial::UserDefined { signal: 1 << 15 },
        ImapbSpecial::UserDefined { signal: 1 << 40 },
    ];
    let imapb_rows: [(u8, FieldOf, f64, f64); 3] = [
        (
            104,
            |r| &mut r.sensor_ellipsoid_height_extended_m,
            -900.0,
            40_000.0,
        ),
        (113, |r| &mut r.altitude_agl_m, -900.0, 40_000.0),
        (117, |r| &mut r.sensor_azimuth_rate_dps, -1000.0, 1000.0),
    ];
    let mut cases: Vec<(String, u8, UasDatalinkLs)> = Vec::new();
    for (tag, field, min, max) in imapb_rows {
        let with = |v: Option<f64>, entries: Vec<(u32, ImapbSpecial)>| {
            let mut r = UasDatalinkLs::default();
            *field(&mut r) = v;
            r.imapb_specials = entries;
            r
        };
        let t = u32::from(tag);
        cases.push((
            format!("{tag} value only"),
            tag,
            with(Some(min + 1.0), vec![]),
        ));
        for s in specials {
            cases.push((format!("{tag} {s:?} only"), tag, with(None, vec![(t, s)])));
        }
        cases.push((
            format!("{tag} at its minimum with BelowMin"),
            tag,
            with(Some(min), vec![(t, ImapbSpecial::BelowMin)]),
        ));
        cases.push((
            format!("{tag} at its maximum with AboveMax"),
            tag,
            with(Some(max), vec![(t, ImapbSpecial::AboveMax)]),
        ));
        cases.push((
            format!("{tag} one step inside its minimum with BelowMin"),
            tag,
            with(Some(min + 1.0), vec![(t, ImapbSpecial::BelowMin)]),
        ));
        cases.push((
            format!("{tag} two entries"),
            tag,
            with(
                None,
                vec![(t, ImapbSpecial::BelowMin), (t, ImapbSpecial::AboveMax)],
            ),
        ));
    }
    let signed = |v: Option<f64>, sentinels: Vec<u32>| UasDatalinkLs {
        platform_pitch_full_deg: v,
        sentinel_tags: sentinels,
        ..UasDatalinkLs::default()
    };
    cases.push(("90 value only".into(), 90, signed(Some(4.5), vec![])));
    cases.push(("90 sentinel only".into(), 90, signed(None, vec![90])));
    cases.push((
        "90 value with a redundant sentinel".into(),
        90,
        signed(Some(4.5), vec![90]),
    ));
    cases.push((
        "an IMAPB entry on a row that is not IMAPB".into(),
        75,
        UasDatalinkLs {
            imapb_specials: vec![(75, ImapbSpecial::BelowMin)],
            ..UasDatalinkLs::default()
        },
    ));
    cases.push((
        "a sentinel on an unknown tag and on a pack tag".into(),
        48,
        UasDatalinkLs {
            sentinel_tags: vec![66, 48],
            imapb_specials: vec![(48, ImapbSpecial::AboveMax)],
            ..UasDatalinkLs::default()
        },
    ));

    let input = set_of(&[MISSION, VERSION], true);
    for (what, tag, edit) in cases {
        // A special no length of the item holds is refused by both.
        let encoded = match encode_to_vec(&edit) {
            Ok(encoded) => encoded,
            Err(e) => {
                assert!(
                    patch(&input, &edit).is_err(),
                    "{what}: encode refused ({e}), patch did not"
                );
                continue;
            }
        };
        assert_eq!(
            tst_core::klv::st0601::encoded_len(&edit),
            encoded.len(),
            "{what}: encoded_len"
        );
        let patched = patch(&input, &edit).unwrap();
        assert_eq!(
            item_value(&patched, tag),
            item_value(&encoded, tag),
            "{what}: the item's bytes"
        );
        let (p, e) = (decode(&patched).unwrap(), decode(&encoded).unwrap());
        let entries = |r: &UasDatalinkLs| {
            (
                r.sentinel_tags.contains(&u32::from(tag)),
                r.imapb_specials
                    .iter()
                    .filter(|(t, _)| *t == u32::from(tag))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(entries(&p), entries(&e), "{what}: the decoded entries");
    }
}
