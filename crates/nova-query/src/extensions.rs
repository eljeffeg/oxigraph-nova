//! Extension registry — custom SPARQL functions, operators, and aggregates.
//! Adapted from OxiRS (cool-japan/oxirs, Apache-2.0).

use anyhow::Result;
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

#[derive(Debug, Default)]
pub struct ExtensionRegistry {
    pub functions: Arc<RwLock<HashMap<String, Box<dyn CustomFunction>>>>,
    pub operators: Arc<RwLock<HashMap<String, Box<dyn CustomOperator>>>>,
    pub aggregates: Arc<RwLock<HashMap<String, Box<dyn CustomAggregate>>>>,
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
}
