//! [`NativeValidator`] — the default, always-available [`ShaclValidator`]
//! implementation.
//!
//! Zero external dependencies beyond `oxigraph-nova-core`/
//! `oxigraph-nova-query`: it compiles the shapes graph via [`crate::shape`]
//! and evaluates every compiled shape's targets/constraints purely through
//! [`Dataset::find_quads`], the same storage-agnostic seam the SPARQL
//! evaluator itself uses. This mirrors
//! `oxigraph_nova_reasoning::LftjFixpointEngine`'s role as the default,
//! always-available `ReasoningEngine` — a heavier backend (e.g. a `rudof`
//! adapter) can be added later as an alternative `ShaclValidator`
//! implementation without this one changing.
//!
//! See [`crate::shape`]'s module doc comment for the exact target/
//! constraint coverage this validator supports in increment 1.

use crate::report::{Severity, ValidationReport, ValidationResult};
use crate::shape::{CompiledShape, Constraint, Target, compile_shapes};
use crate::validator::ShaclValidator;
use anyhow::Result;
use oxigraph_nova_core::{NamedNode, Quad, Term};
use oxigraph_nova_query::{Dataset, GraphSelector, PatternTerm, QuadPattern};
use std::collections::{HashSet, VecDeque};

fn rdf_type() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
}

fn rdfs_sub_class_of() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#subClassOf")
}

/// The default SHACL Core validator — see this module's doc comment.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeValidator;

impl NativeValidator {
    pub fn new() -> Self {
        Self
    }
}

impl ShaclValidator for NativeValidator {
    fn validate(&self, shapes: &[Quad], data: &dyn Dataset) -> Result<ValidationReport> {
        let compiled = compile_shapes(shapes);
        let mut results = Vec::new();

        for shape in &compiled {
            let focus_nodes = resolve_targets(shape, data)?;
            for focus in &focus_nodes {
                validate_node_constraints(
                    shape,
                    focus,
                    &shape.node_constraints,
                    None,
                    data,
                    &mut results,
                )?;
                for prop in &shape.property_shapes {
                    let values = property_values(data, focus, &prop.path)?;
                    validate_node_constraints(
                        shape,
                        focus,
                        &prop.constraints,
                        Some((&prop.path, &values)),
                        data,
                        &mut results,
                    )?;
                }
            }
        }

        Ok(ValidationReport::new(results))
    }
}

/// Resolve every focus node for `shape`'s targets, deduplicated.
fn resolve_targets(shape: &CompiledShape, data: &dyn Dataset) -> Result<Vec<Term>> {
    let mut seen = HashSet::new();
    let mut focus_nodes = Vec::new();

    for target in &shape.targets {
        match target {
            Target::Node(t) => {
                if seen.insert(t.clone()) {
                    focus_nodes.push(t.clone());
                }
            }
            Target::Class(class) | Target::ImplicitClass(class) => {
                for descendant in subclass_closure(data, class)? {
                    for instance in instances_of(data, &descendant)? {
                        if seen.insert(instance.clone()) {
                            focus_nodes.push(instance);
                        }
                    }
                }
            }
        }
    }

    Ok(focus_nodes)
}

/// `class` plus every class `?d` such that `?d rdfs:subClassOf+ class`
/// (i.e. every subclass, transitively), via BFS over the reverse edge.
fn subclass_closure(data: &dyn Dataset, class: &NamedNode) -> Result<Vec<NamedNode>> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut result = Vec::new();

    visited.insert(class.clone());
    queue.push_back(class.clone());
    result.push(class.clone());

    while let Some(current) = queue.pop_front() {
        // Find every ?sub such that ?sub rdfs:subClassOf current.
        let pattern = QuadPattern {
            subject: PatternTerm::Variable,
            predicate: PatternTerm::Bound(Term::NamedNode(rdfs_sub_class_of())),
            object: PatternTerm::Bound(Term::NamedNode(current.clone())),
            graph: GraphSelector::Union,
        };
        for m in data.find_quads(&pattern)? {
            let m = m?;
            if let Term::NamedNode(sub) = m.subject
                && visited.insert(sub.clone())
            {
                result.push(sub.clone());
                queue.push_back(sub);
            }
        }
    }

    Ok(result)
}

/// Every `?x` such that `?x rdf:type class` (exact match only — subclass
/// expansion is handled by iterating [`subclass_closure`]'s results).
fn instances_of(data: &dyn Dataset, class: &NamedNode) -> Result<Vec<Term>> {
    let pattern = QuadPattern {
        subject: PatternTerm::Variable,
        predicate: PatternTerm::Bound(Term::NamedNode(rdf_type())),
        object: PatternTerm::Bound(Term::NamedNode(class.clone())),
        graph: GraphSelector::Union,
    };
    let mut out = Vec::new();
    for m in data.find_quads(&pattern)? {
        out.push(m?.subject);
    }
    Ok(out)
}

/// Every value of `focus path` in `data` (single-predicate path only — see
/// `shape.rs`'s module doc comment).
fn property_values(data: &dyn Dataset, focus: &Term, path: &NamedNode) -> Result<Vec<Term>> {
    let pattern = QuadPattern {
        subject: PatternTerm::Bound(focus.clone()),
        predicate: PatternTerm::Bound(Term::NamedNode(path.clone())),
        object: PatternTerm::Variable,
        graph: GraphSelector::Union,
    };
    let mut out = Vec::new();
    for m in data.find_quads(&pattern)? {
        out.push(m?.object);
    }
    Ok(out)
}

/// `true` if `value`'s asserted `rdf:type` is `class` or an
/// `rdfs:subClassOf`-descendant of `class` — RDFS-subclass-aware `sh:class`
/// semantics, matching Fluree's `fluree-db-shacl` reference behavior (see
/// `shape.rs`'s module doc comment).
fn satisfies_class(data: &dyn Dataset, value: &Term, class: &NamedNode) -> Result<bool> {
    let pattern = QuadPattern {
        subject: PatternTerm::Bound(value.clone()),
        predicate: PatternTerm::Bound(Term::NamedNode(rdf_type())),
        object: PatternTerm::Variable,
        graph: GraphSelector::Union,
    };
    let descendants: HashSet<NamedNode> = subclass_closure(data, class)?.into_iter().collect();
    for m in data.find_quads(&pattern)? {
        let m = m?;
        if let Term::NamedNode(asserted) = m.object
            && descendants.contains(&asserted)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Evaluate `constraints` against either the focus node itself
/// (`values_context = None`, node-level constraints) or a property's value
/// set (`values_context = Some((path, values))`), pushing a
/// [`ValidationResult`] for every failure.
fn validate_node_constraints(
    shape: &CompiledShape,
    focus: &Term,
    constraints: &[Constraint],
    values_context: Option<(&NamedNode, &[Term])>,
    data: &dyn Dataset,
    results: &mut Vec<ValidationResult>,
) -> Result<()> {
    // The "value set" a constraint applies to: either the focus node itself
    // (a single-element set) for node-level constraints, or a property's
    // resolved values.
    let (path, values): (Option<NamedNode>, Vec<Term>) = match values_context {
        Some((p, vs)) => (Some(p.clone()), vs.to_vec()),
        None => (None, vec![focus.clone()]),
    };

    let push = |results: &mut Vec<ValidationResult>,
                constraint: &Constraint,
                message: String,
                value: Option<Term>| {
        results.push(ValidationResult {
            focus_node: focus.clone(),
            path: path.clone(),
            source_shape: shape.id.clone(),
            source_constraint_component: constraint.component(),
            severity: Severity::Violation,
            message,
            value,
        });
    };

    for constraint in constraints {
        match constraint {
            Constraint::MinCount(min) => {
                if (values.len() as u64) < *min {
                    push(
                        results,
                        constraint,
                        format!("expected at least {min} value(s), found {}", values.len()),
                        None,
                    );
                }
            }
            Constraint::MaxCount(max) => {
                if (values.len() as u64) > *max {
                    push(
                        results,
                        constraint,
                        format!("expected at most {max} value(s), found {}", values.len()),
                        None,
                    );
                }
            }
            Constraint::Datatype(dt) => {
                for v in &values {
                    let ok = matches!(v, Term::Literal(lit) if lit.datatype() == dt.as_ref());
                    if !ok {
                        push(
                            results,
                            constraint,
                            format!("value does not have datatype {}", dt.as_str()),
                            Some(v.clone()),
                        );
                    }
                }
            }
            Constraint::NodeKind(nk) => {
                for v in &values {
                    if !nk.matches(v) {
                        push(
                            results,
                            constraint,
                            format!("value does not match node kind {nk:?}"),
                            Some(v.clone()),
                        );
                    }
                }
            }
            Constraint::Class(class) => {
                for v in &values {
                    if !satisfies_class(data, v, class)? {
                        push(
                            results,
                            constraint,
                            format!(
                                "value is not an instance of {} (or a subclass thereof)",
                                class.as_str()
                            ),
                            Some(v.clone()),
                        );
                    }
                }
            }
            Constraint::HasValue(expected) => {
                if !values.contains(expected) {
                    push(
                        results,
                        constraint,
                        format!("value set does not contain required value {expected}"),
                        None,
                    );
                }
            }
            Constraint::In(allowed) => {
                for v in &values {
                    if !allowed.contains(v) {
                        push(
                            results,
                            constraint,
                            "value is not in the sh:in list".to_string(),
                            Some(v.clone()),
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{
        BlankNode, GraphName as CoreGraphName, Literal, NamedOrBlankNode, QuadStore,
    };
    use oxigraph_nova_engine_ring::LoudsStore;
    use oxigraph_nova_query::StoreDataset;
    use std::sync::Arc;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }
    fn sh(local: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl#{local}"))
    }
    fn shq(s: NamedOrBlankNode, p: NamedNode, o: Term) -> Quad {
        Quad::new(s, p, o, CoreGraphName::DefaultGraph)
    }

    fn build_store(quads: Vec<Quad>) -> LoudsStore {
        let store = LoudsStore::new();
        for q in quads {
            store.insert(&q).unwrap();
        }
        store.compact().unwrap();
        store
    }

    /// A minimal `PersonShape`: `sh:targetClass ex:Person`, one property
    /// shape on `ex:name` with `sh:minCount 1`.
    fn person_shape_min_count_1() -> Vec<Quad> {
        let shape = nn("http://ex/PersonShape");
        let prop = BlankNode::new_unchecked("prop1");
        vec![
            shq(
                shape.clone().into(),
                rdf_type(),
                Term::NamedNode(sh("NodeShape")),
            ),
            shq(
                shape.clone().into(),
                sh("targetClass"),
                Term::NamedNode(nn("http://ex/Person")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/name")),
            ),
            shq(
                prop.into(),
                sh("minCount"),
                Term::Literal(Literal::new_typed_literal(
                    "1",
                    nn("http://www.w3.org/2001/XMLSchema#integer"),
                )),
            ),
        ]
    }

    #[test]
    fn conforms_when_min_count_satisfied() {
        let quads = vec![
            Quad::new(
                nn("http://ex/alice"),
                rdf_type(),
                Term::NamedNode(nn("http://ex/Person")),
                CoreGraphName::DefaultGraph,
            ),
            Quad::new(
                nn("http://ex/alice"),
                nn("http://ex/name"),
                Term::Literal(Literal::new_simple_literal("Alice")),
                CoreGraphName::DefaultGraph,
            ),
        ];
        let store = build_store(quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator
            .validate(&person_shape_min_count_1(), &dataset)
            .unwrap();
        assert!(report.conforms, "expected conforming report: {report:?}");
        assert!(report.results.is_empty());
    }

    #[test]
    fn violates_when_min_count_not_satisfied() {
        let quads = vec![Quad::new(
            nn("http://ex/bob"),
            rdf_type(),
            Term::NamedNode(nn("http://ex/Person")),
            CoreGraphName::DefaultGraph,
        )];
        let store = build_store(quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator
            .validate(&person_shape_min_count_1(), &dataset)
            .unwrap();
        assert!(!report.conforms);
        assert_eq!(report.violation_count(), 1);
        assert_eq!(
            report.results[0].focus_node,
            Term::NamedNode(nn("http://ex/bob"))
        );
        assert_eq!(
            report.results[0].source_constraint_component,
            sh("MinCountConstraintComponent")
        );
    }

    #[test]
    fn max_count_violation() {
        let shape = nn("http://ex/S");
        let prop = BlankNode::new_unchecked("p1");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/alice")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/nick")),
            ),
            shq(
                prop.into(),
                sh("maxCount"),
                Term::Literal(Literal::new_typed_literal(
                    "1",
                    nn("http://www.w3.org/2001/XMLSchema#integer"),
                )),
            ),
        ];
        let data_quads = vec![
            Quad::new(
                nn("http://ex/alice"),
                nn("http://ex/nick"),
                Term::Literal(Literal::new_simple_literal("Al")),
                CoreGraphName::DefaultGraph,
            ),
            Quad::new(
                nn("http://ex/alice"),
                nn("http://ex/nick"),
                Term::Literal(Literal::new_simple_literal("Ally")),
                CoreGraphName::DefaultGraph,
            ),
        ];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(!report.conforms);
        assert_eq!(report.violation_count(), 1);
    }

    #[test]
    fn datatype_violation() {
        let shape = nn("http://ex/S");
        let prop = BlankNode::new_unchecked("p1");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/alice")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/age")),
            ),
            shq(
                prop.into(),
                sh("datatype"),
                Term::NamedNode(nn("http://www.w3.org/2001/XMLSchema#integer")),
            ),
        ];
        let data_quads = vec![Quad::new(
            nn("http://ex/alice"),
            nn("http://ex/age"),
            Term::Literal(Literal::new_simple_literal("thirty")),
            CoreGraphName::DefaultGraph,
        )];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(!report.conforms);
        assert_eq!(
            report.results[0].source_constraint_component,
            sh("DatatypeConstraintComponent")
        );
    }

    #[test]
    fn class_constraint_is_rdfs_subclass_aware() {
        // ex:Dog rdfs:subClassOf ex:Animal.
        // ex:fido rdf:type ex:Dog.
        // shape: targetNode ex:fido, sh:class ex:Animal (node-level).
        let shape = nn("http://ex/S");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/fido")),
            ),
            shq(
                shape.into(),
                sh("class"),
                Term::NamedNode(nn("http://ex/Animal")),
            ),
        ];
        let data_quads = vec![
            Quad::new(
                nn("http://ex/Dog"),
                rdfs_sub_class_of(),
                Term::NamedNode(nn("http://ex/Animal")),
                CoreGraphName::DefaultGraph,
            ),
            Quad::new(
                nn("http://ex/fido"),
                rdf_type(),
                Term::NamedNode(nn("http://ex/Dog")),
                CoreGraphName::DefaultGraph,
            ),
        ];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(
            report.conforms,
            "fido is a Dog, a subclass of Animal — sh:class ex:Animal must conform: {report:?}"
        );
    }

    #[test]
    fn target_class_is_rdfs_subclass_aware() {
        // ex:Dog rdfs:subClassOf ex:Animal. ex:fido rdf:type ex:Dog.
        // shape: targetClass ex:Animal, sh:minCount 1 on ex:name -> fido must be checked
        // (and must fail, since it has no name).
        let shape = nn("http://ex/AnimalShape");
        let prop = BlankNode::new_unchecked("p1");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetClass"),
                Term::NamedNode(nn("http://ex/Animal")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/name")),
            ),
            shq(
                prop.into(),
                sh("minCount"),
                Term::Literal(Literal::new_typed_literal(
                    "1",
                    nn("http://www.w3.org/2001/XMLSchema#integer"),
                )),
            ),
        ];
        let data_quads = vec![
            Quad::new(
                nn("http://ex/Dog"),
                rdfs_sub_class_of(),
                Term::NamedNode(nn("http://ex/Animal")),
                CoreGraphName::DefaultGraph,
            ),
            Quad::new(
                nn("http://ex/fido"),
                rdf_type(),
                Term::NamedNode(nn("http://ex/Dog")),
                CoreGraphName::DefaultGraph,
            ),
        ];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(
            !report.conforms,
            "fido (a Dog, subclass of targeted Animal) has no ex:name and must violate minCount"
        );
        assert_eq!(
            report.results[0].focus_node,
            Term::NamedNode(nn("http://ex/fido"))
        );
    }

    #[test]
    fn has_value_constraint() {
        let shape = nn("http://ex/S");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/alice")),
            ),
            shq(
                shape.into(),
                sh("hasValue"),
                Term::NamedNode(nn("http://ex/Employee")),
            ),
        ];
        // Node-level sh:hasValue applies to the focus node itself, which is
        // an IRI, not equal to ex:Employee — expect violation.
        let store = build_store(vec![]);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(!report.conforms);
    }

    #[test]
    fn in_constraint_on_property() {
        let shape = nn("http://ex/S");
        let prop = BlankNode::new_unchecked("p1");
        let l1 = BlankNode::new_unchecked("l1");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/alice")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/status")),
            ),
            shq(prop.into(), sh("in"), Term::BlankNode(l1.clone())),
            shq(
                l1.clone().into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#first"),
                Term::Literal(Literal::new_simple_literal("active")),
            ),
            shq(
                l1.into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest"),
                Term::NamedNode(nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#nil")),
            ),
        ];
        let data_quads = vec![Quad::new(
            nn("http://ex/alice"),
            nn("http://ex/status"),
            Term::Literal(Literal::new_simple_literal("inactive")),
            CoreGraphName::DefaultGraph,
        )];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(!report.conforms);
        assert_eq!(
            report.results[0].source_constraint_component,
            sh("InConstraintComponent")
        );
    }

    #[test]
    fn node_kind_violation() {
        let shape = nn("http://ex/S");
        let prop = BlankNode::new_unchecked("p1");
        let shapes = vec![
            shq(
                shape.clone().into(),
                sh("targetNode"),
                Term::NamedNode(nn("http://ex/alice")),
            ),
            shq(shape.into(), sh("property"), Term::BlankNode(prop.clone())),
            shq(
                prop.clone().into(),
                sh("path"),
                Term::NamedNode(nn("http://ex/friend")),
            ),
            shq(prop.into(), sh("nodeKind"), Term::NamedNode(sh("IRI"))),
        ];
        let data_quads = vec![Quad::new(
            nn("http://ex/alice"),
            nn("http://ex/friend"),
            Term::Literal(Literal::new_simple_literal("not an IRI")),
            CoreGraphName::DefaultGraph,
        )];
        let store = build_store(data_quads);
        let dataset = StoreDataset::new(Arc::new(store));
        let validator = NativeValidator::new();
        let report = validator.validate(&shapes, &dataset).unwrap();
        assert!(!report.conforms);
        assert_eq!(
            report.results[0].source_constraint_component,
            sh("NodeKindConstraintComponent")
        );
    }
}
