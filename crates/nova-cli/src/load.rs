//! File-format detection + parsing for `oxigraph load` / `oxigraph serve --file`.
//!
//! Mirrors the same by-Content-Type dispatch `nova-server`'s
//! `parse_body_triples` uses (see `crates/nova-server/src/lib.rs`), but keyed
//! off a file extension or an explicit `--format` value (extension string or
//! MIME type) instead of an HTTP `Content-Type` header — matching
//! `oxigraph-cli`'s `--format` semantics ("It can be an extension like `nt`
//! or a MIME type like `application/n-triples`").
//!
//! Parsing itself goes through `oxrdfio::RdfParser`, which uniformly yields
//! `Quad`s for every format (see its crate docs): for plain-triple formats
//! (N-Triples/Turtle/RDF-XML) it places every triple in the graph configured
//! via `.with_default_graph(...)`; for dataset formats (N-Quads/TriG/
//! JSON-LD) each quad's own graph (as encoded in the document) is used as-is.
//!
//! ## Streaming bulk-load (single source)
//!
//! [`load_sources_into_store`] never collects the parsed quads into a
//! `Vec` first: `RingStore::bulk_load` already accepts `impl
//! IntoIterator<Item = Quad>` and consumes it lazily (see its doc comment
//! in `oxigraph_nova_storage_ring::store`), so for a single input source
//! (one file, or stdin) the parser's iterator is fed directly into
//! `bulk_load`, capping peak memory to roughly one WAL-batch's worth of
//! interning work rather than "the whole parsed dataset".
//!
//! ## Multiple files
//!
//! Calling `bulk_load` once per file would be quadratic — its
//! implementation re-reads every existing graph's current triples into an
//! in-memory map before merging in new quads (see its doc comment), so N
//! separate calls re-merge the same growing graphs N times. Instead, for
//! multiple `--file`s, one parser thread per file is spawned (under
//! `std::thread::scope`), each parsing its own file fully in parallel and
//! sending `Quad`s over a **bounded** `mpsc::sync_channel` (bounding memory
//! even though parsing runs in parallel — a fast parser thread blocks on
//! `send` once the channel is full, rather than racing ahead and buffering
//! unboundedly). The main thread drains the single shared receiver into one
//! `bulk_load` call. Interning/dictionary work stays serial regardless
//! (it's behind the store's mutex) — only the CPU-bound parsing itself runs
//! in parallel.

use anyhow::{Context, Result};
use oxigraph_nova_core::{GraphName, Quad};
use oxigraph_nova_storage_ring::RingStore;
use oxrdfio::{RdfFormat, RdfParseError, RdfParser};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

/// Resolve a `--format` value (extension string or MIME type) or, if absent,
/// guess from `path`'s extension — mirroring `oxigraph-cli`'s
/// `rdf_format_from_name`/`rdf_format_from_path` behaviour.
fn resolve_format(format: Option<&str>, path: &Path) -> Result<RdfFormat> {
    resolve_format_opt(format, Some(path))
}

/// Like [`resolve_format`], but `path` is optional — used by `oxigraph dump`
/// (stdout output, no `--file`) and `oxigraph convert` (stdin/stdout,
/// `--from-file`/`--to-file` optional). When `path` is `None`, `format` must
/// be given explicitly (there's no extension to guess from).
pub(crate) fn resolve_format_opt(format: Option<&str>, path: Option<&Path>) -> Result<RdfFormat> {
    let name = match format {
        Some(f) => f.to_string(),
        None => path
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .with_context(|| match path {
                Some(p) => format!(
                    "cannot guess RDF format from file extension of {}; pass --format explicitly",
                    p.display()
                ),
                None => "no file given to guess an RDF format from; pass --format explicitly"
                    .to_string(),
            })?,
    };

    RdfFormat::from_extension(&name)
        .or_else(|| RdfFormat::from_media_type(&name))
        .with_context(|| {
            format!(
                "unrecognized RDF format {name:?}; expected one of: nt, ttl, rdf, nq, trig, jsonld \
                 (or the equivalent MIME type)"
            )
        })
}

/// Build a configured [`RdfParser`] for `fmt`, applying `graph` (as the
/// default graph, for plain-triple formats) and `base` (as the base IRI,
/// for formats that support relative IRIs) if given. `lenient` disables
/// most RDF content validation (see `oxrdfio::RdfParser::lenient`),
/// matching `--lenient`.
fn make_parser(
    fmt: RdfFormat,
    graph: Option<&GraphName>,
    base: Option<&str>,
    lenient: bool,
) -> Result<RdfParser> {
    let target_graph = graph.cloned().unwrap_or(GraphName::DefaultGraph);
    let mut parser = RdfParser::from_format(fmt).with_default_graph(target_graph);
    if let Some(base_iri) = base {
        parser = parser
            .with_base_iri(base_iri)
            .with_context(|| format!("invalid --base IRI {base_iri:?}"))?;
    }
    if lenient {
        parser = parser.lenient();
    }
    Ok(parser)
}

fn warn_if_graph_ignored(fmt: RdfFormat, graph: Option<&GraphName>) {
    if fmt.supports_datasets() && graph.is_some() {
        eprintln!(
            "[oxigraph] warning: --graph is ignored for dataset formats (N-Quads/TriG/JSON-LD); \
             each quad's own graph is used instead."
        );
    }
}

/// Stream `files` (or stdin, if `files` is empty) into `store` via a single
/// `RingStore::bulk_load` call, returning the number of quads loaded.
///
/// - Zero files: reads from stdin. `format` must be given explicitly (no
///   extension to guess from).
/// - One file: streams the parser's iterator directly into `bulk_load`, no
///   extra threads.
/// - Multiple files: spawns one parser thread per file, merging their
///   output through a bounded channel into a single `bulk_load` call (see
///   this module's doc comment for the full rationale).
pub fn load_sources_into_store(
    store: &RingStore,
    files: &[PathBuf],
    format: Option<&str>,
    graph: Option<&GraphName>,
    base: Option<&str>,
    lenient: bool,
) -> Result<usize> {
    match files {
        [] => load_stdin(store, format, graph, base, lenient),
        [single] => load_single_file(store, single, format, graph, base, lenient),
        multiple => load_multiple_files(store, multiple, format, graph, base, lenient),
    }
}

/// Bound on in-flight parsed-but-not-yet-interned quads per parser thread —
/// caps peak memory from parallel parsing while still letting each thread
/// run ahead of the (serial, mutex-guarded) interning work in `bulk_load`.
const CHANNEL_BOUND: usize = 4096;

fn load_stdin(
    store: &RingStore,
    format: Option<&str>,
    graph: Option<&GraphName>,
    base: Option<&str>,
    lenient: bool,
) -> Result<usize> {
    let name = format.with_context(
        || "--format is required when reading from stdin (no file extension to guess it from)",
    )?;
    let fmt = RdfFormat::from_extension(name)
        .or_else(|| RdfFormat::from_media_type(name))
        .with_context(|| {
            format!(
                "unrecognized RDF format {name:?}; expected one of: nt, ttl, rdf, nq, trig, jsonld \
                 (or the equivalent MIME type)"
            )
        })?;
    warn_if_graph_ignored(fmt, graph);

    let reader = BufReader::new(std::io::stdin());
    stream_into_store(store, make_parser(fmt, graph, base, lenient)?, reader, fmt)
}

fn load_single_file(
    store: &RingStore,
    path: &Path,
    format: Option<&str>,
    graph: Option<&GraphName>,
    base: Option<&str>,
    lenient: bool,
) -> Result<usize> {
    let fmt = resolve_format(format, path)?;
    warn_if_graph_ignored(fmt, graph);

    let f = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(f);
    stream_into_store(store, make_parser(fmt, graph, base, lenient)?, reader, fmt)
}

/// Feed a single reader's parsed quads directly into `bulk_load`, without
/// materializing them into a `Vec` first. A parse error partway through
/// terminates the iterator early (via [`ErrorCapturingIter`]) and is
/// surfaced afterward, once `bulk_load` has returned.
fn stream_into_store(
    store: &RingStore,
    parser: RdfParser,
    reader: impl Read,
    fmt: RdfFormat,
) -> Result<usize> {
    let format_name = fmt.name();
    let error_slot: Mutex<Option<RdfParseError>> = Mutex::new(None);
    let quads = ErrorCapturingIter {
        inner: parser.for_reader(reader),
        error_slot: &error_slot,
    };

    let count = store.bulk_load(quads).with_context(|| "bulk_load failed")?;

    if let Some(e) = error_slot.into_inner().unwrap() {
        return Err(anyhow::Error::new(e).context(format!("{format_name} parse error")));
    }
    Ok(count)
}

/// Adapts a `Result<Quad, RdfParseError>` iterator into a plain `Quad`
/// iterator suitable for `RingStore::bulk_load` (which wants `impl
/// IntoIterator<Item = Quad>`, not `Item = Result<Quad, _>`): stashes the
/// first error it encounters into `error_slot` and then ends the iterator,
/// so `bulk_load` stops cleanly at the point parsing failed. The caller
/// checks `error_slot` after `bulk_load` returns to surface the failure.
struct ErrorCapturingIter<'a, I> {
    inner: I,
    error_slot: &'a Mutex<Option<RdfParseError>>,
}

impl<I: Iterator<Item = Result<Quad, RdfParseError>>> Iterator for ErrorCapturingIter<'_, I> {
    type Item = Quad;

    fn next(&mut self) -> Option<Quad> {
        match self.inner.next() {
            Some(Ok(q)) => Some(q),
            Some(Err(e)) => {
                *self.error_slot.lock().unwrap() = Some(e);
                None
            }
            None => None,
        }
    }
}

/// Parse `files` in parallel (one thread per file), merging their quads
/// through a single bounded channel into one `bulk_load` call.
fn load_multiple_files(
    store: &RingStore,
    files: &[PathBuf],
    format: Option<&str>,
    graph: Option<&GraphName>,
    base: Option<&str>,
    lenient: bool,
) -> Result<usize> {
    // Resolve each file's format/parser up front (cheap, and surfaces
    // "can't guess format" errors before spawning any threads).
    let mut parsers = Vec::with_capacity(files.len());
    for path in files {
        let fmt = resolve_format(format, path)?;
        warn_if_graph_ignored(fmt, graph);
        parsers.push((path.clone(), fmt, make_parser(fmt, graph, base, lenient)?));
    }

    let (tx, rx) = mpsc::sync_channel::<Result<Quad, String>>(CHANNEL_BOUND);

    let count = std::thread::scope(|scope| -> Result<usize> {
        for (path, fmt, parser) in parsers {
            let tx = tx.clone();
            scope.spawn(move || {
                let format_name = fmt.name();
                let open =
                    File::open(&path).with_context(|| format!("failed to open {}", path.display()));
                let f = match open {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let reader = BufReader::new(f);
                for result in parser.for_reader(reader) {
                    match result {
                        Ok(q) => {
                            if tx.send(Ok(q)).is_err() {
                                // Receiver gone (e.g. bulk_load bailed early
                                // for another reason) — stop parsing.
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("{format_name} parse error: {e}")));
                            return;
                        }
                    }
                }
            });
        }
        // Drop our own sender so the channel closes once every spawned
        // thread's clone has also been dropped (i.e. every file is done).
        drop(tx);

        let error_slot: Mutex<Option<String>> = Mutex::new(None);
        let quads = MultiFileIter {
            rx: &rx,
            error_slot: &error_slot,
        };
        let count = store.bulk_load(quads).with_context(|| "bulk_load failed")?;

        if let Some(msg) = error_slot.into_inner().unwrap() {
            anyhow::bail!(msg);
        }
        Ok(count)
    })?;

    Ok(count)
}

/// Like [`ErrorCapturingIter`], but draining a channel fed by multiple
/// parser threads instead of a single in-process parser.
struct MultiFileIter<'a> {
    rx: &'a mpsc::Receiver<Result<Quad, String>>,
    error_slot: &'a Mutex<Option<String>>,
}

impl Iterator for MultiFileIter<'_> {
    type Item = Quad;

    fn next(&mut self) -> Option<Quad> {
        match self.rx.recv() {
            Ok(Ok(q)) => Some(q),
            Ok(Err(msg)) => {
                *self.error_slot.lock().unwrap() = Some(msg);
                None
            }
            Err(_) => None, // channel closed, all threads done
        }
    }
}
