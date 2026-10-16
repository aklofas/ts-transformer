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
    /// The field's first doc line, `""` when it has none.
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
    /// The record's first doc line, `""` when it has none.
    pub doc: &'static str,
}

/// Strips the single leading space a `///` comment leaves in its doc text.
#[doc(hidden)]
pub const fn __doc_line(s: &'static str) -> &'static str {
    let b = s.as_bytes();
    if !b.is_empty() && b[0] == b' ' {
        let (_, rest) = b.split_at(1);
        match core::str::from_utf8(rest) {
            Ok(t) => t,
            Err(_) => s,
        }
    } else {
        s
    }
}

/// The text of the first `#[doc = "…"]` among the attributes, or `""`.
#[doc(hidden)]
#[macro_export]
macro_rules! __first_doc {
    () => { "" };
    (#[doc = $d:literal] $($rest:tt)*) => { $crate::binding_schema::__doc_line($d) };
    (#[$($x:tt)*] $($rest:tt)*) => { $crate::__first_doc!($($rest)*) };
}

/// `true` when `non_exhaustive` is among the attributes.
#[doc(hidden)]
#[macro_export]
macro_rules! __has_non_exhaustive {
    () => { false };
    (#[non_exhaustive] $($rest:tt)*) => { true };
    (#[$($x:tt)*] $($rest:tt)*) => { $crate::__has_non_exhaustive!($($rest)*) };
}

/// The [`FieldMark`] for a field's `@[...]` prefix. `@[java("…")]` carries no
/// mark of its own and is `Plain`.
#[doc(hidden)]
#[macro_export]
macro_rules! __mark {
    () => {
        $crate::binding_schema::FieldMark::Plain
    };
    (java($j:literal)) => {
        $crate::binding_schema::FieldMark::Plain
    };
    (custom) => {
        $crate::binding_schema::FieldMark::Custom
    };
    (skip) => {
        $crate::binding_schema::FieldMark::Skip
    };
    ($($other:tt)+) => {
        compile_error!(
            "binding_record!: a field mark is `@[custom]`, `@[skip]` or `@[java(\"name\")]`"
        )
    };
}

/// The host-name override carried by `@[java("…")]`.
#[doc(hidden)]
#[macro_export]
macro_rules! __java {
    (java($j:literal)) => {
        ::core::option::Option::Some($j)
    };
    ($($other:tt)*) => {
        ::core::option::Option::None
    };
}

/// Declares a record that crosses to a host language.
///
/// Emits the struct exactly as written (attributes, docs, visibility) and
/// an associated `BINDING_SCHEMA: RecordSchema`. A field may carry one
/// prefix: `@[custom]` (hand-written converter), `@[skip]` (does not cross) or
/// `@[java("name")]` (host name override).
#[macro_export]
macro_rules! binding_record {
    (
        $(#[$($meta:tt)*])*
        $vis:vis struct $name:ident {
            $(
                $(#[$($fmeta:tt)*])*
                $( @ [ $($mark:tt)* ] )?
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
                        mark: $crate::__mark!($($($mark)*)?),
                        java: $crate::__java!($($($mark)*)?),
                        doc: $crate::__first_doc!($(#[$($fmeta)*])*),
                    } ),* ],
                    doc: $crate::__first_doc!($(#[$($meta)*])*),
                };
        }
    };
}
