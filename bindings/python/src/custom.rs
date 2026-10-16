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
use tst_core::klv::st0601::{
    IcingDetected, OperationalMode, PayloadType, PlatformStatus, SensorControlMode, SensorFovName,
};

use crate::klv::{
    convert_field_errors, convert_unknown, enum_field_to_u8, enum_field_to_u64,
    is_st0601_typed_tag, py_to_unknown,
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
