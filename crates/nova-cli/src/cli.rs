//! `clap` argument definitions for the `oxigraph` binary.
//!
//! Subcommand/flag names deliberately mirror upstream `oxigraph-cli`
//! (see `./research/oxigraph/cli/src/cli.rs`) wherever Nova's feature surface
//! overlaps — this binary is even named `oxigraph`, so scripts/muscle
//! memory written against one carry over to the other. Nova currently
//! ships a smaller subset (`load`, `backup`, `serve`) than
//! `oxigraph-cli`'s nine subcommands — see `CLAUDE.md`.

use clap::{Parser, Subcommand, ValueHint};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about, version, name = "oxigraph")]
/// Oxigraph Nova offline CLI tooling: bulk-load, backup, and serve a
/// persistent `RingStore` without going through HTTP.
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Bulk-load a file directly into a persistent store
    ///
    /// Bypasses the HTTP Graph Store Protocol entirely — reads the file,
    /// parses it, and calls `RingStore::bulk_load` directly, which is much
    /// faster than sending it through `nova-server`'s `/store` endpoint for
    /// very large datasets.
    Load {
        /// Directory in which Nova data is persisted
        #[arg(short, long, value_hint = ValueHint::DirPath)]
        location: PathBuf,
        /// File to load
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// The format of the file to load
        ///
        /// Accepts either an extension (`nt`, `ttl`, `nq`, `trig`, `rdf`,
        /// `jsonld`) or a MIME type (e.g. `application/n-triples`).
        ///
        /// By default, the format is guessed from the file's extension.
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
    },
}
