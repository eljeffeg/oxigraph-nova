//! Extension registry — custom SPARQL functions, operators, and aggregates.
//! Adapted from OxiRS (cool-japan/oxirs, Apache-2.0).

use anyhow::Result;
use oxrdf::{NamedNode, Term};
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq)]
pub enum ValueType {
    String,
    Integer,
    Float,
    Boolean,
    DateTime,
    Iri,
    BlankNode,
    Any,
}

#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Iri(String),
    Null,
}

pub trait CustomFunction: Send + Sync + Debug {
    fn name(&self) -> &str;
    fn arity(&self) -> Option<usize>;
    fn return_type(&self) -> ValueType;
    fn execute(&self, args: &[Value]) -> Result<Value>;
    fn is_deterministic(&self) -> bool {
        true
    }
}

pub trait CustomOperator: Send + Sync + Debug {
    fn symbol(&self) -> &str;
    fn execute(&self, lhs: &Value, rhs: &Value) -> Result<Value>;
}

pub trait CustomAggregate: Send + Sync + Debug {
    fn name(&self) -> &str;
    fn init(&self) -> Box<dyn AggregateState>;
    fn supports_distinct(&self) -> bool {
        false
    }
}

pub trait AggregateState: Send + Sync + Debug {
    fn accumulate(&mut self, value: &Value) -> Result<()>;
    fn finalize(&self) -> Result<Value>;
}

/// A `Term`-in, `Term`-out extension function — the same signature Oxigraph's
/// own `spareval` uses for its custom-function registry, and the shape
/// `spargeo::GEOSPARQL_EXTENSION_FUNCTIONS` is published in. Unlike
/// [`CustomFunction`] (which operates on the lossy [`Value`] enum), this can
/// carry any RDF term losslessly — including typed literals with
/// non-built-in datatypes such as `geo:wktLiteral`.
pub type TermFunction = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

#[derive(Default)]
pub struct ExtensionRegistry {
    pub functions: Arc<RwLock<HashMap<String, Box<dyn CustomFunction>>>>,
    pub operators: Arc<RwLock<HashMap<String, Box<dyn CustomOperator>>>>,
    pub aggregates: Arc<RwLock<HashMap<String, Box<dyn CustomAggregate>>>>,
    /// `Term`-typed functions, keyed by function IRI. Checked by the
    /// evaluator's `Function::Custom` dispatch before falling through to
    /// the `Value`-based `functions` map above.
    term_functions: Arc<RwLock<HashMap<NamedNode, TermFunction>>>,
}

// `TermFunction` (a plain `Arc<dyn Fn(...) -> ... + Send + Sync>`, with no
// `Debug` supertrait bound — unlike `CustomFunction`/`CustomOperator`/
// `CustomAggregate`, which all require `Debug`) can't be captured by
// `#[derive(Debug)]`, so this impl is written by hand, printing only the
// number of registered entries in each map rather than their contents.
impl Debug for ExtensionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field("functions", &self.functions.read().unwrap().len())
            .field("operators", &self.operators.read().unwrap().len())
            .field("aggregates", &self.aggregates.read().unwrap().len())
            .field("term_functions", &self.term_functions.read().unwrap().len())
            .finish()
    }
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_function(&self, f: Box<dyn CustomFunction>) -> Result<()> {
        self.functions
            .write()
            .unwrap()
            .insert(f.name().to_string(), f);
        Ok(())
    }

    pub fn register_operator(&self, op: Box<dyn CustomOperator>) -> Result<()> {
        self.operators
            .write()
            .unwrap()
            .insert(op.symbol().to_string(), op);
        Ok(())
    }

    pub fn register_aggregate(&self, agg: Box<dyn CustomAggregate>) -> Result<()> {
        self.aggregates
            .write()
            .unwrap()
            .insert(agg.name().to_string(), agg);
        Ok(())
    }

    /// Register a `Term`-typed extension function under `name` (a function
    /// IRI, e.g. `geof:distance`). Overwrites any previous registration
    /// under the same IRI.
    pub fn register_term_function(
        &self,
        name: NamedNode,
        f: impl Fn(&[Term]) -> Option<Term> + Send + Sync + 'static,
    ) -> Result<()> {
        self.term_functions
            .write()
            .unwrap()
            .insert(name, Arc::new(f));
        Ok(())
    }

    /// Look up a registered `Term`-typed function by its IRI, if any.
    pub fn term_function(&self, name: &NamedNode) -> Option<TermFunction> {
        self.term_functions.read().unwrap().get(name).cloned()
    }
}
