//! The JNI pair: `build_<snake>` and `read_<snake>` as committed Rust text
//! for `bindings/jvm/src/generated/<family>.rs`.

use crate::emit_py::HEADER;
use crate::schema::{Entry, lookup, snake};
use crate::typemap::{Fld, Sc, Ty, resolve};

/// A record with more crossing fields than this is built through its `Builder`.
pub const CTOR_MAX: usize = 8;

/// Local references reserved up front: two per crossing field plus headroom
/// for the temporaries a conversion mints. A nested record is built inside
/// its own local frame and hands back one reference, so it counts as one
/// slot like any other object field.
pub fn capacity(fields: usize) -> usize {
    2 * fields + 32
}

/// The `JValue` for a scalar given a by-value expression.
fn scalar_jvalue(sc: Sc, val: &str) -> String {
    let i = sc.info();
    let arg = match sc {
        Sc::U8 | Sc::U16 | Sc::I8 | Sc::I16 => format!("{val} as i32"),
        Sc::U32 | Sc::U64 | Sc::Usize => format!("{val} as i64"),
        Sc::Duration => format!("i64::try_from({val}.as_micros()).unwrap_or(i64::MAX)"),
        Sc::Bool => format!("u8::from({val})"),
        _ => val.to_string(),
    };
    format!("JValue::{}({arg})", i.jvalue)
}

/// `&expr` to `expr`, anything else to `*expr`.
fn deref(src: &str) -> String {
    match src.strip_prefix('&') {
        Some(rest) => rest.to_string(),
        None => format!("*{src}"),
    }
}

fn conv_fn(reg: &[Entry], from: &Entry, name: &str, prefix: &str) -> String {
    let to = lookup(reg, name).expect("registered");
    let f = format!("{prefix}_{}", snake(name));
    if to.family == from.family {
        f
    } else {
        format!("crate::generated::{}::{f}", to.family)
    }
}

struct Gen<'a> {
    reg: &'a [Entry],
    e: &'a Entry,
    n: usize,
}

impl Gen<'_> {
    fn fresh(&mut self, base: &str) -> String {
        self.n += 1;
        format!("{base}{}", self.n)
    }

    /// Statements that build a Java object for `src` (a `&T` expression);
    /// returns the variable that holds it.
    fn build_obj(
        &mut self,
        ty: &Ty,
        src: &str,
        hint: Option<&str>,
        pre: &mut Vec<String>,
    ) -> String {
        let var = match hint {
            Some(h) => h.to_string(),
            None => self.fresh("o"),
        };
        match ty {
            Ty::Str => pre.push(format!("let {var} = env.new_string({src})?;")),
            Ty::Bytes => pre.push(format!(
                "let {var} = crate::jutil::wrap_heap_byte_buffer(env, {src}).map_err(|()| jni::errors::Error::JavaException)?;"
            )),
            // The nested record mints its own temporaries; the frame frees them
            // and keeps one reference.
            Ty::Record(n) => pre.push(format!(
                "let {var} = env.with_local_frame_returning_local::<_, jni::errors::Error>(16, |env| {}(env, {src}))?;",
                conv_fn(self.reg, self.e, n, "build")
            )),
            Ty::Scalar(sc) => {
                let i = sc.info();
                pre.push(format!(
                    "let {var} = env.call_static_method(\"java/lang/{b}\", \"valueOf\", \"({})Ljava/lang/{b};\", &[{}])?.l()?;",
                    i.sig,
                    scalar_jvalue(*sc, &deref(src)),
                    b = i.boxed
                ));
            }
            Ty::Opt(inner) => {
                let mut inner_pre = Vec::new();
                let x = self.build_obj(inner, "v", None, &mut inner_pre);
                let body = if inner_pre.is_empty() {
                    format!("JObject::from({x})")
                } else {
                    format!("{{ {} JObject::from({x}) }}", inner_pre.join(" "))
                };
                pre.push(format!(
                    "let {var} = match {src} {{ Some(v) => {body}, None => JObject::null() }};"
                ));
            }
            Ty::List(inner) => {
                let mut item_pre = Vec::new();
                let x = self.build_obj(inner, "item", None, &mut item_pre);
                let seq = src.strip_prefix('&').unwrap_or(src);
                pre.push(format!(
                    "let {var} = env.with_local_frame_returning_local::<_, jni::errors::Error>(16, |env| {{ \
                     let list = env.new_object(\"java/util/ArrayList\", \"()V\", &[])?; \
                     for item in {seq}.iter() {{ \
                     env.with_local_frame::<_, (), jni::errors::Error>(16, |env| {{ \
                     {} env.call_method(&list, \"add\", \"(Ljava/lang/Object;)Z\", &[JValue::Object(&{x})])?; Ok(()) }})?; }} \
                     Ok(list) }})?;",
                    item_pre.join(" ")
                ));
            }
            Ty::Host(h) => pre.push(format!(
                "let {var} = {}(env, {}).map_err(|()| jni::errors::Error::JavaException)?;",
                crate::typemap::host_jvm(h),
                deref(src)
            )),
            Ty::Map(inner) => {
                let mut item_pre = Vec::new();
                let x = self.build_obj(inner, "item", None, &mut item_pre);
                let seq = src.strip_prefix('&').unwrap_or(src);
                pre.push(format!(
                    "let {var} = env.with_local_frame_returning_local::<_, jni::errors::Error>(16, |env| {{ \
                     let map = env.new_object(\"java/util/HashMap\", \"()V\", &[])?; \
                     for (key, item) in {seq}.iter() {{ \
                     env.with_local_frame::<_, (), jni::errors::Error>(16, |env| {{ \
                     let k = env.call_static_method(\"java/lang/Integer\", \"valueOf\", \"(I)Ljava/lang/Integer;\", &[JValue::Int(i32::from(*key))])?.l()?; \
                     {} env.call_method(&map, \"put\", \"(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;\", &[JValue::Object(&k), JValue::Object(&{x})])?; Ok(()) }})?; }} \
                     Ok(map) }})?;",
                    item_pre.join(" ")
                ));
            }
            Ty::Custom(_) => unreachable!("custom fields are converted by name"),
        }
        var
    }

    /// An expression that reads the Java object `o` as `ty`.
    fn read_obj(&mut self, ty: &Ty, o: &str, path: &str) -> String {
        match ty {
            Ty::Str => self.read_str(o, Some(path)),
            Ty::Bytes => format!("crate::jutil::read_byte_buffer(env, &{o})?"),
            Ty::Record(n) => format!("{}(env, &{o})?", conv_fn(self.reg, self.e, n, "read")),
            Ty::Scalar(sc) => {
                let i = sc.info();
                let raw = format!(
                    "env.call_method(&{o}, \"{}\", \"(){}\", &[])?.{}()?",
                    i.unbox, i.sig, i.extract
                );
                self.narrow(*sc, &raw, path)
            }
            Ty::Opt(inner) => {
                // The null test above already covers a string.
                let v = match inner.as_ref() {
                    Ty::Str => self.read_str(o, None),
                    _ => self.read_obj(inner, o, path),
                };
                format!("if {o}.is_null() {{ None }} else {{ Some({v}) }}")
            }
            Ty::List(inner) => {
                let e = self.fresh("e");
                let item = self.read_obj(inner, &e, path);
                // `Ok(x?)` is `x`; clippy flags the former.
                let tail = match item.strip_suffix('?') {
                    Some(call) if !call.starts_with('{') && !call.starts_with("if ") => {
                        call.to_string()
                    }
                    _ => format!("Ok({item})"),
                };
                format!(
                    "{{ let size = env.call_method(&{o}, \"size\", \"()I\", &[])?.i()?; \
                     let mut items = Vec::with_capacity(size.max(0) as usize); \
                     for i in 0..size {{ \
                     let item = env.with_local_frame(16, |env| -> jni::errors::Result<_> {{ \
                     let {e} = env.call_method(&{o}, \"get\", \"(I)Ljava/lang/Object;\", &[JValue::Int(i)])?.l()?; \
                     {tail} }})?; items.push(item); }} items }}"
                )
            }
            Ty::Host(_) | Ty::Map(_) => unreachable!("a host-typed field is built, never read"),
            Ty::Custom(_) => unreachable!("custom fields are converted by name"),
        }
    }

    /// An expression that reads the Java `String` `o`. With a `path`, a null
    /// is an `IllegalArgumentException` naming that field: a required string
    /// has no Rust value to take its place.
    fn read_str(&mut self, o: &str, path: Option<&str>) -> String {
        let s = self.fresh("s");
        let guard = match path {
            Some(p) => format!(
                "if {o}.is_null() {{ return Err(crate::jutil::null_field(env, \"{p}\")); }} "
            ),
            None => String::new(),
        };
        format!(
            "{{ {guard}let {s}: &jni::objects::JString = (&{o}).into(); String::from(env.get_string({s})?) }}"
        )
    }

    /// `raw` is a value of the scalar's Java width; unsigned types are
    /// range-checked. The raw read is bound first: `env` cannot be borrowed
    /// twice in one call.
    fn narrow(&mut self, sc: Sc, raw: &str, path: &str) -> String {
        match sc.narrow() {
            None if sc == Sc::U64 => format!("({raw}) as u64"),
            None => raw.to_string(),
            Some(f) => {
                let wide = match sc {
                    Sc::U8 | Sc::U16 | Sc::I8 | Sc::I16 => "i64::from(n)",
                    _ => "n",
                };
                format!("{{ let n = {raw}; crate::jutil::{f}(env, {wide}, \"{path}\")? }}")
            }
        }
    }
}

const RESERVED: &[&str] = &[
    "env", "r", "obj", "b", "v", "item", "list", "size", "items", "i",
];

fn var_of(name: &str) -> String {
    if RESERVED.contains(&name) {
        format!("{name}_f")
    } else {
        name.to_string()
    }
}

fn build(reg: &[Entry], e: &Entry, flds: &[Fld]) -> String {
    let s = &e.schema;
    let path = e.rust_path;
    let class = format!("{}/{}", e.java_pkg, e.name());
    let crossing: Vec<&Fld> = flds.iter().filter(|f| f.ty.is_some()).collect();
    let mut g = Gen { reg, e, n: 0 };
    let mut o = format!(
        "pub(crate) fn build_{}<'l>(env: &mut JNIEnv<'l>, r: &{path}) -> jni::errors::Result<JObject<'l>> {{\n",
        snake(s.name)
    );
    o.push_str(&format!(
        "    env.ensure_local_capacity({})?;\n",
        capacity(crossing.len())
    ));
    let builder = crossing.len() > CTOR_MAX;
    let bclass = format!("{class}$Builder");
    let mut vals = Vec::new();
    let mut setters = String::new();
    for f in &crossing {
        let ty = f.ty.as_ref().expect("crossing");
        let name = f.spec.name;
        let var = var_of(name);
        let mut pre = Vec::new();
        // A builder leaves an absent optional field unset: the setter call sits
        // inside `if let Some(v)`, and the value is built from the inner type.
        let skip_absent = builder && !f.custom && ty.is_opt();
        let val = if f.custom {
            pre.push(format!(
                "let {var} = crate::custom::build_{}_{name}(env, &r.{name})?;",
                snake(s.name)
            ));
            match ty.custom_prim() {
                Some(kind) => format!("JValue::{kind}({var})"),
                None => format!("JValue::Object(&{var})"),
            }
        } else if let Ty::Scalar(sc) = ty {
            scalar_jvalue(*sc, &format!("r.{name}"))
        } else if skip_absent {
            let Ty::Opt(inner) = ty else { unreachable!() };
            if let Ty::Scalar(sc) = inner.as_ref() {
                // A setter takes the primitive; there is nothing to box.
                scalar_jvalue(*sc, "*v")
            } else {
                let x = g.build_obj(inner, "v", None, &mut pre);
                format!("JValue::Object(&{x})")
            }
        } else {
            let x = g.build_obj(ty, &format!("&r.{name}"), Some(&var), &mut pre);
            format!("JValue::Object(&{x})")
        };
        if builder {
            let call = format!(
                "env.call_method(&b, \"{}\", \"({})L{bclass};\", &[{val}])?;",
                f.java,
                ty.setter_sig(reg)
            );
            if skip_absent {
                setters.push_str(&format!(
                    "    if let Some(v) = &r.{name} {{ {} {call} }}\n",
                    pre.join(" ")
                ));
            } else {
                for p in pre {
                    setters.push_str(&format!("    {p}\n"));
                }
                setters.push_str(&format!("    {call}\n"));
            }
        } else {
            for p in pre {
                o.push_str(&format!("    {p}\n"));
            }
            vals.push((ty.sig(reg), val));
        }
    }
    if builder {
        o.push_str(&format!(
            "    let b = env.new_object(\"{bclass}\", \"()V\", &[])?;\n"
        ));
        o.push_str(&setters);
        o.push_str(&format!(
            "    let obj = env.call_method(&b, \"build\", \"()L{class};\", &[])?.l()?;\n"
        ));
    } else {
        let desc: String = vals.iter().map(|(s, _)| s.as_str()).collect();
        let args: Vec<&str> = vals.iter().map(|(_, v)| v.as_str()).collect();
        o.push_str(&format!(
            "    let obj = env.new_object(\"{class}\", \"({desc})V\", &[{}])?;\n",
            args.join(", ")
        ));
    }
    o.push_str("    Ok(obj)\n}\n");
    o
}

fn read(reg: &[Entry], e: &Entry, flds: &[Fld]) -> String {
    let s = &e.schema;
    let path = e.rust_path;
    let mut g = Gen { reg, e, n: 0 };
    let mut o = format!(
        "pub(crate) fn read_{}(env: &mut JNIEnv<'_>, obj: &JObject<'_>) -> jni::errors::Result<{path}> {{\n",
        snake(s.name)
    );
    // Every object-valued getter mints a local reference that lives until
    // the caller returns, so the reader reserves what the builder does.
    o.push_str(&format!(
        "    env.ensure_local_capacity({})?;\n",
        capacity(flds.iter().filter(|f| f.ty.is_some()).count())
    ));
    for f in flds.iter().filter(|f| f.ty.is_some()) {
        let ty = f.ty.as_ref().expect("crossing");
        let name = f.spec.name;
        let var = var_of(name);
        let fpath = format!("{}.{}", e.name(), f.java);
        if f.custom {
            o.push_str(&format!(
                "    let {var} = crate::custom::read_{}_{name}(env, obj)?;\n",
                snake(s.name)
            ));
        } else if let Ty::Scalar(sc) = ty {
            let i = sc.info();
            let raw = format!(
                "env.call_method(obj, \"{}\", \"(){}\", &[])?.{}()?",
                f.java, i.sig, i.extract
            );
            o.push_str(&format!("    let {var} = {raw};\n"));
            if *sc == Sc::U64 {
                o.push_str(&format!("    let {var} = {var} as u64;\n"));
            } else if let Some(f) = sc.narrow() {
                let wide = match sc {
                    Sc::U8 | Sc::U16 | Sc::I8 | Sc::I16 => format!("i64::from({var})"),
                    _ => var.clone(),
                };
                o.push_str(&format!(
                    "    let {var} = crate::jutil::{f}(env, {wide}, \"{fpath}\")?;\n"
                ));
            }
        } else {
            let obj = g.fresh("o");
            o.push_str(&format!(
                "    let {obj} = env.call_method(obj, \"{}\", \"(){}\", &[])?.l()?;\n",
                f.java,
                ty.sig(reg)
            ));
            let x = g.read_obj(ty, &obj, &fpath);
            o.push_str(&format!("    let {var} = {x};\n"));
        }
    }
    if s.non_exhaustive {
        o.push_str(&format!("    let mut r = {path}::default();\n"));
        for f in flds.iter().filter(|f| f.ty.is_some()) {
            o.push_str(&format!(
                "    r.{} = {};\n",
                f.spec.name,
                var_of(f.spec.name)
            ));
        }
        o.push_str("    Ok(r)\n}\n");
    } else {
        o.push_str(&format!("    Ok({path} {{\n"));
        for f in flds {
            if f.ty.is_some() {
                let v = var_of(f.spec.name);
                if v == f.spec.name {
                    o.push_str(&format!("        {v},\n"));
                } else {
                    o.push_str(&format!("        {}: {v},\n", f.spec.name));
                }
            } else {
                o.push_str(&format!("        {}: Default::default(),\n", f.spec.name));
            }
        }
        o.push_str("    })\n}\n");
    }
    o
}

/// The builder and reader text for one record, per its direction.
pub fn record_pair(reg: &[Entry], e: &Entry) -> Result<String, String> {
    let flds = resolve(reg, &e.schema)?;
    if e.direction.crosses_in()
        && let Some(f) = flds
            .iter()
            .find(|f| f.ty.as_ref().is_some_and(|t| t.is_to_host_only()) && !f.custom)
    {
        return Err(format!(
            "{}.{}: a map or a host-typed field only crosses to the host; register the record ToHost",
            e.schema.name, f.spec.name
        ));
    }
    let mut parts = Vec::new();
    if e.direction.crosses_out() {
        parts.push(build(reg, e, &flds));
    }
    if e.direction.crosses_in() {
        parts.push(read(reg, e, &flds));
    }
    Ok(parts.join("\n"))
}

/// The whole `generated/<family>.rs` text.
pub fn file(reg: &[Entry], family: &str) -> Result<String, String> {
    let mut body = Vec::new();
    for e in reg
        .iter()
        .filter(|e| e.family == family && e.jvm && e.jvm_builder)
    {
        body.push(record_pair(reg, e)?);
    }
    let body = body.join("\n");
    let mut out = String::from(HEADER);
    out.push_str("\n#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]\n\n");
    out.push_str("use jni::JNIEnv;\n");
    if body.contains("JValue::") {
        out.push_str("use jni::objects::{JObject, JValue};\n");
    } else {
        out.push_str("use jni::objects::JObject;\n");
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}
