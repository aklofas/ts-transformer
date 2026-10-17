//! Hand-written converters for fields a generated record marks `@[custom]`.
//!
//! A custom field `<field>` of record `<Record>` is converted by a function in
//! this module named for the record and the field, in snake case. The
//! generator calls it and never writes it. See "Adding a crossing record" in
//! `docs/reference/binding-authors.md`.

use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};

use tst_core::error::KlvFieldError;
use tst_core::klv::ImapbSpecial;
use tst_core::klv::UniversalLabel;
use tst_core::klv::pack::OwnedRawField;
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use tst_core::klv::st0601::{
    IcingDetected, OperationalMode, PayloadType, PlatformStatus, SensorControlMode, SensorFovName,
};
use tst_core::klv::st0605::TimeStatus;
use tst_core::klv::st0806::{RvtAoiType, RvtPoiType};
use tst_core::klv::st1204::IdType;

use crate::klv::{
    convert_field_errors, convert_unknown, enum_field_to_u8, enum_field_to_u64, py_to_unknown,
};

// ---------------------------------------------------------------------------
// ST 0601 — UasDatalinkLs
// ---------------------------------------------------------------------------

pub(crate) fn uas_datalink_ls_universal_label_to_py(
    py: Python<'_>,
    v: &UniversalLabel,
) -> PyResult<PyObject> {
    Ok(PyBytes::new_bound(py, &v.0).into_py(py))
}

/// Any length other than 16 is a caller bug: it raises instead of silently
/// leaving the field at the default label.
pub(crate) fn uas_datalink_ls_universal_label_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<UniversalLabel> {
    let bytes: Vec<u8> = obj
        .getattr(intern!(obj.py(), "universal_label"))?
        .extract()?;
    let ul: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
        PyValueError::new_err(format!(
            "universal_label must be 16 bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(UniversalLabel(ul))
}

/// A byte string crosses as Python `bytes`, not as a list of ints.
pub(crate) fn uas_datalink_ls_miis_core_id_to_py(
    py: Python<'_>,
    v: &Option<Vec<u8>>,
) -> PyResult<PyObject> {
    Ok(match v {
        Some(b) => PyBytes::new_bound(py, b).into_py(py),
        None => py.None(),
    })
}

pub(crate) fn uas_datalink_ls_miis_core_id_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<Vec<u8>>> {
    obj.getattr(intern!(obj.py(), "miis_core_id"))?.extract()
}

/// A coded enum: the `tstrans.klv` enum member for a known code point, or the
/// raw `int` for a wire-unknown `Other(code)`.
fn coded_enum(
    py: Python<'_>,
    class: &'static str,
    other: Option<u64>,
    code: u64,
) -> PyResult<PyObject> {
    if let Some(raw) = other {
        return Ok(raw.into_py(py));
    }
    let cls = py.import_bound("tstrans.klv")?.getattr(class)?;
    Ok(cls.call1((code,))?.unbind())
}

pub(crate) fn uas_datalink_ls_icing_detected_to_py(
    py: Python<'_>,
    v: &Option<IcingDetected>,
) -> PyResult<PyObject> {
    let Some(v) = v else { return Ok(py.None()) };
    let other = match v {
        IcingDetected::Other(b) => Some(u64::from(*b)),
        _ => None,
    };
    coded_enum(py, "IcingDetected", other, u64::from(v.to_wire()))
}

pub(crate) fn uas_datalink_ls_icing_detected_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<IcingDetected>> {
    let v = obj.getattr(intern!(obj.py(), "icing_detected"))?;
    Ok(if v.is_none() {
        None
    } else {
        Some(IcingDetected::from_wire(enum_field_to_u8(&v)?))
    })
}

pub(crate) fn uas_datalink_ls_sensor_fov_name_to_py(
    py: Python<'_>,
    v: &Option<SensorFovName>,
) -> PyResult<PyObject> {
    let Some(v) = v else { return Ok(py.None()) };
    let other = match v {
        SensorFovName::Other(b) => Some(u64::from(*b)),
        _ => None,
    };
    coded_enum(py, "SensorFovName", other, u64::from(v.to_wire()))
}

pub(crate) fn uas_datalink_ls_sensor_fov_name_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<SensorFovName>> {
    let v = obj.getattr(intern!(obj.py(), "sensor_fov_name"))?;
    Ok(if v.is_none() {
        None
    } else {
        Some(SensorFovName::from_wire(enum_field_to_u8(&v)?))
    })
}

pub(crate) fn uas_datalink_ls_operational_mode_to_py(
    py: Python<'_>,
    v: &Option<OperationalMode>,
) -> PyResult<PyObject> {
    let Some(v) = v else { return Ok(py.None()) };
    let other = match v {
        OperationalMode::Other(b) => Some(u64::from(*b)),
        _ => None,
    };
    coded_enum(py, "OperationalMode", other, u64::from(v.to_wire()))
}

pub(crate) fn uas_datalink_ls_operational_mode_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<OperationalMode>> {
    let v = obj.getattr(intern!(obj.py(), "operational_mode"))?;
    Ok(if v.is_none() {
        None
    } else {
        Some(OperationalMode::from_wire(enum_field_to_u8(&v)?))
    })
}

pub(crate) fn uas_datalink_ls_platform_status_to_py(
    py: Python<'_>,
    v: &Option<PlatformStatus>,
) -> PyResult<PyObject> {
    let Some(v) = v else { return Ok(py.None()) };
    let other = match v {
        PlatformStatus::Other(b) => Some(u64::from(*b)),
        _ => None,
    };
    coded_enum(py, "PlatformStatus", other, u64::from(v.to_wire()))
}

pub(crate) fn uas_datalink_ls_platform_status_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<PlatformStatus>> {
    let v = obj.getattr(intern!(obj.py(), "platform_status"))?;
    Ok(if v.is_none() {
        None
    } else {
        Some(PlatformStatus::from_wire(enum_field_to_u8(&v)?))
    })
}

pub(crate) fn uas_datalink_ls_sensor_control_mode_to_py(
    py: Python<'_>,
    v: &Option<SensorControlMode>,
) -> PyResult<PyObject> {
    let Some(v) = v else { return Ok(py.None()) };
    let other = match v {
        SensorControlMode::Other(b) => Some(u64::from(*b)),
        _ => None,
    };
    coded_enum(py, "SensorControlMode", other, u64::from(v.to_wire()))
}

pub(crate) fn uas_datalink_ls_sensor_control_mode_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<SensorControlMode>> {
    let v = obj.getattr(intern!(obj.py(), "sensor_control_mode"))?;
    Ok(if v.is_none() {
        None
    } else {
        Some(SensorControlMode::from_wire(enum_field_to_u8(&v)?))
    })
}

pub(crate) fn uas_datalink_ls_unknown_to_py(
    py: Python<'_>,
    v: &[OwnedRawField],
) -> PyResult<PyObject> {
    convert_unknown(py, v)
}

/// An entry whose tag a typed field owns is dropped: typed wins.
pub(crate) fn uas_datalink_ls_unknown_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Vec<OwnedRawField>> {
    py_to_unknown(obj, is_st0601_typed_tag)
}

pub(crate) fn uas_datalink_ls_field_errors_to_py(
    py: Python<'_>,
    v: &[KlvFieldError],
) -> PyResult<PyObject> {
    convert_field_errors(py, v)
}

/// A parser diagnostic: it does not cross back.
pub(crate) fn uas_datalink_ls_field_errors_from_py(
    _obj: &Bound<'_, PyAny>,
) -> PyResult<Vec<KlvFieldError>> {
    Ok(Vec::new())
}

pub(crate) fn uas_datalink_ls_sentinel_tags_to_py(py: Python<'_>, v: &[u32]) -> PyResult<PyObject> {
    Ok(PyTuple::new_bound(py, v.iter().map(|&t| u64::from(t))).into_py(py))
}

/// Extracting a `u32` raises `OverflowError` on an out-of-range value instead
/// of truncating.
pub(crate) fn uas_datalink_ls_sentinel_tags_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    obj.getattr(intern!(obj.py(), "sentinel_tags"))?.extract()
}

/// `Vec<(u32, ImapbSpecial)>` crosses as a tuple of `(tag, code, payload)`.
/// `code` names the `ImapbSpecial` family; `payload` is the NaN id or signal
/// (0 for the codes that carry none).
pub(crate) fn uas_datalink_ls_imapb_specials_to_py(
    py: Python<'_>,
    v: &[(u32, ImapbSpecial)],
) -> PyResult<PyObject> {
    let items = v
        .iter()
        .map(|&(tag, special)| {
            let (code, payload) = imapb_special_to_code(special)?;
            Ok(PyTuple::new_bound(
                py,
                &[tag.into_py(py), code.into_py(py), payload.into_py(py)],
            )
            .unbind()
            .into_py(py))
        })
        .collect::<PyResult<Vec<PyObject>>>()?;
    Ok(PyTuple::new_bound(py, items).into_py(py))
}

pub(crate) fn uas_datalink_ls_imapb_specials_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Vec<(u32, ImapbSpecial)>> {
    let mut out = Vec::new();
    for item in obj.getattr(intern!(obj.py(), "imapb_specials"))?.iter()? {
        let (tag, code, payload): (u32, String, u64) = item?.extract()?;
        out.push((tag, imapb_special_from_code(&code, payload)?));
    }
    Ok(out)
}

/// Errors, rather than mislabelling, on a variant added later.
fn imapb_special_to_code(s: ImapbSpecial) -> PyResult<(&'static str, u64)> {
    Ok(match s {
        ImapbSpecial::BelowMin => ("below_min", 0),
        ImapbSpecial::AboveMax => ("above_max", 0),
        ImapbSpecial::PositiveInfinity => ("pos_infinity", 0),
        ImapbSpecial::NegativeInfinity => ("neg_infinity", 0),
        ImapbSpecial::PositiveQuietNaN { nan_id } => ("pos_quiet_nan", nan_id),
        ImapbSpecial::NegativeQuietNaN { nan_id } => ("neg_quiet_nan", nan_id),
        ImapbSpecial::PositiveSignalingNaN { signal } => ("pos_signaling_nan", signal),
        ImapbSpecial::NegativeSignalingNaN { signal } => ("neg_signaling_nan", signal),
        ImapbSpecial::UserDefined { signal } => ("user_defined", signal),
        // `#[non_exhaustive]` in tst-core: no current variant reaches here.
        _ => {
            return Err(PyValueError::new_err(
                "unknown ImapbSpecial variant crossing the binding",
            ));
        }
    })
}

/// A code outside the nine known ones raises `ValueError`: validate, don't drop.
fn imapb_special_from_code(code: &str, payload: u64) -> PyResult<ImapbSpecial> {
    Ok(match code {
        "below_min" => ImapbSpecial::BelowMin,
        "above_max" => ImapbSpecial::AboveMax,
        "pos_infinity" => ImapbSpecial::PositiveInfinity,
        "neg_infinity" => ImapbSpecial::NegativeInfinity,
        "pos_quiet_nan" => ImapbSpecial::PositiveQuietNaN { nan_id: payload },
        "neg_quiet_nan" => ImapbSpecial::NegativeQuietNaN { nan_id: payload },
        "pos_signaling_nan" => ImapbSpecial::PositiveSignalingNaN { signal: payload },
        "neg_signaling_nan" => ImapbSpecial::NegativeSignalingNaN { signal: payload },
        "user_defined" => ImapbSpecial::UserDefined { signal: payload },
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown imapb_specials code {other:?}; expected one of below_min, above_max, \
                 pos_infinity, neg_infinity, pos_quiet_nan, neg_quiet_nan, pos_signaling_nan, \
                 neg_signaling_nan, user_defined"
            )));
        }
    })
}

// ---------------------------------------------------------------------------
// ST 0601 — pack records
// ---------------------------------------------------------------------------

pub(crate) fn metadata_substream_id_uuid_to_py(
    py: Python<'_>,
    v: &Option<[u8; 16]>,
) -> PyResult<PyObject> {
    Ok(match v {
        Some(u) => PyBytes::new_bound(py, u).into_py(py),
        None => py.None(),
    })
}

pub(crate) fn metadata_substream_id_uuid_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<[u8; 16]>> {
    let v = obj.getattr(intern!(obj.py(), "uuid"))?;
    if v.is_none() {
        return Ok(None);
    }
    let bytes: Vec<u8> = v.extract()?;
    let uuid: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
        PyValueError::new_err(format!(
            "MetadataSubstreamId.uuid must be 16 bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(Some(uuid))
}

pub(crate) fn payload_record_payload_type_to_py(
    py: Python<'_>,
    v: &PayloadType,
) -> PyResult<PyObject> {
    let other = match v {
        PayloadType::Other(c) => Some(*c),
        _ => None,
    };
    coded_enum(py, "PayloadType", other, v.to_wire())
}

pub(crate) fn payload_record_payload_type_from_py(obj: &Bound<'_, PyAny>) -> PyResult<PayloadType> {
    let v = obj.getattr(intern!(obj.py(), "payload_type"))?;
    Ok(PayloadType::from_wire(enum_field_to_u64(&v)?))
}

/// The typed-tag predicates the `unknown` readers filter by: a caller-supplied
/// entry whose tag a typed field owns is dropped, because the typed field wins.
///
/// ST 0601 typed tags: 1..=65 and 67..=143. Tag 66 is the deprecated
/// placeholder and 144..=255 are forward-compat, so both may appear in
/// `unknown`. Keep this in step with the ST 0601 tag table when a tag is typed.
pub(crate) fn is_st0601_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=65 | 67..=143)
}

/// ST 0102 typed tags: 1..=14, 22, 23 and 24.
fn is_st0102_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=14 | 22 | 23 | 24)
}

/// ST 0903.6 VMTI typed tags: 1..=13 and 101..=103. Tag 7 is deprecated in
/// v6; it counts as typed so a colliding entry cannot re-enter.
fn is_st0903_vmti_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=13 | 101..=103)
}

/// ST 0903.6 VTargetPack typed tags: 1..=23 and 100..=107.
fn is_st0903_vtarget_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=23 | 100..=107)
}

/// ST 0806.4 RVT typed tags: 1..=21, no gaps.
fn is_st0806_rvt_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=21)
}

/// ST 0806.4 POI and AOI typed tags: both sets share 1..=10.
fn is_st0806_poi_aoi_typed_tag(tag: u32) -> bool {
    matches!(tag, 1..=10)
}

/// The two diagnostic-and-pass-through fields every typed set carries, per
/// record: `unknown` crosses both ways (typed tags dropped), `field_errors`
/// is a parser diagnostic that does not cross back.
macro_rules! unknown_and_field_errors {
    ($unknown_to:ident, $unknown_from:ident, $errors_to:ident, $errors_from:ident, $typed:ident) => {
        pub(crate) fn $unknown_to(py: Python<'_>, v: &[OwnedRawField]) -> PyResult<PyObject> {
            convert_unknown(py, v)
        }

        /// An entry whose tag a typed field owns is dropped: typed wins.
        pub(crate) fn $unknown_from(obj: &Bound<'_, PyAny>) -> PyResult<Vec<OwnedRawField>> {
            py_to_unknown(obj, $typed)
        }

        pub(crate) fn $errors_to(py: Python<'_>, v: &[KlvFieldError]) -> PyResult<PyObject> {
            convert_field_errors(py, v)
        }

        /// A parser diagnostic: it does not cross back.
        pub(crate) fn $errors_from(_obj: &Bound<'_, PyAny>) -> PyResult<Vec<KlvFieldError>> {
            Ok(Vec::new())
        }
    };
}

/// A coded-enum field: the `tstrans.klv` member for a known code point, the
/// raw `int` for the enum's wire-unknown variant.
macro_rules! coded_enum_fields {
    ($to:ident, $from:ident, $ty:ident, $unknown:ident, $field:literal) => {
        pub(crate) fn $to(py: Python<'_>, v: &Option<$ty>) -> PyResult<PyObject> {
            let Some(v) = v else { return Ok(py.None()) };
            let other = match v {
                $ty::$unknown(b) => Some(u64::from(*b)),
                _ => None,
            };
            coded_enum(py, stringify!($ty), other, u64::from(v.to_wire()))
        }

        pub(crate) fn $from(obj: &Bound<'_, PyAny>) -> PyResult<Option<$ty>> {
            let v = obj.getattr(intern!(obj.py(), $field))?;
            Ok(if v.is_none() {
                None
            } else {
                Some($ty::from_wire(enum_field_to_u8(&v)?))
            })
        }
    };
}

// ---------------------------------------------------------------------------
// ST 0102 — SecurityLs
// ---------------------------------------------------------------------------

coded_enum_fields!(
    security_ls_security_classification_to_py,
    security_ls_security_classification_from_py,
    SecurityClassification,
    Unknown,
    "security_classification"
);
coded_enum_fields!(
    security_ls_classifying_country_coding_method_to_py,
    security_ls_classifying_country_coding_method_from_py,
    ClassifyingCountryCodingMethod,
    Unknown,
    "classifying_country_coding_method"
);
coded_enum_fields!(
    security_ls_object_country_coding_method_to_py,
    security_ls_object_country_coding_method_from_py,
    ObjectCountryCodingMethod,
    Unknown,
    "object_country_coding_method"
);
unknown_and_field_errors!(
    security_ls_unknown_to_py,
    security_ls_unknown_from_py,
    security_ls_field_errors_to_py,
    security_ls_field_errors_from_py,
    is_st0102_typed_tag
);

// ---------------------------------------------------------------------------
// ST 0605 — PrecisionTimeStampPack
// ---------------------------------------------------------------------------

pub(crate) fn precision_time_stamp_pack_time_status_to_py(
    py: Python<'_>,
    v: &TimeStatus,
) -> PyResult<PyObject> {
    let cls = py.import_bound("tstrans.klv")?.getattr("TimeStatus")?;
    Ok(cls.call1((v.0,))?.unbind())
}

pub(crate) fn precision_time_stamp_pack_time_status_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<TimeStatus> {
    let py = obj.py();
    let raw: u8 = obj
        .getattr(intern!(py, "time_status"))?
        .getattr(intern!(py, "raw"))?
        .extract()?;
    Ok(TimeStatus(raw))
}

// ---------------------------------------------------------------------------
// ST 0903 — VTargetPack, VmtiLs
// ---------------------------------------------------------------------------

pub(crate) fn v_target_pack_target_color_to_py(
    py: Python<'_>,
    v: &Option<[u8; 3]>,
) -> PyResult<PyObject> {
    Ok(match v {
        Some(rgb) => PyTuple::new_bound(py, rgb).into_py(py),
        None => py.None(),
    })
}

/// A non-null value of any length but 3 is a caller bug: it raises instead of
/// dropping the colour.
pub(crate) fn v_target_pack_target_color_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<[u8; 3]>> {
    let v = obj.getattr(intern!(obj.py(), "target_color"))?;
    if v.is_none() {
        return Ok(None);
    }
    let rgb: Vec<u8> = v.extract()?;
    let rgb: [u8; 3] = rgb.as_slice().try_into().map_err(|_| {
        PyValueError::new_err(format!(
            "target_color must be a 3-tuple, got length {}",
            rgb.len()
        ))
    })?;
    Ok(Some(rgb))
}

unknown_and_field_errors!(
    v_target_pack_unknown_to_py,
    v_target_pack_unknown_from_py,
    v_target_pack_field_errors_to_py,
    v_target_pack_field_errors_from_py,
    is_st0903_vtarget_typed_tag
);
unknown_and_field_errors!(
    vmti_ls_unknown_to_py,
    vmti_ls_unknown_from_py,
    vmti_ls_field_errors_to_py,
    vmti_ls_field_errors_from_py,
    is_st0903_vmti_typed_tag
);

// ---------------------------------------------------------------------------
// ST 0806 — RvtPoi, RvtAoi, RvtLs
// ---------------------------------------------------------------------------

coded_enum_fields!(
    rvt_poi_poi_type_to_py,
    rvt_poi_poi_type_from_py,
    RvtPoiType,
    Other,
    "poi_type"
);
coded_enum_fields!(
    rvt_aoi_aoi_type_to_py,
    rvt_aoi_aoi_type_from_py,
    RvtAoiType,
    Other,
    "aoi_type"
);

pub(crate) fn rvt_poi_sentinel_tags_to_py(py: Python<'_>, v: &[u32]) -> PyResult<PyObject> {
    Ok(PyTuple::new_bound(py, v.iter().map(|&t| u64::from(t))).into_py(py))
}

pub(crate) fn rvt_poi_sentinel_tags_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    obj.getattr(intern!(obj.py(), "sentinel_tags"))?.extract()
}

pub(crate) fn rvt_aoi_sentinel_tags_to_py(py: Python<'_>, v: &[u32]) -> PyResult<PyObject> {
    rvt_poi_sentinel_tags_to_py(py, v)
}

pub(crate) fn rvt_aoi_sentinel_tags_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    rvt_poi_sentinel_tags_from_py(obj)
}

unknown_and_field_errors!(
    rvt_poi_unknown_to_py,
    rvt_poi_unknown_from_py,
    rvt_poi_field_errors_to_py,
    rvt_poi_field_errors_from_py,
    is_st0806_poi_aoi_typed_tag
);
unknown_and_field_errors!(
    rvt_aoi_unknown_to_py,
    rvt_aoi_unknown_from_py,
    rvt_aoi_field_errors_to_py,
    rvt_aoi_field_errors_from_py,
    is_st0806_poi_aoi_typed_tag
);
unknown_and_field_errors!(
    rvt_ls_unknown_to_py,
    rvt_ls_unknown_from_py,
    rvt_ls_field_errors_to_py,
    rvt_ls_field_errors_from_py,
    is_st0806_rvt_typed_tag
);

// ---------------------------------------------------------------------------
// ST 1010 — SdccFlp
// ---------------------------------------------------------------------------

pub(crate) fn sdcc_flp_std_devs_to_py(py: Python<'_>, v: &[f64]) -> PyResult<PyObject> {
    Ok(PyTuple::new_bound(py, v).into_py(py))
}

pub(crate) fn sdcc_flp_correlations_to_py(py: Python<'_>, v: &[f64]) -> PyResult<PyObject> {
    Ok(PyTuple::new_bound(py, v).into_py(py))
}

pub(crate) fn sdcc_flp_correlation_present_to_py(py: Python<'_>, v: &[bool]) -> PyResult<PyObject> {
    Ok(PyTuple::new_bound(py, v).into_py(py))
}

// ---------------------------------------------------------------------------
// ST 1204 — CoreId
// ---------------------------------------------------------------------------

/// The `tstrans.klv.IdType` member of the same name.
fn id_type_to_py(py: Python<'_>, ty: IdType) -> PyResult<PyObject> {
    let name = match ty {
        IdType::Physical => "PHYSICAL",
        IdType::Virtual => "VIRTUAL",
        IdType::Managed => "MANAGED",
        _ => {
            return Err(PyValueError::new_err(
                "unknown IdType variant crossing the binding",
            ));
        }
    };
    let cls = py.import_bound("tstrans.klv")?.getattr("IdType")?;
    Ok(cls.getattr(name)?.unbind())
}

fn id_type_from_py(obj: &Bound<'_, PyAny>) -> PyResult<IdType> {
    let name: String = obj.getattr("name")?.extract()?;
    match name.as_str() {
        "PHYSICAL" => Ok(IdType::Physical),
        "VIRTUAL" => Ok(IdType::Virtual),
        "MANAGED" => Ok(IdType::Managed),
        other => Err(PyValueError::new_err(format!(
            "unknown IdType variant: {other}"
        ))),
    }
}

/// A UUID of any length but 16 is a caller bug: it raises, naming the field.
fn uuid_from_bytes(field: &str, bytes: &[u8]) -> PyResult<[u8; 16]> {
    bytes.try_into().map_err(|_| {
        PyValueError::new_err(format!(
            "CoreId.{field} UUID must be 16 bytes, got {}",
            bytes.len()
        ))
    })
}

/// A typed UUID crosses as a `(IdType, bytes)` tuple.
fn typed_uuid_to_py(py: Python<'_>, v: &Option<(IdType, [u8; 16])>) -> PyResult<PyObject> {
    Ok(match v {
        Some((ty, uuid)) => {
            let ty = id_type_to_py(py, *ty)?;
            let bytes: PyObject = PyBytes::new_bound(py, uuid).into_py(py);
            PyTuple::new_bound(py, [ty, bytes]).into_py(py)
        }
        None => py.None(),
    })
}

fn typed_uuid_from_py(
    obj: &Bound<'_, PyAny>,
    field: &'static str,
) -> PyResult<Option<(IdType, [u8; 16])>> {
    let v = obj.getattr(field)?;
    if v.is_none() {
        return Ok(None);
    }
    let (ty, bytes): (Bound<'_, PyAny>, Vec<u8>) = v.extract()?;
    Ok(Some((
        id_type_from_py(&ty)?,
        uuid_from_bytes(field, &bytes)?,
    )))
}

fn plain_uuid_to_py(py: Python<'_>, v: &Option<[u8; 16]>) -> PyObject {
    match v {
        Some(uuid) => PyBytes::new_bound(py, uuid).into_py(py),
        None => py.None(),
    }
}

fn plain_uuid_from_py(obj: &Bound<'_, PyAny>, field: &'static str) -> PyResult<Option<[u8; 16]>> {
    let v = obj.getattr(field)?;
    if v.is_none() {
        return Ok(None);
    }
    let bytes: Vec<u8> = v.extract()?;
    Ok(Some(uuid_from_bytes(field, &bytes)?))
}

pub(crate) fn core_id_sensor_to_py(
    py: Python<'_>,
    v: &Option<(IdType, [u8; 16])>,
) -> PyResult<PyObject> {
    typed_uuid_to_py(py, v)
}

pub(crate) fn core_id_sensor_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<(IdType, [u8; 16])>> {
    typed_uuid_from_py(obj, "sensor")
}

pub(crate) fn core_id_platform_to_py(
    py: Python<'_>,
    v: &Option<(IdType, [u8; 16])>,
) -> PyResult<PyObject> {
    typed_uuid_to_py(py, v)
}

pub(crate) fn core_id_platform_from_py(
    obj: &Bound<'_, PyAny>,
) -> PyResult<Option<(IdType, [u8; 16])>> {
    typed_uuid_from_py(obj, "platform")
}

pub(crate) fn core_id_window_to_py(py: Python<'_>, v: &Option<[u8; 16]>) -> PyResult<PyObject> {
    Ok(plain_uuid_to_py(py, v))
}

pub(crate) fn core_id_window_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Option<[u8; 16]>> {
    plain_uuid_from_py(obj, "window")
}

pub(crate) fn core_id_minor_to_py(py: Python<'_>, v: &Option<[u8; 16]>) -> PyResult<PyObject> {
    Ok(plain_uuid_to_py(py, v))
}

pub(crate) fn core_id_minor_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Option<[u8; 16]>> {
    plain_uuid_from_py(obj, "minor")
}
