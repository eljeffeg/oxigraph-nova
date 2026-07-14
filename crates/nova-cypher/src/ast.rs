//! Cypher AST — pure data, no storage/evaluation dependency.
//!
//! Covers the openCypher read-only subset (Phase 1):
//! `MATCH` (node + relationship patterns) / `WHERE` / `RETURN` (with
//! optional `AS` aliases) / `ORDER BY` / `SKIP` / `LIMIT` / `DISTINCT` — see
//! [`CypherQuery`].
//!
//! Also covers the openCypher write subset (Phase 2): `CREATE` / `SET` /
//! `DELETE` / `DETACH DELETE` / `REMOVE` — see [`CypherStatement`] and
//! [`WriteClause`]. `MERGE` is not yet covered (its match-or-create upsert
//! semantics don't map onto a single SPARQL Update operation the way the
//! other write clauses do — see `lower.rs`'s module docs).
//!
//! Not covered (rejected by the parser with a clear error): `WITH`
//! re-scoping chains, list/map comprehensions, `MERGE`, `OPTIONAL MATCH`,
//! `UNION`, multiple `MATCH` clauses, and multi-statement
//! (semicolon-separated) scripts.

/// A parsed Cypher query: one `MATCH` pattern, optional `WHERE`, one
/// `RETURN` clause, and optional `ORDER BY` / `SKIP` / `LIMIT`.
#[derive(Debug, Clone, PartialEq)]
pub struct CypherQuery {
    pub pattern: Pattern,
    pub r#where: Option<Expr>,
    pub r#return: ReturnClause,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// A full `MATCH` pattern: a start node followed by zero or more
/// relationship-node hops, e.g. `(a)-[r:KNOWS]->(b)-[:LIKES]->(c)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub start: NodePattern,
    pub hops: Vec<(RelPattern, NodePattern)>,
}

/// A node pattern `(var:Label1:Label2 {prop: val, ...})`. All parts optional
/// except the parens themselves; an anonymous node has `variable: None`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Vec<(String, Literal)>,
}

/// Direction of a relationship pattern as written in the query source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    /// `-[...]->`
    Right,
    /// `<-[...]-`
    Left,
    /// `-[...]-`  (direction unspecified — matches either way)
    Either,
}

/// Relationship-length quantifier for variable-length patterns like
/// `-[:KNOWS*1..3]->`. `None` means a fixed single-hop relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarLength {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub rel_type: Option<String>,
    pub direction: RelDirectionOrDefault,
    pub var_length: Option<VarLength>,
    /// `-[r:KNOWS {since: 2020}]->`-style relationship properties. On
    /// `MATCH`, lowered to RDF 1.2 quoted-triple annotations (see
    /// `lower.rs`); on `CREATE`, rejected because `oxrdf::Quad`'s subject
    /// field cannot express a quoted triple (see `lower.rs`).
    pub properties: Vec<(String, Literal)>,
}

/// Wrapper so `RelPattern` can derive `Default` (`RelDirection` itself has
/// no natural "unset" value since `Either` is a real, meaningful direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelDirectionOrDefault(pub RelDirection);

impl Default for RelDirectionOrDefault {
    fn default() -> Self {
        Self(RelDirection::Either)
    }
}

/// One item of a `RETURN` clause: an expression plus an optional `AS` alias.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<ReturnItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

/// Literal values as written in Cypher source.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

/// Expression AST for `WHERE`/`RETURN`/`ORDER BY`. Covers property access,
/// literals, variable references, comparisons, and boolean/arithmetic
/// operators — enough for `WHERE`/`RETURN` needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Variable(String),
    /// `n.prop`
    Property(Box<Expr>, String),
    Literal(Literal),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
}

// ── Phase 2: write statements ────────────────────────────────────────────

/// A parsed Cypher write statement: an optional `MATCH`/`WHERE` followed by
/// one or more write clauses ([`WriteClause`]).
///
/// Unlike [`CypherQuery`], there is no `RETURN`/`ORDER BY`/`SKIP`/`LIMIT` —
/// SPARQL Update (the lowering target for write statements) produces no
/// result rows, so a `RETURN` after a write clause is rejected by the parser
/// with a clear error rather than silently accepted and ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct CypherStatement {
    /// `None` for a pattern-less write statement, e.g. a bare
    /// `CREATE (n:Person)` with no preceding `MATCH`.
    pub pattern: Option<Pattern>,
    pub r#where: Option<Expr>,
    /// Always non-empty — the parser rejects a statement with no write
    /// clause at all (that would just be a [`CypherQuery`] instead).
    pub writes: Vec<WriteClause>,
}

/// One write clause of a Cypher write statement. Covers `CREATE` / `SET` /
/// `DELETE` / `DETACH DELETE` / `REMOVE`. `MERGE` is not yet covered (see
/// crate-level docs).
#[derive(Debug, Clone, PartialEq)]
pub enum WriteClause {
    /// `CREATE (pattern)` — unconditionally creates new nodes/relationships;
    /// may also anchor a new relationship to an already-`MATCH`-bound node
    /// variable (e.g. `CREATE (a)-[:KNOWS]->(b:Person)` where `a` was bound
    /// by a preceding `MATCH`).
    Create(Pattern),
    /// `SET n.prop = value` — creates the property if absent, overwrites it
    /// otherwise. `variable` must already be bound by a preceding `MATCH`.
    SetProperty {
        variable: String,
        property: String,
        value: Expr,
    },
    /// `SET n:Label` — adds a label (does not remove any existing labels).
    /// `variable` must already be bound by a preceding `MATCH`.
    SetLabel { variable: String, label: String },
    /// `DELETE var1, var2, ...` / `DETACH DELETE var1, var2, ...`. Every
    /// `variable` must already be bound by a preceding `MATCH`.
    Delete {
        variables: Vec<String>,
        detach: bool,
    },
    /// `REMOVE n.prop` — removes the property if present (no-op otherwise).
    /// `variable` must already be bound by a preceding `MATCH`.
    RemoveProperty { variable: String, property: String },
    /// `REMOVE n:Label` — removes the label if present (no-op otherwise).
    /// `variable` must already be bound by a preceding `MATCH`.
    RemoveLabel { variable: String, label: String },
}
