pub(crate) fn wavelength_record_to_py(py: Python<'_>, r: &tst_core::klv::st0601::WavelengthRecord) -> PyResult<PyObject> {
    let module = py.import_bound("tstrans.klv")?;
    let cls = module.getattr(intern!(py, "WavelengthRecord"))?;
    let kwargs = PyDict::new_bound(py);
    kwargs.set_item(intern!(py, "id"), r.id)?;
    kwargs.set_item(intern!(py, "min_nm"), r.min_nm)?;
    kwargs.set_item(intern!(py, "max_nm"), r.max_nm)?;
    kwargs.set_item(intern!(py, "name"), r.name.as_str())?;
    Ok(cls.call((), Some(&kwargs))?.unbind())
}

pub(crate) fn wavelength_record_from_py(obj: &Bound<'_, PyAny>) -> PyResult<tst_core::klv::st0601::WavelengthRecord> {
    let py = obj.py();
    Ok(tst_core::klv::st0601::WavelengthRecord {
        id: obj.getattr(intern!(py, "id"))?.extract()?,
        min_nm: obj.getattr(intern!(py, "min_nm"))?.extract()?,
        max_nm: obj.getattr(intern!(py, "max_nm"))?.extract()?,
        name: obj.getattr(intern!(py, "name"))?.extract()?,
    })
}
