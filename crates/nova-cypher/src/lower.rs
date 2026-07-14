//! Lowering pass: Cypher Phase 1 AST → `spargebra::Query`.
//!
//! ## RDF ↔ property-graph (LPG) mapping
//!
//! This crate maps Cypher's property-graph model onto plain RDF triples
//! using a fixed convention:
//!
//! | Cypher concept                       | RDF representation                                             |
//! |--------------------------------------|----------------------------------------------------------------|
//! | node                                 | the subject term matched by a SPARQL variable                  |
//! | node label `:Label`                  | triple `?node rdf:type <LABEL_NS + "Label">`                   |
//! | node scalar property `{k: v}`        | triple `?node <PROP_NS + "k"> v` (literal object)              |
//! | relationship `-[:TYPE]->`            | triple `?from <REL_NS + "TYPE"> ?to`                           |
//! | relationship variable (typed)        | lowers every reference to the constant type IRI                |
//! | relationship variable (untyped)      | lowers every reference to the SPARQL variable bound to the predicate position |
//!
//! Relationship *properties* (`-[r:KNOWS {since: 2020}]->`) are represented
//! via RDF 1.2 quoted-triple annotations: `-[:TYPE {k: v}]->` between `?from`
//! and `?to` additionally emits a triple pattern
//! `<< ?from <REL_NS+TYPE> ?to >> <PROP_NS+"k"> v` (the quoted triple is the
//! *subject* of the annotation triple). `MATCH` can lower this because
//! matching/binding a quoted-triple subject in a BGP is fully supported by
//! Nova's query engine (`rdf-12`/`sparql-12` features are enabled
//! workspace-wide).
//!
//! `CREATE` relationship properties are rejected with a clear error:
//! inserting a quad whose *subject* is a quoted triple requires a storage
//! write API that `oxrdf` 0.3.3 cannot express (`oxrdf::Quad`'s `subject`
//! field is `NamedOrBlankNode`, with no `Triple` variant — see
//! `nova-query::update`'s module docs).
//!
//! Bare labels/relationship-types/property names (no namespace prefix in
//! Cypher source) are resolved against fixed base namespaces below. Making
//! these configurable (e.g. per-database prefix mapping) is left open.
//!
//! ## Property access and fresh variables
//!
//! spargebra's `Expression` has no "traverse this term's property" operator
//! — unlike Cypher, SPARQL expressions only operate on already-bound
//! variables/literals. So `n.age` appearing in `WHERE`/`RETURN`/`ORDER BY`
//! is lowered by allocating a fresh SPARQL variable (named `{var}_{prop}`)
//! and adding a triple pattern `?n <PROP_NS + "age"> ?n_age` to the query's
//! BGP, then referencing `?n_age` wherever `n.age` appeared. Chained access
//! (`n.a.b`) and property access on relationship variables are rejected
//! with a clear error, since neither has a representation in this mapping.
//!
//! ## Variable-length relationships
//!
//! `-[:KNOWS*1..3]->` (an explicit upper bound) cannot be losslessly
//! represented: SPARQL 1.1/1.2 property paths only support unbounded
//! repetition (`*`, `+`), not bounded `{min,max}` repetition. Phase 1
//! therefore only supports the *unbounded* forms — bare `*` and `*1..`
//! both lower to one-or-more (matching openCypher's own default range of
//! `1..` for a bare `*`), and `*0..` lowers to zero-or-more — and rejects
//! any pattern with an explicit maximum, or an explicit minimum other than
//! 0 or 1, with a clear "not supported" error.
//!
//! ## Solution-modifier assembly order
//!
//! Mirrors spargebra's own `build_select` (confirmed by reading
//! `spargebra::parser::build_select`): `Slice(Distinct(Project(OrderBy(Filter(Bgp)))))`
//! — `ORDER BY` wraps the raw (filtered) pattern *before* projection,
//! `DISTINCT` wraps the projection, and `SKIP`/`LIMIT` wrap everything last.
//!
//! ## Phase 2: write statements
//!
//! [`lower_statement`] lowers a [`CypherStatement`] (optional `MATCH`/`WHERE`
//! plus one or more [`WriteClause`]s) into a `spargebra::Update` — a `Vec` of
//! `GraphUpdateOperation`s, one *or more* per write clause, each a
//! `DeleteInsert { delete, insert, using: None, pattern }` reusing the same
//! `MATCH`/`WHERE` lowering machinery (`lower_pattern`/`lower_expr`) Phase 1
//! already has. All operations for one statement share the same base
//! (`MATCH`/`WHERE`) pattern, extended per-clause as needed (see below);
//! operations execute in clause order, matching both Cypher's own
//! left-to-right clause execution order and `execute_update`'s documented
//! sequential-operation semantics.
//!
//! **`CREATE`**: every node/relationship in the pattern becomes an `INSERT`
//! template quad. A node variable already bound by a preceding `MATCH`
//! resolves to that SPARQL variable (anchoring the new relationship/node to
//! an existing entity); a brand-new variable (or an anonymous node) mints a
//! fresh RDF blank node label, instantiated fresh per solution row by
//! `execute_update`'s own per-row blank-node map (`fresh_bnode_for`) — this
//! is exactly the "one new node per matched row" semantics Cypher's `CREATE`
//! has. Relationships created this way must specify a single relationship
//! type (`-[:TYPE]->`); untyped/variable-length relationships are rejected.
//!
//! **`SET n.prop = value`**: lowered as a delete-old/insert-new pair. The
//! "old" value (if any) is captured by `LEFT JOIN`-ing an extra triple
//! pattern `?n <PROP_NS+prop> ?old` onto the base pattern (so rows where the
//! property was never set still match, with `?old` left unbound — and an
//! unbound delete-template variable is simply skipped, never removed, by
//! `execute_update`'s `instantiate_ground_quad_pattern`); `value` is lowered
//! via the ordinary expression-lowering path and bound with `Extend`/`BIND`
//! so a computed value only has to be evaluated once (a `QuadPattern`
//! template's object cannot itself hold an arbitrary expression, only a
//! term).
//!
//! **`SET n:Label`**: a single unconditional `rdf:type` triple insert (no
//! delete needed — adding a label already present is idempotent).
//!
//! **`DELETE`/`DETACH DELETE`**: this crate's RDF↔LPG mapping has no bounded
//! set of predicates that make up "a node" (a node is just whatever
//! terms happen to appear as a triple's subject/object) — so deleting a
//! node deletes *every* triple where the matched variable appears as
//! subject or object, found via a variable-predicate/object (or
//! variable-subject/predicate) join onto the base pattern, one
//! `DeleteInsert` operation for each direction. `DELETE` and
//! `DETACH DELETE` are treated identically (both fully "detach"); openCypher's
//! plain `DELETE`-errors-if-still-connected check is not implemented — this
//! mirrors `nova-query::update`'s own precedent of treating `CLEAR`/`DROP`
//! identically for a similar reason (no cheap way to distinguish the stricter
//! case without extra bookkeeping this mapping doesn't have). Deleting a
//! relationship variable instead removes exactly the one triple that
//! relationship was matched from (using the subject/predicate/object
//! recorded when the `MATCH` pattern lowered it).
//!
//! **`REMOVE n.prop`**: identical delete-old-value shape as `SET`'s delete
//! half, with no insert. **`REMOVE n:Label`**: a single unconditional
//! `rdf:type` triple delete (removing an absent label is a no-op, mirroring
//! `SET n:Label`'s insert side).
//!
//! **Not supported** (clear error, not a panic): `MERGE` (see crate docs);
//! `SET`/`REMOVE`/`DELETE` targeting a variable introduced by `CREATE`
//! earlier in the *same* statement rather than by a preceding `MATCH` — this
//! doesn't work because each write clause becomes its own `DeleteInsert`
//! operation executed sequentially against the store, and a per-row blank
//! node's identity does not survive from one operation's `INSERT` to a later
//! operation's own fresh per-row blank-node map (see
//! `nova-query::update::delete_insert`'s `bnode_map`, which is local to each
//! `DeleteInsert` call) — there is no way for a later clause to refer back to
//! "the same" entity a `CREATE` in an earlier clause just created.

use crate::ast::{
    CypherQuery, CypherStatement, Expr, Literal, NodePattern, Pattern, RelDirection, RelPattern,
    ReturnItem, WriteClause,
};
use oxrdf::{BlankNode, Literal as RdfLiteral, NamedNode, Variable};
use spargebra::algebra::{Expression, GraphPattern, OrderExpression, PropertyPathExpression};
use spargebra::term::{
    GraphNamePattern, GroundQuadPattern, GroundTermPattern, NamedNodePattern, QuadPattern,
    TermPattern, TriplePattern,
};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::collections::HashMap;

/// Base namespace for bare (unprefixed) node labels, e.g. `:Person` →
/// `<http://oxigraph-nova.dev/cypher/label/Person>`.
pub const LABEL_NS: &str = "http://oxigraph-nova.dev/cypher/label/";
/// Base namespace for bare relationship types, e.g. `:KNOWS` →
/// `<http://oxigraph-nova.dev/cypher/rel/KNOWS>`.
pub const REL_NS: &str = "http://oxigraph-nova.dev/cypher/rel/";
/// Base namespace for bare property keys, e.g. `age` →
/// `<http://oxigraph-nova.dev/cypher/prop/age>`.
pub const PROP_NS: &str = "http://oxigraph-nova.dev/cypher/prop/";

/// How a relationship variable resolves when referenced in `WHERE`/`RETURN`
/// (see module doc's RDF↔LPG mapping table).
#[derive(Debug, Clone)]
enum RelBinding {
    /// Untyped relationship (`-[r]->`) — `r` is the SPARQL variable bound to
    /// the actual predicate IRI matched.
    Var(Variable),
    /// Typed relationship (`-[r:KNOWS]->`) — `r` always refers to this fixed
    /// type IRI.
    Const(NamedNode),
}

#[derive(Default)]
struct LowerCtx {
    node_vars: HashMap<String, Variable>,
    rel_vars: HashMap<String, RelBinding>,
    /// `(subject_var, object_var)` recorded for every *named* relationship
    /// pattern lowered from a `MATCH`, direction already resolved — lets
    /// Phase 2's `DELETE`/`DETACH DELETE` on a relationship variable rebuild
    /// exactly the one triple that relationship was matched from.
    rel_endpoints: HashMap<String, (Variable, Variable)>,
    prop_vars: HashMap<(String, String), Variable>,
    triples: Vec<TriplePattern>,
    /// `GraphPattern::Path` components contributed by variable-length
    /// relationships, `Join`-ed with the main BGP at the end.
    paths: Vec<GraphPattern>,
    anon_counter: u64,
}

impl LowerCtx {
    fn fresh_var(&mut self, prefix: &str) -> Variable {
        let n = self.anon_counter;
        self.anon_counter += 1;
        Variable::new_unchecked(format!("_{prefix}{n}"))
    }

    /// Mints a fresh blank-node label for use in a Phase 2 `CREATE` INSERT
    /// template (see [`lower_create`]) — distinct from [`Self::fresh_var`],
    /// which mints SPARQL variables for already/to-be-bound query results.
    fn fresh_create_bnode(&mut self) -> BlankNode {
        let n = self.anon_counter;
        self.anon_counter += 1;
        BlankNode::new_unchecked(format!("cyphercreate{n}"))
    }

    /// Gets-or-creates the SPARQL variable for a named Cypher node variable.
    fn node_var(&mut self, name: &str) -> Result<Variable, String> {
        if let Some(v) = self.node_vars.get(name) {
            return Ok(v.clone());
        }
        let v = Variable::new(name).map_err(|e| format!("invalid variable name `{name}`: {e}"))?;
        self.node_vars.insert(name.to_string(), v.clone());
        Ok(v)
    }
}

/// Lowers a full Cypher Phase 1 query into a `spargebra::Query::Select`.
pub fn lower(query: &CypherQuery) -> Result<Query, String> {
    let mut ctx = LowerCtx::default();

    lower_pattern(&mut ctx, &query.pattern)?;

    let where_expr = query
        .r#where
        .as_ref()
        .map(|e| lower_expr(&mut ctx, e))
        .transpose()?;

    let mut project_vars = Vec::with_capacity(query.r#return.items.len());
    let mut extends = Vec::new();
    for item in &query.r#return.items {
        let (out_var, extend) = lower_return_item(&mut ctx, item)?;
        if let Some((v, e)) = extend {
            extends.push((v, e));
        }
        project_vars.push(out_var);
    }

    let mut order_exprs = Vec::with_capacity(query.order_by.len());
    for oi in &query.order_by {
        let e = lower_expr(&mut ctx, &oi.expr)?;
        order_exprs.push(if oi.descending {
            OrderExpression::Desc(e)
        } else {
            OrderExpression::Asc(e)
        });
    }

    // All property-access fresh variables discovered while lowering WHERE /
    // RETURN / ORDER BY have been appended to `ctx.triples` by now — build
    // the base BGP (joined with any variable-length relationship paths)
    // only after all three phases have run.
    let mut base = GraphPattern::Bgp {
        patterns: std::mem::take(&mut ctx.triples),
    };
    for path in ctx.paths.drain(..) {
        base = GraphPattern::Join {
            left: Box::new(base),
            right: Box::new(path),
        };
    }

    if let Some(expr) = where_expr {
        base = GraphPattern::Filter {
            expr,
            inner: Box::new(base),
        };
    }

    for (variable, expression) in extends {
        base = GraphPattern::Extend {
            inner: Box::new(base),
            variable,
            expression,
        };
    }

    if !order_exprs.is_empty() {
        base = GraphPattern::OrderBy {
            inner: Box::new(base),
            expression: order_exprs,
        };
    }

    base = GraphPattern::Project {
        inner: Box::new(base),
        variables: project_vars,
    };

    if query.r#return.distinct {
        base = GraphPattern::Distinct {
            inner: Box::new(base),
        };
    }

    if query.skip.is_some() || query.limit.is_some() {
        base = GraphPattern::Slice {
            inner: Box::new(base),
            start: query.skip.unwrap_or(0) as usize,
            length: query.limit.map(|l| l as usize),
        };
    }

    Ok(Query::Select {
        dataset: None,
        pattern: base,
        base_iri: None,
    })
}

// ── Pattern (MATCH) lowering ─────────────────────────────────────────────

fn lower_pattern(ctx: &mut LowerCtx, pattern: &Pattern) -> Result<(), String> {
    let mut prev = lower_node_pattern(ctx, &pattern.start)?;
    for (rel, node) in &pattern.hops {
        let next = lower_node_pattern(ctx, node)?;
        lower_rel_pattern(ctx, rel, &prev, &next)?;
        prev = next;
    }
    Ok(())
}

/// Lowers a node pattern, returning the SPARQL variable that represents it
/// (fresh and unregistered if the node is anonymous).
fn lower_node_pattern(ctx: &mut LowerCtx, node: &NodePattern) -> Result<Variable, String> {
    let var = match &node.variable {
        Some(name) => ctx.node_var(name)?,
        None => ctx.fresh_var("node"),
    };

    for label in &node.labels {
        ctx.triples.push(TriplePattern {
            subject: TermPattern::Variable(var.clone()),
            predicate: NamedNodePattern::NamedNode(oxrdf::vocab::rdf::TYPE.into_owned()),
            object: TermPattern::NamedNode(NamedNode::new_unchecked(format!("{LABEL_NS}{label}"))),
        });
    }

    for (key, value) in &node.properties {
        let term = lower_literal_term(value)?;
        ctx.triples.push(TriplePattern {
            subject: TermPattern::Variable(var.clone()),
            predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                "{PROP_NS}{key}"
            ))),
            object: term,
        });
    }

    Ok(var)
}

fn lower_rel_pattern(
    ctx: &mut LowerCtx,
    rel: &RelPattern,
    from: &Variable,
    to: &Variable,
) -> Result<(), String> {
    let (subject, object) = match rel.direction.0 {
        // `Either` (undirected, `-[...]-`) is treated the same as `Right` in
        // Phase 1 — plain RDF triples have no undirected-edge concept, and
        // representing `Either` correctly would require unioning both
        // directions. Documented simplification; may be revisited later.
        RelDirection::Right | RelDirection::Either => (from.clone(), to.clone()),
        RelDirection::Left => (to.clone(), from.clone()),
    };

    if let Some(var_length) = rel.var_length {
        if rel.variable.is_some() {
            return Err(
                "cannot bind a variable to a variable-length relationship path in Phase 1"
                    .to_string(),
            );
        }
        if !rel.properties.is_empty() {
            return Err(
                "relationship properties are not supported on a variable-length relationship path"
                    .to_string(),
            );
        }
        let Some(rel_type) = &rel.rel_type else {
            return Err(
                "variable-length relationships must specify a single relationship type in Phase 1 (e.g. `-[:KNOWS*]->`)"
                    .to_string(),
            );
        };
        if var_length.max.is_some() {
            return Err(
                "bounded variable-length relationships (`*min..max` with an explicit max) are not supported in Phase 1 — SPARQL property paths only support unbounded `*`/`+` repetition".to_string(),
            );
        }
        let iri = PropertyPathExpression::NamedNode(NamedNode::new_unchecked(format!(
            "{REL_NS}{rel_type}"
        )));
        // Cypher's own default range for a bare `*` (no explicit bounds) is
        // `1..` (one-or-more) per the openCypher spec — NOT zero-or-more.
        // An explicit `*0..` is what's needed to get zero-or-more semantics.
        let path_expr = match var_length.min {
            None | Some(1) => PropertyPathExpression::OneOrMore(Box::new(iri)),
            Some(0) => PropertyPathExpression::ZeroOrMore(Box::new(iri)),
            Some(_) => {
                return Err(
                    "variable-length relationships with an explicit minimum > 1 and no maximum are not supported in Phase 1 (only `*`, `*0..`, and `*1..`/`+` are supported)".to_string(),
                );
            }
        };

        ctx.paths.push(GraphPattern::Path {
            subject: TermPattern::Variable(subject),
            path: path_expr,
            object: TermPattern::Variable(object),
        });
        return Ok(());
    }

    let predicate = if let Some(rel_type) = &rel.rel_type {
        NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!("{REL_NS}{rel_type}")))
    } else {
        let v = match &rel.variable {
            Some(name) => {
                Variable::new(name).map_err(|e| format!("invalid variable name `{name}`: {e}"))?
            }
            None => ctx.fresh_var("rel"),
        };
        NamedNodePattern::Variable(v)
    };

    if let Some(name) = &rel.variable {
        let binding = if let Some(rel_type) = &rel.rel_type {
            RelBinding::Const(NamedNode::new_unchecked(format!("{REL_NS}{rel_type}")))
        } else if let NamedNodePattern::Variable(v) = &predicate {
            RelBinding::Var(v.clone())
        } else {
            unreachable!("predicate is always Variable when rel_type is None")
        };
        ctx.rel_vars.insert(name.clone(), binding);
        // Recorded so Phase 2's `DELETE`/`DETACH DELETE` on a relationship
        // variable can rebuild exactly the one triple it was matched from.
        ctx.rel_endpoints
            .insert(name.clone(), (subject.clone(), object.clone()));
    }

    ctx.triples.push(TriplePattern {
        subject: TermPattern::Variable(subject.clone()),
        predicate: predicate.clone(),
        object: TermPattern::Variable(object.clone()),
    });

    // Relationship properties (`-[:TYPE {k: v}]->`) lower to RDF 1.2
    // quoted-triple annotations: `<< ?from <REL_NS+TYPE> ?to >> <PROP_NS+k> v`
    // (see module docs).
    for (key, value) in &rel.properties {
        let value_term = lower_literal_term(value)?;
        ctx.triples.push(TriplePattern {
            subject: TermPattern::Triple(Box::new(TriplePattern {
                subject: TermPattern::Variable(subject.clone()),
                predicate: predicate.clone(),
                object: TermPattern::Variable(object.clone()),
            })),
            predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                "{PROP_NS}{key}"
            ))),
            object: value_term,
        });
    }

    Ok(())
}

// ── RETURN item lowering ─────────────────────────────────────────────────

/// The implied default output-column name for a bare variable or single-level
/// property access (Cypher's own convention, minus the literal dot — SPARQL
/// variable names cannot contain `.`, so `n.name` implies `n_name`).
fn implied_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Variable(v) => Some(v.clone()),
        Expr::Property(base, prop) => match base.as_ref() {
            Expr::Variable(v) => Some(format!("{v}_{prop}")),
            _ => None,
        },
        _ => None,
    }
}

fn lower_return_item(
    ctx: &mut LowerCtx,
    item: &ReturnItem,
) -> Result<(Variable, Option<(Variable, Expression)>), String> {
    let lowered = lower_expr(ctx, &item.expr)?;
    let name = match &item.alias {
        Some(a) => a.clone(),
        None => implied_name(&item.expr).ok_or_else(|| {
            "an explicit `AS` alias is required for computed expressions in RETURN (Phase 1)"
                .to_string()
        })?,
    };
    let out_var =
        Variable::new(&name).map_err(|e| format!("invalid RETURN column name `{name}`: {e}"))?;

    if let Expression::Variable(v) = &lowered
        && v.as_str() == out_var.as_str()
    {
        // Already bound under exactly this name — no BIND/Extend needed.
        return Ok((out_var, None));
    }
    Ok((out_var.clone(), Some((out_var, lowered))))
}

// ── Expression lowering ──────────────────────────────────────────────────

fn lower_expr(ctx: &mut LowerCtx, expr: &Expr) -> Result<Expression, String> {
    match expr {
        Expr::Variable(name) => lower_variable_ref(ctx, name),
        Expr::Property(base, prop) => lower_property_ref(ctx, base, prop),
        Expr::Literal(lit) => Ok(Expression::Literal(lower_literal(lit)?)),
        Expr::And(l, r) => Ok(Expression::And(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Or(l, r) => Ok(Expression::Or(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Not(inner) => Ok(Expression::Not(Box::new(lower_expr(ctx, inner)?))),
        Expr::Eq(l, r) => Ok(Expression::Equal(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        // spargebra's `Expression` has no dedicated "not equal" variant —
        // lower `!=` as `!(l = r)`.
        Expr::Ne(l, r) => Ok(Expression::Not(Box::new(Expression::Equal(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )))),
        Expr::Lt(l, r) => Ok(Expression::Less(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Le(l, r) => Ok(Expression::LessOrEqual(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Gt(l, r) => Ok(Expression::Greater(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Ge(l, r) => Ok(Expression::GreaterOrEqual(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Add(l, r) => Ok(Expression::Add(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Sub(l, r) => Ok(Expression::Subtract(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Mul(l, r) => Ok(Expression::Multiply(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Div(l, r) => Ok(Expression::Divide(
            Box::new(lower_expr(ctx, l)?),
            Box::new(lower_expr(ctx, r)?),
        )),
        Expr::Neg(inner) => Ok(Expression::UnaryMinus(Box::new(lower_expr(ctx, inner)?))),
    }
}

fn lower_variable_ref(ctx: &mut LowerCtx, name: &str) -> Result<Expression, String> {
    if let Some(v) = ctx.node_vars.get(name) {
        return Ok(Expression::Variable(v.clone()));
    }
    if let Some(binding) = ctx.rel_vars.get(name) {
        return Ok(match binding {
            RelBinding::Var(v) => Expression::Variable(v.clone()),
            RelBinding::Const(n) => Expression::NamedNode(n.clone()),
        });
    }
    Err(format!(
        "unknown variable `{name}` (not bound by the MATCH pattern)"
    ))
}

fn lower_property_ref(ctx: &mut LowerCtx, base: &Expr, prop: &str) -> Result<Expression, String> {
    let Expr::Variable(var_name) = base else {
        return Err(
            "chained property access (e.g. `a.b.c`) is not supported in Phase 1".to_string(),
        );
    };

    if ctx.rel_vars.contains_key(var_name) {
        return Err(format!(
            "property access on relationship variable `{var_name}` is not supported in Phase 1 (relationship properties have no RDF mapping — see crate docs)"
        ));
    }
    if !ctx.node_vars.contains_key(var_name) {
        return Err(format!(
            "unknown variable `{var_name}` (not bound by the MATCH pattern)"
        ));
    }

    let key = (var_name.clone(), prop.to_string());
    if let Some(v) = ctx.prop_vars.get(&key) {
        return Ok(Expression::Variable(v.clone()));
    }

    let node_var = ctx.node_vars.get(var_name).unwrap().clone();
    let prop_var = Variable::new_unchecked(format!("{var_name}_{prop}"));
    ctx.triples.push(TriplePattern {
        subject: TermPattern::Variable(node_var),
        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
            "{PROP_NS}{prop}"
        ))),
        object: TermPattern::Variable(prop_var.clone()),
    });
    ctx.prop_vars.insert(key, prop_var.clone());
    Ok(Expression::Variable(prop_var))
}

// ── Literal lowering ──────────────────────────────────────────────────────

fn lower_literal(lit: &Literal) -> Result<RdfLiteral, String> {
    Ok(match lit {
        Literal::Str(s) => RdfLiteral::new_simple_literal(s.clone()),
        Literal::Int(n) => RdfLiteral::from(*n),
        Literal::Float(f) => RdfLiteral::from(*f),
        Literal::Bool(b) => RdfLiteral::from(*b),
        Literal::Null => {
            return Err("the `NULL` literal is not supported in Phase 1".to_string());
        }
    })
}

fn lower_literal_term(lit: &Literal) -> Result<TermPattern, String> {
    Ok(TermPattern::Literal(lower_literal(lit)?))
}

// ── Phase 2: write statement lowering ─────────────────────────────────────

/// Lowers a full Cypher Phase 2 write statement into a `spargebra::Update`.
/// See the module-level "Phase 2" doc section above for the full design
/// rationale behind each `WriteClause` variant's lowering.
pub fn lower_statement(stmt: &CypherStatement) -> Result<Update, String> {
    let mut ctx = LowerCtx::default();

    if let Some(pattern) = &stmt.pattern {
        lower_pattern(&mut ctx, pattern)?;
    }

    let where_expr = stmt
        .r#where
        .as_ref()
        .map(|e| lower_expr(&mut ctx, e))
        .transpose()?;

    // Base pattern shared by every write clause below: the same
    // Filter(Join(Bgp, paths...)) assembly `lower()` uses for the MATCH/WHERE
    // portion of a read query, minus the read-only solution modifiers
    // (RETURN/ORDER BY/etc.) which have no equivalent in a write statement.
    let mut base = GraphPattern::Bgp {
        patterns: ctx.triples.clone(),
    };
    for path in ctx.paths.clone() {
        base = GraphPattern::Join {
            left: Box::new(base),
            right: Box::new(path),
        };
    }
    if let Some(expr) = where_expr {
        base = GraphPattern::Filter {
            expr,
            inner: Box::new(base),
        };
    }

    let mut operations = Vec::new();
    for write in &stmt.writes {
        match write {
            WriteClause::Create(pattern) => {
                operations.push(lower_create(&mut ctx, pattern, &base)?);
            }
            WriteClause::SetProperty {
                variable,
                property,
                value,
            } => {
                operations.push(lower_set_property(
                    &mut ctx, variable, property, value, &base,
                )?);
            }
            WriteClause::SetLabel { variable, label } => {
                operations.push(lower_set_label(&ctx, variable, label, &base)?);
            }
            WriteClause::Delete {
                variables,
                detach: _,
            } => {
                // `DELETE` and `DETACH DELETE` are lowered identically — see
                // the module-level "Phase 2" doc section for why.
                operations.extend(lower_delete(&mut ctx, variables, &base)?);
            }
            WriteClause::RemoveProperty { variable, property } => {
                operations.push(lower_remove_property(&mut ctx, variable, property, &base)?);
            }
            WriteClause::RemoveLabel { variable, label } => {
                operations.push(lower_remove_label(&ctx, variable, label, &base)?);
            }
        }
    }

    Ok(Update {
        base_iri: None,
        operations,
    })
}

/// Lowers a `CREATE (pattern)` write clause into a single `DeleteInsert`
/// operation with an empty `delete` list and one `QuadPattern` per
/// label/property/relationship triple implied by `pattern`.
fn lower_create(
    ctx: &mut LowerCtx,
    pattern: &Pattern,
    base: &GraphPattern,
) -> Result<GraphUpdateOperation, String> {
    let mut insert = Vec::new();
    // Scoped to this one `CREATE` clause only (see module docs: a later
    // clause can never refer back to a variable a `CREATE` just minted).
    let mut create_bnodes: HashMap<String, BlankNode> = HashMap::new();

    let mut prev = lower_create_node(ctx, &mut create_bnodes, &pattern.start, &mut insert)?;
    for (rel, node) in &pattern.hops {
        let next = lower_create_node(ctx, &mut create_bnodes, node, &mut insert)?;
        lower_create_rel(rel, &prev, &next, &mut insert)?;
        prev = next;
    }
    drop(prev);

    Ok(GraphUpdateOperation::DeleteInsert {
        delete: Vec::new(),
        insert,
        using: None,
        pattern: Box::new(base.clone()),
    })
}

/// Lowers one node of a `CREATE` pattern to a `TermPattern` (its INSERT-time
/// identity) and pushes any label/property quads for it into `insert`. A
/// variable already bound by a preceding `MATCH` resolves to that SPARQL
/// variable; otherwise a fresh blank node is minted (shared by every
/// reference to the same variable *within this one `CREATE` clause*, via
/// `create_bnodes`, but never shared with any other clause — see module
/// docs).
fn lower_create_node(
    ctx: &mut LowerCtx,
    create_bnodes: &mut HashMap<String, BlankNode>,
    node: &NodePattern,
    insert: &mut Vec<QuadPattern>,
) -> Result<TermPattern, String> {
    let term = match &node.variable {
        Some(name) => {
            if let Some(v) = ctx.node_vars.get(name) {
                TermPattern::Variable(v.clone())
            } else if let Some(b) = create_bnodes.get(name) {
                TermPattern::BlankNode(b.clone())
            } else {
                let b = ctx.fresh_create_bnode();
                create_bnodes.insert(name.clone(), b.clone());
                TermPattern::BlankNode(b)
            }
        }
        None => TermPattern::BlankNode(ctx.fresh_create_bnode()),
    };

    for label in &node.labels {
        insert.push(QuadPattern {
            subject: term.clone(),
            predicate: NamedNodePattern::NamedNode(oxrdf::vocab::rdf::TYPE.into_owned()),
            object: TermPattern::NamedNode(NamedNode::new_unchecked(format!("{LABEL_NS}{label}"))),
            graph_name: GraphNamePattern::DefaultGraph,
        });
    }
    for (key, value) in &node.properties {
        let value_term = lower_literal_term(value)?;
        insert.push(QuadPattern {
            subject: term.clone(),
            predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                "{PROP_NS}{key}"
            ))),
            object: value_term,
            graph_name: GraphNamePattern::DefaultGraph,
        });
    }

    Ok(term)
}

/// Lowers one relationship hop of a `CREATE` pattern into a single INSERT
/// `QuadPattern`. Untyped and variable-length relationships are rejected —
/// see module docs.
fn lower_create_rel(
    rel: &RelPattern,
    from: &TermPattern,
    to: &TermPattern,
    insert: &mut Vec<QuadPattern>,
) -> Result<(), String> {
    if rel.var_length.is_some() {
        return Err("CREATE cannot create a variable-length relationship path".to_string());
    }
    if !rel.properties.is_empty() {
        return Err(
            "CREATE cannot set relationship properties — representing them requires              inserting a quad whose subject is a quoted triple (`<< ?from :TYPE ?to >> :prop              value`), which oxrdf 0.3 cannot express (oxrdf::Quad's subject field has no              Triple variant). Relationship properties on MATCH lower via the same              quoted-triple annotation syntax; see this crate's lower.rs module docs."
                .to_string(),
        );
    }
    let Some(rel_type) = &rel.rel_type else {
        return Err(
            "CREATE relationships must specify a single relationship type (e.g. `-[:KNOWS]->`)"
                .to_string(),
        );
    };
    let (subject, object) = match rel.direction.0 {
        RelDirection::Right | RelDirection::Either => (from.clone(), to.clone()),
        RelDirection::Left => (to.clone(), from.clone()),
    };
    insert.push(QuadPattern {
        subject,
        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
            "{REL_NS}{rel_type}"
        ))),
        object,
        graph_name: GraphNamePattern::DefaultGraph,
    });
    Ok(())
}

/// Looks up a node variable that a Phase 2 write clause (`SET`/`REMOVE`/
/// `DELETE`) is targeting, erroring clearly if it wasn't bound by a
/// preceding `MATCH` (see module docs on why a `CREATE`-introduced variable
/// can't be targeted this way).
fn lookup_written_node_var(
    ctx: &LowerCtx,
    variable: &str,
    clause_kw: &str,
) -> Result<Variable, String> {
    ctx.node_vars.get(variable).cloned().ok_or_else(|| {
        format!(
            "unknown variable `{variable}` in {clause_kw} (not bound by a preceding MATCH — \
             note that a variable introduced by an earlier CREATE in the same statement cannot \
             be referenced this way; see crate docs)"
        )
    })
}

/// Lowers `SET n.prop = value` into a delete-old/insert-new `DeleteInsert`
/// pair — see module docs for the full rationale.
fn lower_set_property(
    ctx: &mut LowerCtx,
    variable: &str,
    property: &str,
    value: &Expr,
    base: &GraphPattern,
) -> Result<GraphUpdateOperation, String> {
    let node_var = lookup_written_node_var(ctx, variable, "SET")?;

    let old_var = ctx.fresh_var("set_old");
    let with_old = GraphPattern::LeftJoin {
        left: Box::new(base.clone()),
        right: Box::new(GraphPattern::Bgp {
            patterns: vec![TriplePattern {
                subject: TermPattern::Variable(node_var.clone()),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                    "{PROP_NS}{property}"
                ))),
                object: TermPattern::Variable(old_var.clone()),
            }],
        }),
        expression: None,
    };

    let value_expr = lower_expr(ctx, value)?;
    let value_var = ctx.fresh_var("set_val");
    let with_bind = GraphPattern::Extend {
        inner: Box::new(with_old),
        variable: value_var.clone(),
        expression: value_expr,
    };

    let delete = vec![GroundQuadPattern {
        subject: GroundTermPattern::Variable(node_var.clone()),
        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
            "{PROP_NS}{property}"
        ))),
        object: GroundTermPattern::Variable(old_var),
        graph_name: GraphNamePattern::DefaultGraph,
    }];
    let insert = vec![QuadPattern {
        subject: TermPattern::Variable(node_var),
        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
            "{PROP_NS}{property}"
        ))),
        object: TermPattern::Variable(value_var),
        graph_name: GraphNamePattern::DefaultGraph,
    }];

    Ok(GraphUpdateOperation::DeleteInsert {
        delete,
        insert,
        using: None,
        pattern: Box::new(with_bind),
    })
}

/// Lowers `SET n:Label` into an insert-only `DeleteInsert` (idempotent — see
/// module docs).
fn lower_set_label(
    ctx: &LowerCtx,
    variable: &str,
    label: &str,
    base: &GraphPattern,
) -> Result<GraphUpdateOperation, String> {
    let node_var = lookup_written_node_var(ctx, variable, "SET")?;

    let insert = vec![QuadPattern {
        subject: TermPattern::Variable(node_var),
        predicate: NamedNodePattern::NamedNode(oxrdf::vocab::rdf::TYPE.into_owned()),
        object: TermPattern::NamedNode(NamedNode::new_unchecked(format!("{LABEL_NS}{label}"))),
        graph_name: GraphNamePattern::DefaultGraph,
    }];

    Ok(GraphUpdateOperation::DeleteInsert {
        delete: Vec::new(),
        insert,
        using: None,
        pattern: Box::new(base.clone()),
    })
}

/// Lowers `REMOVE n.prop` into a delete-old-only `DeleteInsert` — the same
/// shape as `SET`'s delete half, minus the insert (see module docs).
fn lower_remove_property(
    ctx: &mut LowerCtx,
    variable: &str,
    property: &str,
    base: &GraphPattern,
) -> Result<GraphUpdateOperation, String> {
    let node_var = lookup_written_node_var(ctx, variable, "REMOVE")?;

    let old_var = ctx.fresh_var("remove_old");
    let with_old = GraphPattern::LeftJoin {
        left: Box::new(base.clone()),
        right: Box::new(GraphPattern::Bgp {
            patterns: vec![TriplePattern {
                subject: TermPattern::Variable(node_var.clone()),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                    "{PROP_NS}{property}"
                ))),
                object: TermPattern::Variable(old_var.clone()),
            }],
        }),
        expression: None,
    };

    let delete = vec![GroundQuadPattern {
        subject: GroundTermPattern::Variable(node_var),
        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
            "{PROP_NS}{property}"
        ))),
        object: GroundTermPattern::Variable(old_var),
        graph_name: GraphNamePattern::DefaultGraph,
    }];

    Ok(GraphUpdateOperation::DeleteInsert {
        delete,
        insert: Vec::new(),
        using: None,
        pattern: Box::new(with_old),
    })
}

/// Lowers `REMOVE n:Label` into an unconditional delete-only `DeleteInsert`
/// (a no-op if the label is absent — see module docs).
fn lower_remove_label(
    ctx: &LowerCtx,
    variable: &str,
    label: &str,
    base: &GraphPattern,
) -> Result<GraphUpdateOperation, String> {
    let node_var = lookup_written_node_var(ctx, variable, "REMOVE")?;

    let delete = vec![GroundQuadPattern {
        subject: GroundTermPattern::Variable(node_var),
        predicate: NamedNodePattern::NamedNode(oxrdf::vocab::rdf::TYPE.into_owned()),
        object: GroundTermPattern::NamedNode(NamedNode::new_unchecked(format!(
            "{LABEL_NS}{label}"
        ))),
        graph_name: GraphNamePattern::DefaultGraph,
    }];

    Ok(GraphUpdateOperation::DeleteInsert {
        delete,
        insert: Vec::new(),
        using: None,
        pattern: Box::new(base.clone()),
    })
}

/// Lowers `DELETE var1, var2, ...` (and `DETACH DELETE`, treated identically
/// — see module docs) into one or more delete-only `DeleteInsert`
/// operations: two per node variable (one for each triple-position it might
/// appear in), or exactly one per relationship variable (removing precisely
/// the triple it was matched from).
fn lower_delete(
    ctx: &mut LowerCtx,
    variables: &[String],
    base: &GraphPattern,
) -> Result<Vec<GraphUpdateOperation>, String> {
    let mut ops = Vec::new();

    for name in variables {
        if let Some((subj_var, obj_var)) = ctx.rel_endpoints.get(name).cloned() {
            let predicate = match ctx.rel_vars.get(name) {
                Some(RelBinding::Var(v)) => NamedNodePattern::Variable(v.clone()),
                Some(RelBinding::Const(n)) => NamedNodePattern::NamedNode(n.clone()),
                None => unreachable!("rel_endpoints and rel_vars are always populated together"),
            };
            let delete = vec![GroundQuadPattern {
                subject: GroundTermPattern::Variable(subj_var),
                predicate,
                object: GroundTermPattern::Variable(obj_var),
                graph_name: GraphNamePattern::DefaultGraph,
            }];
            ops.push(GraphUpdateOperation::DeleteInsert {
                delete,
                insert: Vec::new(),
                using: None,
                pattern: Box::new(base.clone()),
            });
            continue;
        }

        let node_var = lookup_written_node_var(ctx, name, "DELETE")?;

        // Subject-position triples: `?node ?p ?o`.
        let pred_var = ctx.fresh_var("del_p");
        let obj_var = ctx.fresh_var("del_o");
        let subject_side_pattern = GraphPattern::Join {
            left: Box::new(base.clone()),
            right: Box::new(GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: TermPattern::Variable(node_var.clone()),
                    predicate: NamedNodePattern::Variable(pred_var.clone()),
                    object: TermPattern::Variable(obj_var.clone()),
                }],
            }),
        };
        ops.push(GraphUpdateOperation::DeleteInsert {
            delete: vec![GroundQuadPattern {
                subject: GroundTermPattern::Variable(node_var.clone()),
                predicate: NamedNodePattern::Variable(pred_var),
                object: GroundTermPattern::Variable(obj_var),
                graph_name: GraphNamePattern::DefaultGraph,
            }],
            insert: Vec::new(),
            using: None,
            pattern: Box::new(subject_side_pattern),
        });

        // Object-position triples: `?s ?p2 ?node`.
        let subj_var = ctx.fresh_var("del_s");
        let pred2_var = ctx.fresh_var("del_p2");
        let object_side_pattern = GraphPattern::Join {
            left: Box::new(base.clone()),
            right: Box::new(GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: TermPattern::Variable(subj_var.clone()),
                    predicate: NamedNodePattern::Variable(pred2_var.clone()),
                    object: TermPattern::Variable(node_var.clone()),
                }],
            }),
        };
        ops.push(GraphUpdateOperation::DeleteInsert {
            delete: vec![GroundQuadPattern {
                subject: GroundTermPattern::Variable(subj_var),
                predicate: NamedNodePattern::Variable(pred2_var),
                object: GroundTermPattern::Variable(node_var),
                graph_name: GraphNamePattern::DefaultGraph,
            }],
            insert: Vec::new(),
            using: None,
            pattern: Box::new(object_side_pattern),
        });
    }

    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, parse_statement};

    fn lower_str(src: &str) -> Query {
        lower(&parse(src).unwrap()).unwrap()
    }

    fn lower_stmt_str(src: &str) -> Update {
        lower_statement(&parse_statement(src).unwrap()).unwrap()
    }

    #[test]
    fn lowers_minimal_match_return() {
        let q = lower_str("MATCH (n:Person) RETURN n");
        match q {
            Query::Select { pattern, .. } => match pattern {
                GraphPattern::Project { inner, variables } => {
                    assert_eq!(variables.len(), 1);
                    assert_eq!(variables[0].as_str(), "n");
                    match *inner {
                        GraphPattern::Bgp { patterns } => assert_eq!(patterns.len(), 1),
                        other => panic!("expected Bgp, got {other:?}"),
                    }
                }
                other => panic!("expected Project, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn lowers_where_property_access_into_fresh_variable_triple() {
        let q = lower_str("MATCH (n:Person) WHERE n.age > 30 RETURN n.name");
        match q {
            Query::Select { pattern, .. } => match pattern {
                GraphPattern::Project { inner, variables } => {
                    assert_eq!(variables[0].as_str(), "n_name");
                    match *inner {
                        GraphPattern::Filter { expr, inner } => {
                            assert!(matches!(expr, Expression::Greater(_, _)));
                            match *inner {
                                // label triple + n_age triple + n_name triple
                                GraphPattern::Bgp { patterns } => assert_eq!(patterns.len(), 3),
                                other => panic!("expected Bgp, got {other:?}"),
                            }
                        }
                        other => panic!("expected Filter, got {other:?}"),
                    }
                }
                other => panic!("expected Project, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn lowers_relationship_pattern() {
        let q = lower_str("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b");
        match q {
            Query::Select { pattern, .. } => match pattern {
                GraphPattern::Project { inner, variables } => {
                    assert_eq!(variables.len(), 2);
                    match *inner {
                        // 2 label triples + 1 relationship triple
                        GraphPattern::Bgp { patterns } => assert_eq!(patterns.len(), 3),
                        other => panic!("expected Bgp, got {other:?}"),
                    }
                }
                other => panic!("expected Project, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn lowers_distinct_order_by_skip_limit() {
        let q = lower_str(
            "MATCH (n:Person) RETURN DISTINCT n.name AS name ORDER BY n.name DESC SKIP 5 LIMIT 10",
        );
        match q {
            Query::Select { pattern, .. } => match pattern {
                GraphPattern::Slice {
                    inner,
                    start,
                    length,
                } => {
                    assert_eq!(start, 5);
                    assert_eq!(length, Some(10));
                    match *inner {
                        GraphPattern::Distinct { inner } => match *inner {
                            GraphPattern::Project { .. } => {}
                            other => panic!("expected Project, got {other:?}"),
                        },
                        other => panic!("expected Distinct, got {other:?}"),
                    }
                }
                other => panic!("expected Slice, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bounded_variable_length_relationship() {
        let ast = parse("MATCH (a)-[:KNOWS*1..3]->(b) RETURN a").unwrap();
        assert!(lower(&ast).is_err());
    }

    #[test]
    fn rejects_chained_property_access() {
        let ast = parse("MATCH (n) RETURN n.a.b").unwrap();
        assert!(lower(&ast).is_err());
    }

    #[test]
    fn rejects_relationship_property_access() {
        let ast = parse("MATCH (a)-[r:KNOWS]->(b) WHERE r.since > 2000 RETURN a").unwrap();
        assert!(lower(&ast).is_err());
    }

    #[test]
    fn lowers_relationship_property_via_annotation_triple() {
        let q = lower_str("MATCH (a:Person)-[:KNOWS {since: 2020}]->(b:Person) RETURN a, b");
        match q {
            Query::Select { pattern, .. } => match pattern {
                GraphPattern::Project { inner, .. } => match *inner {
                    // 2 label triples + 1 relationship triple + 1 annotation triple
                    GraphPattern::Bgp { patterns } => {
                        assert_eq!(patterns.len(), 4);
                        let annotation = patterns
                            .iter()
                            .find(|t| matches!(t.subject, TermPattern::Triple(_)))
                            .expect("annotation triple with quoted-triple subject present");
                        assert_eq!(
                            annotation.predicate,
                            NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                                "{PROP_NS}since"
                            )))
                        );
                        assert_eq!(
                            annotation.object,
                            TermPattern::Literal(RdfLiteral::from(2020_i64))
                        );
                        if let TermPattern::Triple(inner_tp) = &annotation.subject {
                            assert_eq!(
                                inner_tp.predicate,
                                NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                                    "{REL_NS}KNOWS"
                                )))
                            );
                        } else {
                            panic!("expected TermPattern::Triple subject");
                        }
                    }
                    other => panic!("expected Bgp, got {other:?}"),
                },
                other => panic!("expected Project, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn rejects_relationship_property_on_variable_length_path() {
        let ast = parse("MATCH (a)-[:KNOWS* {since: 2020}]->(b) RETURN a").unwrap();
        assert!(lower(&ast).is_err());
    }

    #[test]
    fn rejects_unaliased_computed_expression() {
        let ast = parse("MATCH (n) RETURN n.age + 1").unwrap();
        assert!(lower(&ast).is_err());
    }

    // ── Phase 2 lowering tests ────────────────────────────────────────────

    #[test]
    fn lowers_bare_create() {
        let u = lower_stmt_str("CREATE (n:Person {name: \"Alice\"})");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert {
                delete,
                insert,
                pattern,
                ..
            } => {
                assert!(delete.is_empty());
                // label triple + property triple
                assert_eq!(insert.len(), 2);
                assert!(
                    matches!(**pattern, GraphPattern::Bgp { ref patterns } if patterns.is_empty())
                );
                assert!(matches!(insert[0].subject, TermPattern::BlankNode(_)));
                assert_eq!(insert[0].subject, insert[1].subject);
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn lowers_create_anchored_to_match() {
        let u = lower_stmt_str("MATCH (a:Person) CREATE (a)-[:KNOWS]->(b:Person)");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert { insert, .. } => {
                // relationship triple + label triple for b
                assert_eq!(insert.len(), 2);
                let rel_quad = insert
                    .iter()
                    .find(|q| {
                        q.predicate
                            == NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                                "{REL_NS}KNOWS"
                            )))
                    })
                    .expect("relationship quad present");
                assert_eq!(
                    rel_quad.subject,
                    TermPattern::Variable(Variable::new_unchecked("a"))
                );
                assert!(matches!(rel_quad.object, TermPattern::BlankNode(_)));
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn rejects_create_untyped_relationship() {
        let ast = crate::parser::parse_statement("CREATE (a)-[:KNOWS*]->(b)");
        // Variable-length CREATE relationships are already rejected in the
        // shared pattern-lowering path used by lower_create_rel, but that
        // path is only reached once parsing succeeds.
        if let Ok(stmt) = ast {
            assert!(lower_statement(&stmt).is_err());
        }
        let stmt = crate::parser::parse_statement("CREATE (a)-[r]->(b)").unwrap();
        assert!(lower_statement(&stmt).is_err());
    }

    #[test]
    fn rejects_create_relationship_properties() {
        // CREATE rejects relationship properties because oxrdf::Quad cannot
        // express a quoted-triple subject (see lower.rs module docs). The
        // property syntax itself parses; only CREATE-side lowering fails.
        let stmt =
            crate::parser::parse_statement("CREATE (a)-[:KNOWS {since: 2020}]->(b)").unwrap();
        let err = lower_statement(&stmt).unwrap_err();
        assert!(err.contains("oxrdf"), "error should mention oxrdf: {err}");
    }

    #[test]
    fn lowers_set_property() {
        let u = lower_stmt_str("MATCH (n:Person) SET n.age = 31");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert {
                delete,
                insert,
                pattern,
                ..
            } => {
                assert_eq!(delete.len(), 1);
                assert_eq!(insert.len(), 1);
                assert!(matches!(**pattern, GraphPattern::Extend { .. }));
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn lowers_set_label() {
        let u = lower_stmt_str("MATCH (n:Person) SET n:Admin");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert { delete, insert, .. } => {
                assert!(delete.is_empty());
                assert_eq!(insert.len(), 1);
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn lowers_multiple_set_items() {
        let u = lower_stmt_str("MATCH (n:Person) SET n.age = 31, n:Admin");
        assert_eq!(u.operations.len(), 2);
    }

    #[test]
    fn lowers_delete_node() {
        let u = lower_stmt_str("MATCH (n:Person) DETACH DELETE n");
        // Two DeleteInsert ops: subject-position and object-position.
        assert_eq!(u.operations.len(), 2);
    }

    #[test]
    fn lowers_delete_relationship() {
        let u = lower_stmt_str("MATCH (a)-[r:KNOWS]->(b) DELETE r");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert { delete, .. } => {
                assert_eq!(delete.len(), 1);
                assert_eq!(
                    delete[0].predicate,
                    NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!("{REL_NS}KNOWS")))
                );
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn lowers_remove_property() {
        let u = lower_stmt_str("MATCH (n:Person) REMOVE n.age");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert { delete, insert, .. } => {
                assert_eq!(delete.len(), 1);
                assert!(insert.is_empty());
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn lowers_remove_label() {
        let u = lower_stmt_str("MATCH (n:Person) REMOVE n:Admin");
        assert_eq!(u.operations.len(), 1);
        match &u.operations[0] {
            GraphUpdateOperation::DeleteInsert { delete, insert, .. } => {
                assert_eq!(delete.len(), 1);
                assert!(insert.is_empty());
            }
            other => panic!("expected DeleteInsert, got {other:?}"),
        }
    }

    #[test]
    fn rejects_set_on_unbound_variable() {
        let stmt =
            crate::parser::parse_statement("CREATE (a) SET a.x = 1").expect("parses syntactically");
        assert!(lower_statement(&stmt).is_err());
    }
}
