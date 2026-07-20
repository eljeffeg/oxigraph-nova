pub mod dataset;
pub mod evaluator;
pub mod extensions;
pub mod lftj;
pub mod options;
pub mod path;
pub mod select_vars;
pub mod service;
pub mod solution;
pub mod update;

pub use dataset::{
    Dataset, DatasetLftjSource, GraphSelector, InMemoryDataset, PatternTerm, QuadIter, QuadMatch,
    QuadPattern, StoreDataset,
};
pub use evaluator::{Evaluator, QueryResult, SolutionStream, TripleStream};
pub use extensions::{
    AggregateState, CustomAggregate, CustomFunction, CustomOperator, ExtensionRegistry,
    TermFunction, Value, ValueType,
};
pub use lftj::{
    collapse_counters_snapshot, lftj_fallback_total, lftj_used_total, reset_collapse_counters,
    CollapseCounterSnapshot,
};
pub use options::{CancellationToken, EvalLimitError, QueryOptions};
pub use select_vars::projected_variables;
#[cfg(feature = "http-client")]
pub use service::HttpServiceHandler;
pub use service::ServiceHandler;
pub use solution::{Solution, Solutions, SparqlVariable};
pub use update::{clear_graph, execute_update};
