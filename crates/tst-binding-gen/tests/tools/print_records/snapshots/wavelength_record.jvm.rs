pub(crate) fn build_wavelength_record<'l>(env: &mut Env<'l>, r: &tst_core::klv::st0601::WavelengthRecord) -> jni::errors::Result<JObject<'l>> {
    env.ensure_local_capacity(40)?;
    let name = env.new_string(&r.name)?;
    let obj = env.new_object(crate::jutil::FindClass(jni_str!("org/tstrans/klv/WavelengthRecord")), jni_sig!("(JDDLjava/lang/String;)V"), &[JValue::Long(r.id as i64), JValue::Double(r.min_nm), JValue::Double(r.max_nm), JValue::Object(&name)])?;
    Ok(obj)
}

pub(crate) fn read_wavelength_record(env: &mut Env<'_>, obj: &JObject<'_>) -> jni::errors::Result<tst_core::klv::st0601::WavelengthRecord> {
    env.ensure_local_capacity(40)?;
    let id = env.call_method(obj, jni_str!("id"), jni_sig!("()J"), &[])?.j()?;
    let id = id as u64;
    let min_nm = env.call_method(obj, jni_str!("minNm"), jni_sig!("()D"), &[])?.d()?;
    let max_nm = env.call_method(obj, jni_str!("maxNm"), jni_sig!("()D"), &[])?.d()?;
    let o1 = env.call_method(obj, jni_str!("name"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
    let name = { if o1.is_null() { return Err(crate::jutil::null_field(env, "WavelengthRecord.name")); } let s2 = env.as_cast::<jni::objects::JString>(&o1)?; s2.try_to_string(env)? };
    Ok(tst_core::klv::st0601::WavelengthRecord {
        id,
        min_nm,
        max_nm,
        name,
    })
}
