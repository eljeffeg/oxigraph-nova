//! SHACL Core validation for Oxigraph Nova.
//!
//! This crate provides:
//!
//! - The [`ShaclValidator`] trait seam — a pluggable "validate a data graph
//!   against a shapes graph" operation over any
//!   [`Dataset`](oxigraph_nova_query::Dataset), mirroring
//!   `oxigraph_nova_reasoning::ReasoningEngine`'s design.
//! - Nova-owned [`ValidationReport`]/[`ValidationResult`] types, modeled on
//!   the W3C SHACL `sh:` validation-report vocabulary, returned by every
//!   `ShaclValidator` implementation regardless of backend.
//! - [`NativeValidator`] — the default, always-available, zero-external-
//!   dependency `ShaclValidator` implementation (increment 1).
//!
//! ## Increment 1 scope
//!
//! **Targets:** `sh:targetNode`, `sh:targetClass` (RDFS-subclass-aware),
//! and the implicit class target (a shape that is itself `rdf:type
//! rdfs:Class`/`owl:Class` targets its own instances).
//!
//! **Constraints:** `sh:minCount`, `sh:maxCount`, `sh:datatype`,
//! `sh:nodeKind`, `sh:class` (RDFS-subclass-aware, matching Fluree's
//! `fluree-db-shacl` reference behavior), `sh:hasValue`, `sh:in`.
//!
//! Deferred to later increments: range/string/pair/language constraints,
//! logical constraints (`sh:not`/`sh:and`/`sh:or`/`sh:xone`),
//! `sh:closed`/`sh:ignoredProperties`, `sh:node`, qualified value shapes,
//! `sh:sparql`, full property-path expressions,
//! `sh:targetSubjectsOf`/`sh:targetObjectsOf`, `sh:deactivated`, and
//! `sh:message` overrides. Also deferred: a `rudof`-backed
//! `ShaclValidator` adapter (heavier dependency tree, full SHACL Core/SHACL-
//! SPARQL coverage), an `oxreason`-backed differential-testing oracle, and
//! the HTTP `/validate` endpoint + CLI `validate` subcommand that will
//! eventually consume [`ValidationReport::to_quads`].

pub mod native;
pub mod report;
pub mod shape;
pub mod validator;

pub use native::NativeValidator;
pub use report::{Severity, ValidationReport, ValidationResult};
pub use shape::{CompiledShape, Constraint, NodeKind, PropertyShape, Target, compile_shapes};
pub use validator::ShaclValidator;
