use thiserror::Error;

#[derive(Debug, Error)]
pub enum Oxigraph {
    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Invalid IRI: {0}")]
    InvalidIri(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),

    /// Returned by Dictionary::intern() when the 40-bit TermId space is full.
    #[error("term ID space exhausted (limit ≈ 1.1 trillion distinct terms)")]
    IdSpaceExhausted,

    /// Returned by Dictionary::intern_graph() when the 8-bit GraphId space is full.
    #[error("graph ID space exhausted (maximum 253 user-named graphs)")]
    GraphSpaceExhausted,
}
