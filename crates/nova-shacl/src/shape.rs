//! Compiles a shapes graph (`&[Quad]`) into an in-memory [`CompiledShape`]
//! model that [`crate::native::NativeValidator`] evaluates against a
//! [`Dataset`](oxigraph_nova_query::Dataset).
//!
//! ## Scope (increment 1)
//!
//! Only single-predicate `sh:path` values are compiled (`sh:path ex:foo`) —
//! full property-path expressions (`sh:inversePath`, sequence paths,
//! `sh:alternativePath`, `sh:zeroOrMorePath`, etc.) are out of scope for
//! this increment; a `sh:property` shape whose `sh:path` is not a single
//! IRI is skipped (with no error — see [`compile_shapes`]'s doc comment).
//!
//! Constraint coverage compiled: `sh:minCount`, `sh:maxCount`,
//! `sh:datatype`, `sh:nodeKind`, `sh:class`, `sh:hasValue`, `sh:in`. Range
//! constraints, string constraints, pair constraints, language constraints,
//! logical constraints, `sh:closed`, `sh:node`, qualified value shapes and
//! `sh:sparql` are all deferred.
//!
//! ## Target resolution
//!
//! `sh:targetNode`, `sh:targetClass`, and the *implicit class target*
//! (a shape that is itself asserted `rdf:type rdfs:Class`/`owl:Class`
//! targets its own instances — SHACL spec §2.1.3.1) are compiled. Both
//! `sh:targetClass` and the implicit class target are RDFS-subclass-aware
//! at evaluation time (see `native.rs`): an instance of a subclass of the
//! target class is in-scope, matching Fluree's `fluree-db-shacl` reference
//! behavior. `sh:targetSubjectsOf`/`sh:targetObjectsOf` are deferred.

use oxigraph_nova_core::{NamedNode, Quad, Term};
use std::collections::HashMap;

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl#{local}"))
}

fn rdf_type() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
}

fn rdfs_class() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2000/01/rdf-schema#Class")
}

fn owl_class() -> NamedNode {
    NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#Class")
}

/// A single SHACL Core target declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// `sh:targetNode` — a single, specific focus node.
    Node(Term),
    /// `sh:targetClass` — every instance of this class (and, at evaluation
    /// time, every instance of an `rdfs:subClassOf` descendant of it).
    Class(NamedNode),
    /// Implicit class target: the shape IRI is itself `rdf:type
    /// rdfs:Class`/`owl:Class`, so it targets its own instances.
    ImplicitClass(NamedNode),
}

/// `sh:IRI` / `sh:BlankNode` / `sh:Literal` / `sh:BlankNodeOrIRI` /
/// `sh:BlankNodeOrLiteral` / `sh:IRIOrLiteral`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Iri,
    BlankNode,
    Literal,
    BlankNodeOrIri,
    BlankNodeOrLiteral,
    IriOrLiteral,
}

impl NodeKind {
    fn from_term(t: &Term) -> Option<Self> {
        let iri = match t {
            Term::NamedNode(n) => n.as_str(),
            _ => return None,
        };
        Some(match iri {
            "http://www.w3.org/ns/shacl#IRI" => NodeKind::Iri,
            "http://www.w3.org/ns/shacl#BlankNode" => NodeKind::BlankNode,
            "http://www.w3.org/ns/shacl#Literal" => NodeKind::Literal,
            "http://www.w3.org/ns/shacl#BlankNodeOrIRI" => NodeKind::BlankNodeOrIri,
            "http://www.w3.org/ns/shacl#BlankNodeOrLiteral" => NodeKind::BlankNodeOrLiteral,
            "http://www.w3.org/ns/shacl#IRIOrLiteral" => NodeKind::IriOrLiteral,
            _ => return None,
        })
    }

    pub fn matches(&self, term: &Term) -> bool {
        let is_iri = matches!(term, Term::NamedNode(_));
        let is_bnode = matches!(term, Term::BlankNode(_));
        let is_literal = matches!(term, Term::Literal(_));
        match self {
            NodeKind::Iri => is_iri,
            NodeKind::BlankNode => is_bnode,
            NodeKind::Literal => is_literal,
            NodeKind::BlankNodeOrIri => is_iri || is_bnode,
            NodeKind::BlankNodeOrLiteral => is_bnode || is_literal,
            NodeKind::IriOrLiteral => is_iri || is_literal,
        }
    }
}

/// One SHACL Core constraint, compiled from its declaring predicate/object.
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    MinCount(u64),
    MaxCount(u64),
    Datatype(NamedNode),
    NodeKind(NodeKind),
    /// `sh:class` — RDFS-subclass-aware at evaluation time (see
    /// `native.rs`): a value satisfies `sh:class ex:Foo` if its asserted
    /// `rdf:type` is `ex:Foo` or a subclass of `ex:Foo`.
    Class(NamedNode),
    HasValue(Term),
    In(Vec<Term>),
}

impl Constraint {
    /// The exact `sh:*ConstraintComponent` IRI for `sh:sourceConstraintComponent`.
    pub fn component(&self) -> NamedNode {
        let local = match self {
            Constraint::MinCount(_) => "MinCountConstraintComponent",
            Constraint::MaxCount(_) => "MaxCountConstraintComponent",
            Constraint::Datatype(_) => "DatatypeConstraintComponent",
            Constraint::NodeKind(_) => "NodeKindConstraintComponent",
            Constraint::Class(_) => "ClassConstraintComponent",
            Constraint::HasValue(_) => "HasValueConstraintComponent",
            Constraint::In(_) => "InConstraintComponent",
        };
        sh(local)
    }
}

/// A compiled `sh:property` shape: a single-predicate path plus its
/// constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyShape {
    pub path: NamedNode,
    pub constraints: Vec<Constraint>,
}

/// A compiled `sh:NodeShape`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledShape {
    /// The shape's own subject node (`sh:sourceShape` in results).
    pub id: Term,
    pub targets: Vec<Target>,
    /// Constraints declared directly on the node shape (apply to the focus
    /// node itself, not to a property's values).
    pub node_constraints: Vec<Constraint>,
    pub property_shapes: Vec<PropertyShape>,
}

/// Simple `(subject, predicate) -> Vec<object>` index over a shapes graph,
/// used only during compilation (not by the validator's hot path, which
/// queries the *data* graph via [`Dataset`](oxigraph_nova_query::Dataset)).
struct ShapesIndex {
    by_subject_predicate: HashMap<(Term, NamedNode), Vec<Term>>,
}

impl ShapesIndex {
    fn build(shapes: &[Quad]) -> Self {
        let mut by_subject_predicate: HashMap<(Term, NamedNode), Vec<Term>> = HashMap::new();
        for q in shapes {
            let subject = Term::from(q.subject.clone());
            by_subject_predicate
                .entry((subject, q.predicate.clone()))
                .or_default()
                .push(q.object.clone());
        }
        Self {
            by_subject_predicate,
        }
    }

    fn get(&self, subject: &Term, predicate: &NamedNode) -> &[Term] {
        self.by_subject_predicate
            .get(&(subject.clone(), predicate.clone()))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn get_one(&self, subject: &Term, predicate: &NamedNode) -> Option<&Term> {
        self.get(subject, predicate).first()
    }

    /// All distinct subjects appearing anywhere in the shapes graph.
    fn all_subjects(&self) -> impl Iterator<Item = &Term> {
        self.by_subject_predicate.keys().map(|(s, _)| s)
    }
}

fn compile_constraints(index: &ShapesIndex, subject: &Term) -> Vec<Constraint> {
    let mut constraints = Vec::new();

    if let Some(Term::Literal(lit)) = index.get_one(subject, &sh("minCount"))
        && let Ok(n) = lit.value().parse::<u64>()
    {
        constraints.push(Constraint::MinCount(n));
    }
    if let Some(Term::Literal(lit)) = index.get_one(subject, &sh("maxCount"))
        && let Ok(n) = lit.value().parse::<u64>()
    {
        constraints.push(Constraint::MaxCount(n));
    }
    if let Some(Term::NamedNode(dt)) = index.get_one(subject, &sh("datatype")) {
        constraints.push(Constraint::Datatype(dt.clone()));
    }
    if let Some(nk_term) = index.get_one(subject, &sh("nodeKind"))
        && let Some(nk) = NodeKind::from_term(nk_term)
    {
        constraints.push(Constraint::NodeKind(nk));
    }
    if let Some(Term::NamedNode(class)) = index.get_one(subject, &sh("class")) {
        constraints.push(Constraint::Class(class.clone()));
    }
    if let Some(value) = index.get_one(subject, &sh("hasValue")) {
        constraints.push(Constraint::HasValue(value.clone()));
    }
    // sh:in ?list — the list is an RDF list (rdf:first/rdf:rest chain).
    if let Some(list_head) = index.get_one(subject, &sh("in")) {
        let items = collect_rdf_list(index, list_head);
        if !items.is_empty() {
            constraints.push(Constraint::In(items));
        }
    }

    constraints
}

fn collect_rdf_list(index: &ShapesIndex, head: &Term) -> Vec<Term> {
    let rdf_first = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#first");
    let rdf_rest = NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest");
    let rdf_nil = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

    let mut items = Vec::new();
    let mut current = head.clone();
    // Guard against malformed cyclic lists.
    let mut guard = 0usize;
    loop {
        guard += 1;
        if guard > 10_000 {
            break;
        }
        if let Term::NamedNode(n) = &current
            && n.as_str() == rdf_nil
        {
            break;
        }
        let Some(first) = index.get_one(&current, &rdf_first) else {
            break;
        };
        items.push(first.clone());
        let Some(rest) = index.get_one(&current, &rdf_rest) else {
            break;
        };
        current = rest.clone();
    }
    items
}

/// Compile every `sh:NodeShape`-like subject in `shapes` into a
/// [`CompiledShape`].
///
/// A subject is recognized as a shape if it has at least one
/// shape-declaring predicate (`sh:targetNode`, `sh:targetClass`,
/// `sh:property`, or any of the constraint predicates listed in this
/// module's doc comment), or is explicitly typed `sh:NodeShape`. Subjects
/// with no recognizable shape-declaring predicate are silently ignored
/// (not an error) — this increment does not validate the shapes graph
/// itself.
///
/// `sh:property` objects whose `sh:path` is missing or is not a single
/// `NamedNode` are skipped (no full property-path AST support yet — see
/// this module's doc comment).
pub fn compile_shapes(shapes: &[Quad]) -> Vec<CompiledShape> {
    let index = ShapesIndex::build(shapes);
    let sh_node_shape = sh("NodeShape");
    let sh_target_node = sh("targetNode");
    let sh_target_class = sh("targetClass");
    let sh_property = sh("property");
    let sh_path = sh("path");

    let shape_predicates = [
        sh_target_node.clone(),
        sh_target_class.clone(),
        sh_property.clone(),
        sh("minCount"),
        sh("maxCount"),
        sh("datatype"),
        sh("nodeKind"),
        sh("class"),
        sh("hasValue"),
        sh("in"),
    ];

    // Every node that appears as the object of a `sh:property` anywhere in
    // the shapes graph is a nested property shape, not an independent
    // top-level shape -- even though it has its own constraint predicates
    // (e.g. `sh:minCount`) directly on it. Exclude these from the top-level
    // shape-subject scan so they're only compiled once, as a
    // `PropertyShape` nested under their owning `CompiledShape`.
    let mut property_shape_nodes = std::collections::HashSet::new();
    for q in shapes {
        if q.predicate == sh_property {
            property_shape_nodes.insert(q.object.clone());
        }
    }

    let mut shape_subjects: Vec<Term> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for subject in index.all_subjects() {
        if !seen.insert(subject.clone()) {
            continue;
        }
        if property_shape_nodes.contains(subject) {
            continue;
        }
        let is_shape = index
            .get(subject, &rdf_type())
            .iter()
            .any(|t| *t == Term::NamedNode(sh_node_shape.clone()))
            || shape_predicates
                .iter()
                .any(|p| !index.get(subject, p).is_empty());
        if is_shape {
            shape_subjects.push(subject.clone());
        }
    }

    let mut compiled = Vec::new();
    for subject in shape_subjects {
        let mut targets = Vec::new();
        for t in index.get(&subject, &sh_target_node) {
            targets.push(Target::Node(t.clone()));
        }
        for t in index.get(&subject, &sh_target_class) {
            if let Term::NamedNode(class) = t {
                targets.push(Target::Class(class.clone()));
            }
        }
        // Implicit class target: subject itself declared rdf:type rdfs:Class/owl:Class.
        if let Term::NamedNode(subject_iri) = &subject {
            let types = index.get(&subject, &rdf_type());
            let is_class = types
                .iter()
                .any(|t| *t == Term::NamedNode(rdfs_class()) || *t == Term::NamedNode(owl_class()));
            if is_class {
                targets.push(Target::ImplicitClass(subject_iri.clone()));
            }
        }

        let node_constraints = compile_constraints(&index, &subject);

        let mut property_shapes = Vec::new();
        for prop_shape_node in index.get(&subject, &sh_property) {
            let Some(Term::NamedNode(path)) = index.get_one(prop_shape_node, &sh_path) else {
                continue;
            };
            let constraints = compile_constraints(&index, prop_shape_node);
            property_shapes.push(PropertyShape {
                path: path.clone(),
                constraints,
            });
        }

        compiled.push(CompiledShape {
            id: subject,
            targets,
            node_constraints,
            property_shapes,
        });
    }

    compiled
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{GraphName, Literal, NamedOrBlankNode};

    fn nn(s: &str) -> NamedNode {
        NamedNode::new_unchecked(s)
    }
    fn shq(s: NamedOrBlankNode, p: NamedNode, o: Term) -> Quad {
        Quad::new(s, p, o, GraphName::DefaultGraph)
    }

    #[test]
    fn compiles_target_class_and_min_max_count() {
        let shape = nn("http://ex/PersonShape");
        let quads = vec![
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
            shq(
                shape.clone().into(),
                sh("property"),
                Term::BlankNode(oxrdf::BlankNode::new_unchecked("prop1")),
            ),
            shq(
                NamedOrBlankNode::BlankNode(oxrdf::BlankNode::new_unchecked("prop1")),
                sh("path"),
                Term::NamedNode(nn("http://ex/name")),
            ),
            shq(
                NamedOrBlankNode::BlankNode(oxrdf::BlankNode::new_unchecked("prop1")),
                sh("minCount"),
                Term::Literal(Literal::new_typed_literal(
                    "1",
                    nn("http://www.w3.org/2001/XMLSchema#integer"),
                )),
            ),
        ];

        let shapes = compile_shapes(&quads);
        assert_eq!(shapes.len(), 1);
        let s = &shapes[0];
        assert_eq!(s.targets, vec![Target::Class(nn("http://ex/Person"))]);
        assert_eq!(s.property_shapes.len(), 1);
        assert_eq!(s.property_shapes[0].path, nn("http://ex/name"));
        assert_eq!(
            s.property_shapes[0].constraints,
            vec![Constraint::MinCount(1)]
        );
    }

    #[test]
    fn compiles_implicit_class_target() {
        let shape = nn("http://ex/Animal");
        let quads = vec![shq(
            shape.clone().into(),
            rdf_type(),
            Term::NamedNode(NamedNode::new_unchecked(
                "http://www.w3.org/2000/01/rdf-schema#Class",
            )),
        )];
        // No shape-declaring predicate other than rdf:type rdfs:Class — but
        // that alone doesn't register it as a shape subject in this
        // increment's simple heuristic, since implicit class targets only
        // apply to nodes that are otherwise participating as a shape. Add a
        // targetNode-free constraint to make it recognizable: sh:class.
        let mut quads = quads;
        quads.push(shq(
            shape.clone().into(),
            sh("class"),
            Term::NamedNode(nn("http://ex/Whatever")),
        ));
        let shapes = compile_shapes(&quads);
        assert_eq!(shapes.len(), 1);
        assert!(
            shapes[0]
                .targets
                .contains(&Target::ImplicitClass(shape.clone()))
        );
    }

    #[test]
    fn compiles_in_list() {
        let shape = nn("http://ex/ColorShape");
        let b1 = oxrdf::BlankNode::new_unchecked("l1");
        let b2 = oxrdf::BlankNode::new_unchecked("l2");
        let rdf_nil = Term::NamedNode(nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#nil"));
        let quads = vec![
            shq(shape.clone().into(), sh("in"), Term::BlankNode(b1.clone())),
            shq(
                b1.clone().into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#first"),
                Term::Literal(Literal::new_simple_literal("red")),
            ),
            shq(
                b1.into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest"),
                Term::BlankNode(b2.clone()),
            ),
            shq(
                b2.clone().into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#first"),
                Term::Literal(Literal::new_simple_literal("blue")),
            ),
            shq(
                b2.into(),
                nn("http://www.w3.org/1999/02/22-rdf-syntax-ns#rest"),
                rdf_nil,
            ),
        ];
        let shapes = compile_shapes(&quads);
        assert_eq!(shapes.len(), 1);
        assert_eq!(
            shapes[0].node_constraints,
            vec![Constraint::In(vec![
                Term::Literal(Literal::new_simple_literal("red")),
                Term::Literal(Literal::new_simple_literal("blue")),
            ])]
        );
    }
}
