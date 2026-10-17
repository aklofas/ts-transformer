//! The schema of a record that crosses to a host language.
//!
//! One declaration per crossing record: [`binding_record!`](crate::binding_record)
//! emits the struct exactly as written, plus an associated
//! `BINDING_SCHEMA` constant naming every field, its type and how it
//! crosses. The binding generator reads that constant to render the Python
//! and JVM projections, so the struct stays the single schema.
//!
//! Internal tier: the shape exists for the generator, not for consumers.
//!
//! A field's `ty` is the type's spelling as written in the declaration
//! (`stringify!`). The binding generator maps spellings, so a type alias is
//! not resolved and is reported by the generator.
//!
//! ```ignore
//! tst_core::binding_record! {
//!     /// A band definition.
//!     #[derive(Debug, Clone, PartialEq)]
//!     pub struct Band {
//!         /// Band id.
//!         pub id: u64,
//!         @[custom] pub name: Option<String>, // converted by a hand-written function
//!         @[skip] pub scratch: Vec<u8>,       // never crosses
//!         @[java("minCode")] pub min: u8,     // host name override
//!         @[py_default("0")] pub count: u8,   // Python dataclass default
//!     }
//! }
//! ```

/// How a field crosses: generated as-is, through a hand-written per-host
/// converter named by convention, or not at all.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMark {
    /// Generated as-is from the field's type.
    Plain,
    /// Converted by a hand-written function named by convention.
    Custom,
    /// Does not cross.
    Skip,
}

/// One field of a [`RecordSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    /// The Rust field name.
    pub name: &'static str,
    /// The field type as written in the declaration (`stringify!` of the type).
    pub ty: &'static str,
    /// How the field crosses.
    pub mark: FieldMark,
    /// Host name override; `None` means the camelCase of `name`.
    pub java: Option<&'static str>,
    /// The Python source text of the field's dataclass default, when the
    /// host default is not the one the generator derives from the type.
    pub py_default: Option<&'static str>,
    /// The field's doc lines joined by `"\n"`, each as written after `///`;
    /// `""` when it has none.
    pub doc: &'static str,
}

/// The schema of one record declared with [`binding_record!`](crate::binding_record).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordSchema {
    /// Full path, e.g. `tst_core::klv::st0601::WavelengthRecord`.
    pub rust_path: &'static str,
    /// The struct name.
    pub name: &'static str,
    /// Whether the struct is `#[non_exhaustive]`.
    pub non_exhaustive: bool,
    /// The fields in declaration order.
    pub fields: &'static [FieldSpec],
    /// The record's doc lines joined by `"\n"`, each as written after `///`;
    /// `""` when it has none.
    pub doc: &'static str,
}

/// Every `#[doc = "…"]` among the attributes, joined by `"\n"` with each line
/// as written after `///` (a leading space stays); `""` when there is none.
#[doc(hidden)]
#[macro_export]
macro_rules! __docs {
    (@go [$($acc:tt)*];) => { concat!($($acc)*) };
    (@go []; #[doc = $d:literal] $($r:tt)*) => { $crate::__docs!(@go [$d,]; $($r)*) };
    (@go [$($acc:tt)+]; #[doc = $d:literal] $($r:tt)*) => {
        $crate::__docs!(@go [$($acc)+ "\n", $d,]; $($r)*)
    };
    (@go [$($acc:tt)*]; #[$($x:tt)*] $($r:tt)*) => { $crate::__docs!(@go [$($acc)*]; $($r)*) };
    ($($t:tt)*) => { $crate::__docs!(@go []; $($t)*) };
}

/// `true` when `non_exhaustive` is among the attributes.
#[doc(hidden)]
#[macro_export]
macro_rules! __has_non_exhaustive {
    () => { false };
    (#[non_exhaustive] $($rest:tt)*) => { true };
    (#[$($x:tt)*] $($rest:tt)*) => { $crate::__has_non_exhaustive!($($rest)*) };
}

/// The [`FieldMark`] named by a field's `@[...]` groups, scanned as one flat
/// token list: none is `Plain`, `custom` or `skip` selects its mark (both is
/// an error), `java("…")` and commas are skipped, anything else is an error.
#[doc(hidden)]
#[macro_export]
macro_rules! __mark {
    (@go $cur:ident;) => { $crate::binding_schema::FieldMark::$cur };
    (@go Plain; custom $($r:tt)*) => { $crate::__mark!(@go Custom; $($r)*) };
    (@go Plain; skip $($r:tt)*) => { $crate::__mark!(@go Skip; $($r)*) };
    (@go $cur:ident; custom $($r:tt)*) => {
        compile_error!("binding_record!: a field takes one of `@[custom]` or `@[skip]`, once")
    };
    (@go $cur:ident; skip $($r:tt)*) => {
        compile_error!("binding_record!: a field takes one of `@[custom]` or `@[skip]`, once")
    };
    (@go $cur:ident; java($j:literal) $($r:tt)*) => { $crate::__mark!(@go $cur; $($r)*) };
    (@go $cur:ident; py_default($p:literal) $($r:tt)*) => { $crate::__mark!(@go $cur; $($r)*) };
    (@go $cur:ident; , $($r:tt)*) => { $crate::__mark!(@go $cur; $($r)*) };
    (@go $cur:ident; $x:tt $($r:tt)*) => {
        compile_error!(concat!(
            "binding_record!: unknown field mark `",
            stringify!($x),
            "`; a mark is `custom`, `skip`, `java(\"name\")` or `py_default(\"text\")`"
        ))
    };
    ($($t:tt)*) => { $crate::__mark!(@go Plain; $($t)*) };
}

/// The host-name override carried by a `java("…")` among the `@[...]` groups;
/// a second `java("…")` on one field is an error.
#[doc(hidden)]
#[macro_export]
macro_rules! __java {
    (@go;) => { ::core::option::Option::None };
    (@go [$j:literal];) => { ::core::option::Option::Some($j) };
    (@go; java($j:literal) $($r:tt)*) => { $crate::__java!(@go [$j]; $($r)*) };
    (@go [$j0:literal]; java($j:literal) $($r:tt)*) => {
        compile_error!("binding_record!: a field takes `@[java(\"name\")]` once")
    };
    (@go $([$j0:literal])?; $x:tt $($r:tt)*) => { $crate::__java!(@go $([$j0])?; $($r)*) };
    ($($t:tt)*) => { $crate::__java!(@go; $($t)*) };
}

/// The Python default text carried by a `py_default("…")` among the `@[...]`
/// groups; a second `py_default("…")` on one field is an error.
#[doc(hidden)]
#[macro_export]
macro_rules! __py_default {
    (@go;) => { ::core::option::Option::None };
    (@go [$p:literal];) => { ::core::option::Option::Some($p) };
    (@go; py_default($p:literal) $($r:tt)*) => { $crate::__py_default!(@go [$p]; $($r)*) };
    (@go [$p0:literal]; py_default($p:literal) $($r:tt)*) => {
        compile_error!("binding_record!: a field takes `@[py_default(\"text\")]` once")
    };
    (@go $([$p0:literal])?; $x:tt $($r:tt)*) => { $crate::__py_default!(@go $([$p0])?; $($r)*) };
    ($($t:tt)*) => { $crate::__py_default!(@go; $($t)*) };
}

/// Declares a record that crosses to a host language.
///
/// Emits the struct exactly as written (attributes, docs, visibility) and
/// an associated `BINDING_SCHEMA: RecordSchema`. A field's marks sit in
/// `@[...]` groups before it: `@[custom]` (hand-written converter), `@[skip]`
/// (does not cross), `@[java("name")]` (host name override) and
/// `@[py_default("text")]` (the Python source text of the dataclass default,
/// for a default the type does not imply). A field may carry several groups,
/// each mark at most once, and `custom` excludes `skip`:
///
/// ```
/// tst_core::binding_record! {
///     pub struct X {
///         @[custom] @[java("eCode")] pub e: Option<u8>,
///         @[py_default("\"x\"")] pub name: String,
///     }
/// }
/// ```
///
/// Each of these does not compile (the control above differs only in the
/// marks):
///
/// ```compile_fail
/// tst_core::binding_record! {
///     pub struct X {
///         @[nonsense] pub e: Option<u8>,
///     }
/// }
/// ```
///
/// ```compile_fail
/// tst_core::binding_record! {
///     pub struct X {
///         @[custom] @[skip] pub e: Option<u8>,
///     }
/// }
/// ```
///
/// ```compile_fail
/// tst_core::binding_record! {
///     pub struct X {
///         @[custom] @[custom] pub e: Option<u8>,
///     }
/// }
/// ```
///
/// ```compile_fail
/// tst_core::binding_record! {
///     pub struct X {
///         @[java("a")] @[java("b")] pub e: Option<u8>,
///     }
/// }
/// ```
///
/// ```compile_fail
/// tst_core::binding_record! {
///     pub struct X {
///         @[py_default("1")] @[py_default("2")] pub e: u8,
///     }
/// }
/// ```
#[macro_export]
macro_rules! binding_record {
    (
        $(#[$($meta:tt)*])*
        $vis:vis struct $name:ident {
            $(
                $(#[$($fmeta:tt)*])*
                $( @ [ $($mark:tt)* ] )*
                $fvis:vis $field:ident : $fty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$($meta)*])*
        $vis struct $name { $( $(#[$($fmeta)*])* $fvis $field : $fty ),* }
        impl $name {
            #[doc(hidden)]
            pub const BINDING_SCHEMA: $crate::binding_schema::RecordSchema =
                $crate::binding_schema::RecordSchema {
                    rust_path: concat!(module_path!(), "::", stringify!($name)),
                    name: stringify!($name),
                    non_exhaustive: $crate::__has_non_exhaustive!($(#[$($meta)*])*),
                    fields: &[ $( $crate::binding_schema::FieldSpec {
                        name: stringify!($field),
                        ty: stringify!($fty),
                        mark: $crate::__mark!($($($mark)*)*),
                        java: $crate::__java!($($($mark)*)*),
                        py_default: $crate::__py_default!($($($mark)*)*),
                        doc: $crate::__docs!($(#[$($fmeta)*])*),
                    } ),* ],
                    doc: $crate::__docs!($(#[$($meta)*])*),
                };
        }
    };
}
