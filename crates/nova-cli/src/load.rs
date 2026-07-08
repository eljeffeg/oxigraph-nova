//! File-format detection + parsing for `nova-cli load` / `nova-cli serve --file`.
//!
//! Mirrors the same by-Content-Type dispatch `nova-server`'s
//! `parse_body_triples` uses (see `crates/nova-server/src/lib.rs`), but keyed
//! off a file extension or an explicit `--format` value (extension string or
//! MIME type) instead of an HTTP `Content-Type` header — matching
//! `oxigraph-cli`'s `--format` semantics ("It can be an extension like `nt`
//! or a MIME type like `application/n-triples`").

use anyhow::{Context, Result, bail};
use oxigraph_nova_core::{GraphName, Quad};
use oxjsonld::JsonLdParser;
use oxrdfxml::RdfXmlParser;
use oxttl::{NQuadsParser, NTriplesParser, TriGParser, TurtleParser};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Which parsing family a format resolves to: a plain-triple ("graph")
/// format, where every parsed triple is inserted into one caller-chosen
/// target graph, or a full quad/dataset format, which carries its own
/// per-quad graph information already.
enum RdfFormat {
    NTriples,
    Turtle,
    RdfXml,
    NQuads,
    TriG,
    JsonLd,
}

impl RdfFormat {
    /// `true` for formats that can express multiple named graphs directly
    /// (N-Quads, TriG, JSON-LD) — for these, an explicit `--graph` override
    /// doesn't make sense and is ignored (with a warning).
    fn is_dataset_format(&self) -> bool {
        matches!(
            self,
            RdfFormat::NQuads | RdfFormat::TriG | RdfFormat::JsonLd
        )
    }
}

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

    Ok(match name.as_str() {
        "nt" | "application/n-triples" => RdfFormat::NTriples,
        "ttl" | "turtle" | "text/turtle" => RdfFormat::Turtle,
        "rdf" | "xml" | "rdfxml" | "application/rdf+xml" => RdfFormat::RdfXml,
        "nq" | "nquads" | "application/n-quads" => RdfFormat::NQuads,
        "trig" | "application/trig" => RdfFormat::TriG,
        "jsonld" | "json" | "application/ld+json" => RdfFormat::JsonLd,
        other => bail!(
            "unrecognized RDF format {other:?}; expected one of: nt, ttl, rdf, nq, trig, jsonld \
             (or the equivalent MIME type)"
        ),
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

    if fmt.is_dataset_format() && graph.is_some() {
        eprintln!(
            "[nova-cli] warning: --graph is ignored for dataset formats (N-Quads/TriG/JSON-LD); \
             each quad's own graph is used instead."
        );
    }

    let target_graph = graph.cloned().unwrap_or(GraphName::DefaultGraph);
    let f = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(f);

    let quads = match fmt {
        RdfFormat::NTriples => NTriplesParser::new()
            .for_reader(reader)
            .map(|r| r.map(|t| Quad::new(t.subject, t.predicate, t.object, target_graph.clone())))
            .collect::<Result<Vec<_>, _>>()
            .context("N-Triples parse error")?,
        RdfFormat::Turtle => TurtleParser::new()
            .for_reader(reader)
            .map(|r| r.map(|t| Quad::new(t.subject, t.predicate, t.object, target_graph.clone())))
            .collect::<Result<Vec<_>, _>>()
            .context("Turtle parse error")?,
        RdfFormat::RdfXml => RdfXmlParser::new()
            .for_reader(reader)
            .map(|r| r.map(|t| Quad::new(t.subject, t.predicate, t.object, target_graph.clone())))
            .collect::<Result<Vec<_>, _>>()
            .context("RDF/XML parse error")?,
        RdfFormat::NQuads => NQuadsParser::new()
            .for_reader(reader)
            .collect::<Result<Vec<_>, _>>()
            .context("N-Quads parse error")?,
        RdfFormat::TriG => TriGParser::new()
            .for_reader(reader)
            .collect::<Result<Vec<_>, _>>()
            .context("TriG parse error")?,
        RdfFormat::JsonLd => JsonLdParser::new()
            .for_reader(reader)
            .collect::<Result<Vec<_>, _>>()
            .context("JSON-LD parse error")?,
    };

    Ok(quads)
}
