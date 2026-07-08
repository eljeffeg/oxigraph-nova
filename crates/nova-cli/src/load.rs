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

use anyhow::{Context, Result};
use oxigraph_nova_core::{GraphName, Quad};
use oxrdfio::{RdfFormat, RdfParser};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Resolve a `--format` value (extension string or MIME type) or, if absent,
/// guess from `path`'s extension — mirroring `oxigraph-cli`'s
/// `rdf_format_from_name`/`rdf_format_from_path` behaviour.
fn resolve_format(format: Option<&str>, path: &Path) -> Result<RdfFormat> {
    let name = match format {
        Some(f) => f.to_string(),
        None => path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .with_context(|| {
                format!(
                    "cannot guess RDF format from file extension of {}; pass --format explicitly",
                    path.display()
                )
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

/// Parse `path` (per `format`/its extension) into `Quad`s targeting `graph`
/// (for plain-triple formats), or the quad's own graph (for dataset
/// formats, where `graph` is ignored with a warning if explicitly given).
pub fn parse_file(
    path: &Path,
    format: Option<&str>,
    graph: Option<&GraphName>,
) -> Result<Vec<Quad>> {
    let fmt = resolve_format(format, path)?;

    if fmt.supports_datasets() && graph.is_some() {
        eprintln!(
            "[oxigraph] warning: --graph is ignored for dataset formats (N-Quads/TriG/JSON-LD); \
             each quad's own graph is used instead."
        );
    }

    let target_graph = graph.cloned().unwrap_or(GraphName::DefaultGraph);
    let f = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(f);

    let format_name = fmt.name();
    let quads = RdfParser::from_format(fmt)
        .with_default_graph(target_graph)
        .for_reader(reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("{format_name} parse error"))?;

    Ok(quads)
}
