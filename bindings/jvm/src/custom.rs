//! Hand-written converters for fields a generated record marks `@[custom]`.
//!
//! A custom field `<field>` of record `<Record>` is converted by a function in
//! this module named for the record and the field, in snake case. The
//! generator calls it and never writes it. See "Adding a crossing record" in
//! `docs/reference/binding-authors.md`.
