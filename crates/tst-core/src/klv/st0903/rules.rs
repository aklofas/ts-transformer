//! ST 0903.6 rules on a whole record: a zero where an item's range starts
//! at one, and the items the standard requires together. Both strict
//! encoders and `decode_strict` (with its standalone twin) read them here,
//! so the encoders refuse exactly what strict decode refuses.

use crate::klv::st0903::model::VmtiLs;
use crate::klv::st0903::vtarget_pack::VTargetPack;

/// A rule a record breaks.
pub(super) struct Violation {
    /// The item the rule names.
    pub tag: u32,
    /// The pack it is in, by its index in `targets`; `None` at the top
    /// level.
    pub target: Option<usize>,
    pub rule: Rule,
}

pub(super) enum Rule {
    /// Zero in an item whose range is 1 to `max`.
    Zero { max: f64 },
    /// Items the standard requires together; the text names the clause.
    Relation(&'static str),
}

/// What strict decode says of a [`Rule::Zero`].
pub(super) const ZERO: &str = "zero is outside the item's range, which starts at one";

const V2_MAX: u64 = (1 << 16) - 1;
const V3_MAX: u64 = (1 << 24) - 1;
const V4_MAX: u64 = (1 << 32) - 1;
const V6_MAX: u64 = (1 << 48) - 1;

/// The first value or relation rule `ls` breaks: the single values first
/// (top level, then each pack), then the relations, pack by pack.
pub(super) fn first_violation(ls: &VmtiLs) -> Option<Violation> {
    let zero = |tag, target, max: u64| Violation {
        tag,
        target,
        rule: Rule::Zero { max: max as f64 },
    };
    // §10.1.4, §10.1.8, §10.1.9: version 1 to 65535; "do not use a value
    // of zero" for the frame size.
    for (tag, v, max) in [
        (4, ls.version_number.map(u64::from), V2_MAX),
        (8, ls.frame_width.map(u64::from), V3_MAX),
        (9, ls.frame_height.map(u64::from), V3_MAX),
    ] {
        if v == Some(0) {
            return Some(zero(tag, None, max));
        }
    }
    // §10.2.2.2 to .4 (pixel numbers), .20, .21 (row, column) start at 1.
    for (i, t) in ls.targets.iter().enumerate() {
        for (tag, v, max) in [
            (1, t.centroid_pixel, V6_MAX),
            (2, t.bbox_top_left_pixel, V6_MAX),
            (3, t.bbox_bottom_right_pixel, V6_MAX),
            (19, t.centroid_pix_row, V4_MAX),
            (20, t.centroid_pix_col, V4_MAX),
        ] {
            if v == Some(0) {
                return Some(zero(tag, Some(i), max));
            }
        }
    }
    for (i, t) in ls.targets.iter().enumerate() {
        let relation = |tag, reason| {
            Some(Violation {
                tag,
                target: Some(i),
                rule: Rule::Relation(reason),
            })
        };
        if ls.frame_width.is_none()
            && let Some(tag) = first_pixel_number(t)
        {
            return relation(tag, "a pixel number needs the frameWidth (ST 0903.6-121)");
        }
        match (t.centroid_pix_row.is_some(), t.centroid_pix_col.is_some()) {
            (true, false) => {
                return relation(19, "centroidPixRow needs centroidPixCol (ST 0903.4-38)");
            }
            (false, true) => {
                return relation(20, "centroidPixCol needs centroidPixRow (ST 0903.4-37)");
            }
            _ => {}
        }
        // Table 5: 1 is Active-Moving, 3 Active-Stopped.
        if matches!(t.detection_status, Some(1 | 3)) && !has_position(t) {
            return relation(
                23,
                "an active target needs a centroid, a row and column, or both bounding-box corners (ST 0903.6-127, -128)",
            );
        }
    }
    None
}

/// The rules of standalone carriage only, after [`first_violation`]: an
/// active or coasting target has a location (ST 0903.6-130), and the MIIS
/// Core Identifier is one ST 1204 accepts (§10.1.13, ST 0903.6-125).
pub(super) fn first_standalone_violation(ls: &VmtiLs) -> Option<Violation> {
    for (i, t) in ls.targets.iter().enumerate() {
        // Table 5: 1 Active-Moving, 3 Active-Stopped, 4 Active-Coasting. A
        // zero-length location is "unknown" (ST 0107.4-17), so none.
        if matches!(t.detection_status, Some(1 | 3 | 4))
            && t.target_location.as_ref().is_none_or(|l| l.is_empty())
        {
            return Some(Violation {
                tag: 23,
                target: Some(i),
                rule: Rule::Relation(
                    "a standalone active or coasting target needs a targetLocation (ST 0903.6-130)",
                ),
            });
        }
    }
    if let Some(id) = &ls.miis_id
        && crate::klv::st1204::decode(id).is_err()
    {
        return Some(Violation {
            tag: 13,
            target: None,
            rule: Rule::Relation("the MIIS Core Identifier is not one ST 1204 accepts"),
        });
    }
    None
}

fn first_pixel_number(t: &VTargetPack) -> Option<u32> {
    [
        (1, t.centroid_pixel),
        (2, t.bbox_top_left_pixel),
        (3, t.bbox_bottom_right_pixel),
    ]
    .into_iter()
    .find_map(|(tag, v)| v.map(|_| tag))
}

/// ST 0903.6-127 / -128: a centroid, a row and column, or both corners.
fn has_position(t: &VTargetPack) -> bool {
    t.centroid_pixel.is_some()
        || (t.centroid_pix_row.is_some() && t.centroid_pix_col.is_some())
        || (t.bbox_top_left_pixel.is_some() && t.bbox_bottom_right_pixel.is_some())
}
