//! `clap` argument definitions for the `oxigraph` binary.
//!
//! Subcommand/flag names deliberately mirror upstream `oxigraph-cli`
//! (see `./research/oxigraph/cli/src/cli.rs`) wherever Nova's feature surface
//! overlaps — this binary is even named `oxigraph`, so scripts/muscle
//! memory written against one carry over to the other. Nova ships 9
//! subcommands: `load`, `backup`, `serve`, `query`, `update`, `dump`,
//! `convert`, `optimize`, `serve-read-only` — matching upstream's full
//! subcommand surface (some flags are trimmed relative to upstream where
//! Nova doesn't yet implement the corresponding capability — e.g. no
//! `--explain`/`--stats`/stdin input for `query`/`update`.

use clap::{Parser, Subcommand, ValueHint};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about, version, name = "oxigraph")]
/// Oxigraph Nova offline CLI tooling: bulk-load, backup, query, update,
/// dump, convert, optimize, and serve a persistent `RingStore` without
/// necessarily going through HTTP.
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Bulk-load one or more files (or stdin) directly into a persistent
    /// store
    ///
    /// Bypasses the HTTP Graph Store Protocol entirely — reads the
    /// file(s), parses them, and calls `RingStore::bulk_load` directly,
    /// which is much faster than sending it through `nova-server`'s
    /// `/store` endpoint for very large datasets. Multiple `--file`s are
    /// parsed in parallel (one thread per file) and merged into a single
    /// bulk-load pass; parsing itself is streamed incrementally into the
    /// store rather than materializing the whole dataset in memory first.
    Load {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// File(s) to load
        ///
        /// May be given multiple times (`--file a.ttl --file b.nt`) or as a
        /// single space-separated flag (`--file a.ttl b.nt`) to load several
        /// files in one bulk-load pass (parsed in parallel, merged into a
        /// single store build — much cheaper than one `load` invocation per
        /// file). If omitted entirely, data is read from stdin instead, in
        /// which case `--format` must be given explicitly (there's no file
        /// extension to guess it from).
        #[arg(short, long, num_args = 0.., value_hint = ValueHint::FilePath)]
        file: Vec<PathBuf>,
        /// The format of the file(s) to load
        ///
        /// Accepts either an extension (`nt`, `ttl`, `nq`, `trig`, `rdf`,
        /// `jsonld`) or a MIME type (e.g. `application/n-triples`).
        ///
        /// By default, the format is guessed from each file's extension;
        /// required when reading from stdin (no `--file` given).
        #[arg(long)]
        format: Option<String>,
        /// Name of the graph to load the data into
        ///
        /// By default, the default graph is used.
        ///
        /// Only meaningful when loading a graph file (N-Triples, Turtle,
        /// RDF/XML) — dataset formats (N-Quads, TriG, JSON-LD) already carry
        /// their own per-quad graph information, so this flag is ignored
        /// (with a warning) for those.
        #[arg(long, value_hint = ValueHint::Url)]
        graph: Option<String>,
        /// Base IRI used to resolve relative IRIs in the loaded file(s)
        ///
        /// Only meaningful for formats that can contain relative IRIs
        /// (Turtle, TriG, RDF/XML, JSON-LD) — ignored otherwise (N-Triples/
        /// N-Quads have no relative IRIs). When loading multiple files, the
        /// same base IRI is applied to each.
        #[arg(long, value_hint = ValueHint::Url)]
        base: Option<String>,
        /// Attempt to keep loading even if the data file is invalid
        ///
        /// This disables most of the validation on RDF content (via
        /// `oxrdfio::RdfParser::lenient`), matching upstream `oxigraph
        /// load`'s `--lenient` flag — useful for ingesting slightly-invalid
        /// real-world dumps (e.g. Wikidata) at the cost of silently
        /// accepting some malformed input rather than erroring on it.
        #[arg(long)]
        lenient: bool,
    },
    /// Create a database backup into a target directory
    ///
    /// After creation, the backup is usable as a fully independent Nova
    /// store, decoupled from the original (see `RingStore::backup`).
    Backup {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// Directory in which the backup will be written
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        destination: PathBuf,
    },
    /// Run a SPARQL query against a persistent store, offline (no HTTP)
    ///
    /// Results are format-negotiated the same way as `nova-server`'s
    /// `/sparql` endpoint, just driven by `--results-format` (or the
    /// extension of `--results-file`) instead of an `Accept` header.
    Query {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// The SPARQL query to execute, given directly on the command line
        ///
        /// Exactly one of `--query` / `--query-file` is required.
        #[arg(short, long, conflicts_with = "query_file")]
        query: Option<String>,
        /// Read the SPARQL query from a file instead of the command line
        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "query")]
        query_file: Option<PathBuf>,
        /// Write results to this file instead of stdout
        #[arg(long, value_hint = ValueHint::FilePath)]
        results_file: Option<PathBuf>,
        /// The format results should be written in
        ///
        /// For SELECT/ASK queries, one of: `json`, `xml`, `csv`, `tsv`
        /// (default: `json`, or guessed from `--results-file`'s extension).
        /// For CONSTRUCT/DESCRIBE queries, an RDF format extension or MIME
        /// type (default: `nt`, or guessed from `--results-file`'s
        /// extension).
        #[arg(long)]
        results_format: Option<String>,
        /// Equivalent to upstream Oxigraph's `oxigraph query
        /// --union-default-graph`: if this query has no `FROM`/`FROM NAMED`
        /// dataset clause of its own, use the RDF merge of the default
        /// graph and every named graph as its effective default graph,
        /// instead of just the store's actual default graph. A query that
        /// specifies its own `FROM`/`FROM NAMED` clause is unaffected either
        /// way. See `Command::Serve::union_default_graph`'s doc comment for
        /// the same semantics applied server-wide.
        #[arg(long)]
        union_default_graph: bool,
    },
    /// Run a SPARQL update against a persistent store, offline (no HTTP)
    Update {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// The SPARQL update to execute, given directly on the command line
        ///
        /// Exactly one of `--update` / `--update-file` is required.
        #[arg(short, long, conflicts_with = "update_file")]
        update: Option<String>,
        /// Read the SPARQL update from a file instead of the command line
        #[arg(long, value_hint = ValueHint::FilePath, conflicts_with = "update")]
        update_file: Option<PathBuf>,
    },
    /// Dump the contents of a store out to an RDF file
    ///
    /// Distinct from `backup`, which copies the whole binary store
    /// directory: `dump` serializes the store's *logical RDF content* to a
    /// chosen RDF format (Turtle/N-Quads/etc.), optionally restricted to one
    /// graph via `--graph`.
    Dump {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// File to write the dump to (defaults to stdout)
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        file: Option<PathBuf>,
        /// The format to serialize into
        ///
        /// Accepts either an extension (`nt`, `ttl`, `nq`, `trig`, `rdf`,
        /// `jsonld`) or a MIME type. Required if `--file` is not given
        /// (stdout output needs an explicit format) or if `--file`'s
        /// extension can't be guessed.
        #[arg(long)]
        format: Option<String>,
        /// Only dump this named graph's triples
        ///
        /// If not given, every graph (default + named) is dumped, which
        /// requires `--format` to be a dataset format (N-Quads/TriG/
        /// JSON-LD) — a plain triple format needs `--graph` to pick one
        /// graph to serialize.
        #[arg(long, value_hint = ValueHint::Url)]
        graph: Option<String>,
    },
    /// Stream-convert one RDF file to another format, without a store at all
    ///
    /// A pure `oxrdfio` pipe — reads `--from-file` (or stdin), reparses/
    /// reserializes term-by-term, and writes `--to-file` (or stdout). Useful
    /// as a cheap standalone format converter, independent of any Nova
    /// store.
    Convert {
        /// File to convert from (defaults to stdin)
        #[arg(long, value_hint = ValueHint::FilePath)]
        from_file: Option<PathBuf>,
        /// The format to convert from
        ///
        /// Required if `--from-file` is not given, or its extension can't
        /// be guessed.
        #[arg(long)]
        from_format: Option<String>,
        /// File to convert to (defaults to stdout)
        #[arg(long, value_hint = ValueHint::FilePath)]
        to_file: Option<PathBuf>,
        /// The format to convert to
        ///
        /// Required if `--to-file` is not given, or its extension can't be
        /// guessed.
        #[arg(long)]
        to_format: Option<String>,
        /// Only keep quads from this named graph (remapped to the default
        /// graph in the output)
        #[arg(long, value_hint = ValueHint::Url, conflicts_with = "from_default_graph")]
        from_graph: Option<String>,
        /// Only keep quads that are already in the default graph
        #[arg(long, conflicts_with = "from_graph")]
        from_default_graph: bool,
        /// Remap the (post-filter) default graph to this named graph in the
        /// output
        ///
        /// Only quads that are in the default graph *after* the
        /// `--from-graph`/`--from-default-graph` filter above has run are
        /// remapped — i.e. either quads that were already in the default
        /// graph (when no `--from-*` filter was given), or quads that
        /// `--from-graph`/`--from-default-graph` just selected and remapped
        /// there. This is **not** "remap every quad regardless of graph" —
        /// quads left in a named graph after the `--from-*` filter (e.g.
        /// when no filter is given and the input has multiple named graphs)
        /// pass through unchanged.
        #[arg(long, value_hint = ValueHint::Url)]
        to_graph: Option<String>,
        /// Attempt to keep converting even if the data file is invalid
        ///
        /// This disables most of the validation on RDF content (via
        /// `oxrdfio::RdfParser::lenient`), matching upstream `oxigraph
        /// convert`'s `--lenient` flag.
        #[arg(long)]
        lenient: bool,
    },
    /// Force storage compaction on a persistent store
    ///
    /// Nova already compacts automatically once the write-buffer delta
    /// crosses a size threshold (see `--compact-threshold` on `serve`); this
    /// subcommand lets an operator force it on demand (e.g. after a large
    /// batch of deletes, or before taking a `backup`).
    Optimize {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
    },
    /// Start the Nova SPARQL 1.2 HTTP server, opened read-only
    ///
    /// Rejects every write (`/update`, and `PUT`/`POST`/`DELETE` on
    /// `/store`) at the HTTP layer with `403 Forbidden`, so this server
    /// process can never mutate `--location`. **This is not the same as
    /// storage-level concurrent-multi-process read isolation** —
    /// `RingStore` still uses a single-writer WAL design (see
    /// `oxigraph_nova_storage_ring::store`'s module doc, "Isolation
    /// semantics"); running this against a `--location` directory that
    /// another process is concurrently writing to is not guaranteed safe.
    /// This flag only guarantees that *this* process won't write.
    ServeReadOnly {
        /// Directory in which Nova data is persisted (required — unlike
        /// `serve`, there is no in-memory read-only mode)
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// Host and port to listen on
        #[arg(short, long, default_value = "0.0.0.0:3030", value_hint = ValueHint::Hostname)]
        bind: String,
        /// Abort a `/sparql` query that runs longer than this many seconds,
        /// returning 504 Gateway Timeout. Unset by default (no timeout).
        #[arg(long)]
        query_timeout_s: Option<u64>,
        /// Cap the number of result rows/triples a single `/sparql` query
        /// may produce; exceeding it returns 413 Payload Too Large. Unset
        /// by default (no cap).
        #[arg(long)]
        max_results: Option<usize>,
        /// Bound the number of `/sparql` query evaluations running
        /// concurrently; a request arriving while this many evaluations are
        /// already in flight is rejected immediately with 503 Service
        /// Unavailable. Unset by default (unbounded).
        #[arg(long)]
        max_parallel_queries: Option<usize>,
        /// Enable Tantivy-backed full-text search (`text:query`/
        /// `text:contains` SPARQL extension functions), indexed
        /// incrementally on the store's compaction cycle.
        ///
        /// Requires this binary to have been built with the `fulltext`
        /// cargo feature (`cargo run -p oxigraph-nova-cli --features
        /// fulltext`); passing this flag without that feature enabled is a
        /// hard error at startup.
        #[arg(long)]
        fulltext: bool,
        /// Server-wide default equivalent to upstream Oxigraph's `oxigraph
        /// serve --union-default-graph`: a query with no `FROM`/`FROM NAMED`
        /// dataset clause of its own then uses the RDF merge of the default
        /// graph and every named graph as its effective default graph,
        /// instead of just the store's actual default graph. A query that
        /// specifies its own `FROM`/`FROM NAMED` clause is unaffected either
        /// way.
        #[arg(long)]
        union_default_graph: bool,
    },
    /// Start the Nova SPARQL 1.2 HTTP server
    ///
    /// A thin wrapper around the same store-construction + `Server::run`
    /// logic as the standalone `nova_serve` binary (see
    /// `crates/nova-server/src/bin/nova_serve.rs`), exposed here as a
    /// subcommand of the unified `oxigraph` CLI tool. `nova_serve` itself is
    /// kept unchanged (and unaffected by this command) since external
    /// benchmark scripts depend on its exact flags.
    Serve {
        /// Directory in which the data should be persisted
        ///
        /// If not present, an in-memory store is used.
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: Option<PathBuf>,
        /// File to bulk-load on startup
        ///
        /// Only used if the store comes up empty (a fresh in-memory store,
        /// or a fresh/empty --location directory with no prior WAL
        /// history) — if a persistent store already has data recovered
        /// from its WAL, --file is ignored.
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        file: Option<PathBuf>,
        /// Host and port to listen on
        #[arg(short, long, default_value = "0.0.0.0:3030", value_hint = ValueHint::Hostname)]
        bind: String,
        /// Delta-size threshold (number of live entries) that triggers
        /// automatic inline compaction. Default: 1,000,000. Has no effect
        /// without --location.
        #[arg(long)]
        compact_threshold: Option<usize>,
        /// WAL durability policy: fsync every N milliseconds ("group
        /// commit") instead of the default 500ms interval. Has no effect
        /// without --location.
        #[arg(long)]
        sync_interval_ms: Option<u64>,
        /// Abort a `/sparql` query that runs longer than this many seconds,
        /// returning 504 Gateway Timeout. Unset by default (no timeout).
        /// Matches upstream Oxigraph's `--timeout` flag.
        #[arg(long)]
        query_timeout_s: Option<u64>,
        /// Cap the number of result rows/triples a single `/sparql` query
        /// may produce; exceeding it returns 413 Payload Too Large. Unset
        /// by default (no cap).
        #[arg(long)]
        max_results: Option<usize>,
        /// Bound the number of `/sparql` query evaluations running
        /// concurrently; a request arriving while this many evaluations are
        /// already in flight is rejected immediately with 503 Service
        /// Unavailable. Unset by default (unbounded).
        #[arg(long)]
        max_parallel_queries: Option<usize>,
        /// Enable Tantivy-backed full-text search (`text:query`/
        /// `text:contains` SPARQL extension functions), indexed
        /// incrementally on the store's compaction cycle.
        ///
        /// Requires this binary to have been built with the `fulltext`
        /// cargo feature (`cargo run -p oxigraph-nova-cli --features
        /// fulltext`); passing this flag without that feature enabled is a
        /// hard error at startup.
        #[arg(long)]
        fulltext: bool,
        /// Server-wide default equivalent to upstream Oxigraph's `oxigraph
        /// serve --union-default-graph`: a query with no `FROM`/`FROM NAMED`
        /// dataset clause of its own then uses the RDF merge of the default
        /// graph and every named graph as its effective default graph,
        /// instead of just the store's actual default graph. A query that
        /// specifies its own `FROM`/`FROM NAMED` clause is unaffected either
        /// way.
        #[arg(long)]
        union_default_graph: bool,
    },
}
