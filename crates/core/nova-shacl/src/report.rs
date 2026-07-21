//! Nova-owned [`ValidationReport`]/[`ValidationResult`] types, modeled on the
//! W3C SHACL `sh:` validation-report vocabulary
//! (<https://www.w3.org/TR/shacl/#results-validation-result>).
//!
//! These types are deliberately **not** a wrapper around any particular
//! backend's own report type (e.g. `rudof`'s or `oxreason`'s) — every
//! [`ShaclValidator`](crate::ShaclValidator) implementation returns exactly
//! this type, so callers never need to know which backend produced a
//! report.

use oxigraph_nova_core::{BlankNode, GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term};

/// Severity of a single [`ValidationResult`] — mirrors `sh:Violation` /
/// `sh:Warning` / `sh:Info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Violation,
    Warning,
    Info,
}

impl Severity {
    /// The `sh:*` individual IRI for this severity (used by
    /// [`ValidationReport::to_quads`] as the `sh:resultSeverity` object).
    pub fn iri(&self) -> NamedNode {
        let s = match self {
            Severity::Violation => "http://www.w3.org/ns/shacl#Violation",
            Severity::Warning => "http://www.w3.org/ns/shacl#Warning",
            Severity::Info => "http://www.w3.org/ns/shacl#Info",
        };
        NamedNode::new_unchecked(s)
    }
}

/// One failed constraint check — the Nova analog of a single `sh:result`
/// blank node in the SHACL validation-report vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationResult {
    /// `sh:focusNode` — the node that was being validated.
    pub focus_node: Term,
    /// `sh:resultPath` — the property path that was being validated, if
    /// this result came from a property shape (`None` for node-level
    /// constraints).
    ///
    /// Increment 1 only compiles single-predicate paths (see
    /// [`crate::shape`]'s module doc comment), so this is a plain
    /// [`NamedNode`] rather than a full property-path AST.
    pub path: Option<NamedNode>,
    /// `sh:sourceShape` — the shape node that declared the failing
    /// constraint.
    pub source_shape: Term,
    /// `sh:sourceConstraintComponent` — the exact `sh:*ConstraintComponent`
    /// IRI (e.g. `sh:MinCountConstraintComponent`) that failed.
    pub source_constraint_component: NamedNode,
    /// `sh:resultSeverity`.
    pub severity: Severity,
    /// `sh:resultMessage` — a human-readable description.
    pub message: String,
    /// `sh:value` — the offending value, if this result is about a specific
    /// value in the value set (absent for e.g. `sh:minCount`/`sh:maxCount`,
    /// which are about the value set's cardinality rather than any single
    /// value).
    pub value: Option<Term>,
}

impl ValidationResult {
    /// `true` if this result reports an actual SHACL violation ([`Severity::Violation`]).
    pub fn is_violation(&self) -> bool {
        self.severity == Severity::Violation
    }
}

/// The result of validating a data graph against a shapes graph.
///
/// Per the SHACL spec, `conforms` is `true` **iff `results` is empty** —
/// `sh:Warning`/`sh:Info` results also count against conformance, not just
/// `sh:Violation`s (see the W3C spec's definition of `sh:conforms`, and
/// Fluree's `fluree-db-shacl` reference implementation, which encodes the
/// exact same rule).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    pub conforms: bool,
    pub results: Vec<ValidationResult>,
}

impl ValidationReport {
    /// Build a report from a result set, computing `conforms` per the SHACL
    /// spec rule (`results.is_empty()`).
    pub fn new(results: Vec<ValidationResult>) -> Self {
        Self {
            conforms: results.is_empty(),
            results,
        }
    }

    /// An empty, conforming report — the common "nothing failed" case.
    pub fn conforming() -> Self {
        Self {
            conforms: true,
            results: Vec::new(),
        }
    }

    /// Number of [`Severity::Violation`] results.
    pub fn violation_count(&self) -> usize {
        self.results.iter().filter(|r| r.is_violation()).count()
    }

    /// Number of [`Severity::Warning`] results.
    pub fn warning_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.severity == Severity::Warning)
            .count()
    }

    /// Serialize this report to the `sh:` validation-report RDF vocabulary,
    /// as a set of quads in `graph`.
    ///
    /// Produces:
    /// ```turtle
    /// [] a sh:ValidationReport ;
    ///    sh:conforms true/false ;
    ///    sh:result [
    ///        a sh:ValidationResult ;
    ///        sh:focusNode ... ;
    ///        sh:resultPath ... ;
    ///        sh:sourceShape ... ;
    ///        sh:sourceConstraintComponent ... ;
    ///        sh:resultSeverity ... ;
    ///        sh:resultMessage "..." ;
    ///        sh:value ... ;
    ///    ] , [ ... ] , ... .
    /// ```
    pub fn to_quads(&self, graph: GraphName) -> Vec<Quad> {
        let sh_validation_report =
            NamedNode::new_unchecked("http://www.w3.org/ns/shacl#ValidationReport");
        let sh_validation_result =
            NamedNode::new_unchecked("http://www.w3.org/ns/shacl#ValidationResult");
        let rdf_type = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
        let sh_conforms = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#conforms");
        let sh_result = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#result");
        let sh_focus_node = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#focusNode");
        let sh_result_path = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#resultPath");
        let sh_source_shape = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#sourceShape");
        let sh_source_constraint_component =
            NamedNode::new_unchecked("http://www.w3.org/ns/shacl#sourceConstraintComponent");
        let sh_result_severity =
            NamedNode::new_unchecked("http://www.w3.org/ns/shacl#resultSeverity");
        let sh_result_message =
            NamedNode::new_unchecked("http://www.w3.org/ns/shacl#resultMessage");
        let sh_value = NamedNode::new_unchecked("http://www.w3.org/ns/shacl#value");

        let mut quads = Vec::new();
        let report_node = NamedOrBlankNode::BlankNode(BlankNode::default());

        quads.push(Quad::new(
            report_node.clone(),
            rdf_type.clone(),
            Term::NamedNode(sh_validation_report),
            graph.clone(),
        ));
        quads.push(Quad::new(
            report_node.clone(),
            sh_conforms,
            Term::Literal(Literal::new_typed_literal(
                if self.conforms { "true" } else { "false" },
                NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#boolean"),
            )),
            graph.clone(),
        ));

        for result in &self.results {
            let result_node = NamedOrBlankNode::BlankNode(BlankNode::default());
            quads.push(Quad::new(
                report_node.clone(),
                sh_result.clone(),
                Term::from(result_node.clone()),
                graph.clone(),
            ));
            quads.push(Quad::new(
                result_node.clone(),
                rdf_type.clone(),
                Term::NamedNode(sh_validation_result.clone()),
                graph.clone(),
            ));
            quads.push(Quad::new(
                result_node.clone(),
                sh_focus_node.clone(),
                result.focus_node.clone(),
                graph.clone(),
            ));
            if let Some(path) = &result.path {
                quads.push(Quad::new(
                    result_node.clone(),
                    sh_result_path.clone(),
                    Term::NamedNode(path.clone()),
                    graph.clone(),
                ));
            }
            quads.push(Quad::new(
                result_node.clone(),
                sh_source_shape.clone(),
                result.source_shape.clone(),
                graph.clone(),
            ));
            quads.push(Quad::new(
                result_node.clone(),
                sh_source_constraint_component.clone(),
                Term::NamedNode(result.source_constraint_component.clone()),
                graph.clone(),
            ));
            quads.push(Quad::new(
                result_node.clone(),
                sh_result_severity.clone(),
                Term::NamedNode(result.severity.iri()),
                graph.clone(),
            ));
            quads.push(Quad::new(
                result_node.clone(),
                sh_result_message.clone(),
                Term::Literal(Literal::new_simple_literal(&result.message)),
                graph.clone(),
            ));
            if let Some(value) = &result.value {
                quads.push(Quad::new(
                    result_node.clone(),
                    sh_value.clone(),
                    value.clone(),
                    graph.clone(),
                ));
            }
        }

        quads
    }
}
