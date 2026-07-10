//! SPARQL Service Description (`http://www.w3.org/ns/sparql-service-description#`).
//!
//! Builds the RDF graph returned by `GET /` — a machine-readable capabilities
//! document describing this endpoint, matching upstream Oxigraph's
//! `oxigraph serve`. Some SPARQL tooling (e.g. query builders) fetches this
//! before sending real requests, using it to discover supported result/RDF
//! formats and query/update language support rather than guessing.
//!
//! Declares support per both the SPARQL 1.1 Service Description Recommendation
//! (<https://www.w3.org/TR/sparql11-service-description/>) and its successor,
//! the SPARQL 1.2 Service Description Working Draft
//! (<https://www.w3.org/TR/sparql12-service-description/>) — the latter keeps
//! the same `sd:` namespace but replaces the old per-version-per-language
//! instances (`sd:SPARQL10Query`, `sd:SPARQL11Query`, `sd:SPARQL11Update`)
//! with unified `sd:SPARQLQuery`/`sd:SPARQLUpdate` language terms plus a new
//! `sd:supportedVersion` property (range `http://www.w3.org/ns/sparql#`
//! version resources) that declares which language version(s) are supported.
//! The old per-version instances are retained here too, since the new spec
//! preserves them "for backwards compatibility" with SPARQL 1.1-only-aware
//! consumers.
//!
//! Serialization is handled by the caller (`lib.rs`'s `serialize_triples`),
//! which content-negotiates the same way as CONSTRUCT/DESCRIBE results — this
//! module only builds the plain `Vec<Triple>` graph.

use oxrdf::vocab::rdf;
use oxrdf::{BlankNode, NamedNode, NamedOrBlankNode, Triple};

/// `sd:` vocabulary terms actually used here (a small subset of the full
/// `http://www.w3.org/ns/sparql-service-description#` vocabulary). Plain
/// functions rather than `const NamedNode`s, since this `oxrdf` version has
/// no `NamedNode::new_const_unchecked`.
mod sd {
    use oxrdf::NamedNode;

    pub fn service() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#Service")
    }
    pub fn default_entailment_regime() -> NamedNode {
        NamedNode::new_unchecked(
            "http://www.w3.org/ns/sparql-service-description#defaultEntailmentRegime",
        )
    }
    pub fn endpoint() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#endpoint")
    }
    pub fn feature() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#feature")
    }
    pub fn result_format() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#resultFormat")
    }
    pub fn supported_language() -> NamedNode {
        NamedNode::new_unchecked(
            "http://www.w3.org/ns/sparql-service-description#supportedLanguage",
        )
    }
    /// `sd:supportedVersion` — SPARQL 1.2 Service Description WD § 3.2.11.
    /// Relates `sd:Service` to a SPARQL language version resource (range
    /// `rdfs:Resource`, values from the `http://www.w3.org/ns/sparql#`
    /// namespace); combined with `sd:supportedLanguage` to indicate exactly
    /// which version(s) of that language are implemented.
    pub fn supported_version() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#supportedVersion")
    }
    pub fn empty_graphs() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#EmptyGraphs")
    }
    pub fn sparql_10_query() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#SPARQL10Query")
    }
    pub fn sparql_11_query() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#SPARQL11Query")
    }
    pub fn sparql_11_update() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#SPARQL11Update")
    }
    /// `sd:SPARQLQuery` — SPARQL 1.2 Service Description WD § 3.4.4. Unified
    /// `sd:Language` instance for the SPARQL Query language, version-agnostic
    /// (paired with `sd:supportedVersion` to pin down which version(s)).
    /// Supersedes `sd:SPARQL10Query`/`sd:SPARQL11Query` for version-aware
    /// consumers, though those are kept too for backwards compatibility.
    pub fn sparql_query() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#SPARQLQuery")
    }
    /// `sd:SPARQLUpdate` — SPARQL 1.2 Service Description WD § 3.4.5. Unified
    /// `sd:Language` instance for the SPARQL Update language; see
    /// `sparql_query`'s doc comment for the same version-agnostic rationale.
    pub fn sparql_update() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql-service-description#SPARQLUpdate")
    }
    /// `sd:extensionFunction` — relates `sd:Service` to a `NamedNode` naming
    /// a non-standard SPARQL extension function this endpoint recognizes.
    /// Used to advertise `text:query`/`text:contains` when the `fulltext`
    /// feature is enabled (see `generate_service_description_graph`'s
    /// `fulltext_enabled` parameter).
    pub fn extension_function() -> NamedNode {
        NamedNode::new_unchecked(
            "http://www.w3.org/ns/sparql-service-description#extensionFunction",
        )
    }
}

/// SPARQL language version resources (`http://www.w3.org/ns/sparql#`
/// namespace, `sparql:` prefix in the spec's own examples), used as
/// `sd:supportedVersion` objects. Defined by the SPARQL 1.2 Service
/// Description Working Draft (§ 3.2.11, § 3.4.4/3.4.5).
mod sparql_version {
    use oxrdf::NamedNode;

    pub fn v1_0() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql#version-1.0")
    }
    pub fn v1_1() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql#version-1.1")
    }
    pub fn v1_2() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql#version-1.2")
    }
    /// "Basic" RDF 1.2 conformance level of SPARQL 1.2 — per the spec's own
    /// example: "when a service description indicates support for the
    /// sd:SPARQLQuery language and sparql:version-1.2-basic version, support
    /// for SPARQL 1.2 Query with RDF 1.2 Basic conformance is indicated."
    /// Nova's `rdf-12`/`sparql-12`-enabled stack passes 265/269 (98%) of the
    /// W3C SPARQL 1.2 Working Draft test suite, so this is declared too.
    pub fn v1_2_basic() -> NamedNode {
        NamedNode::new_unchecked("http://www.w3.org/ns/sparql#version-1.2-basic")
    }
}

/// `sd:resultFormat` IRIs for the SPARQL results (SELECT/ASK) serializations
/// this server supports — matches `results_format_for_accept`'s four formats.
const RESULT_FORMAT_IRIS: [&str; 4] = [
    "http://www.w3.org/ns/formats/SPARQL_Results_JSON",
    "http://www.w3.org/ns/formats/SPARQL_Results_XML",
    "http://www.w3.org/ns/formats/SPARQL_Results_CSV",
    "http://www.w3.org/ns/formats/SPARQL_Results_TSV",
];

/// `sd:resultFormat` IRIs for the RDF (CONSTRUCT/DESCRIBE) serializations
/// this server supports — matches `serialize_triples`'s dispatch table.
const RDF_FORMAT_IRIS: [&str; 6] = [
    "http://www.w3.org/ns/formats/N-Triples",
    "http://www.w3.org/ns/formats/N-Quads",
    "http://www.w3.org/ns/formats/Turtle",
    "http://www.w3.org/ns/formats/TriG",
    "http://www.w3.org/ns/formats/RDF_XML",
    "https://www.w3.org/ns/formats/data/JSON-LD",
];

/// Function-IRI namespace for Nova's full-text search extension functions
/// (`text:query`/`text:contains`) — must match `oxigraph_nova_query::evaluator`'s
/// private `TEXT_NS` constant exactly (duplicated here rather than shared,
/// since that constant is a private evaluator-internal detail, not part of
/// `nova-query`'s public API).
const TEXT_FN_NS: &str = "http://oxigraph-nova.dev/fn/text#";

/// True when the binary was built with the `geosparql` cargo feature — used
/// as the default `geosparql_enabled` value at `generate_service_description_graph`'s
/// only call site. GeoSPARQL functions are pure/stateless and always
/// registered whenever the feature is compiled in (no `--geosparql` runtime
/// flag exists), so this is a compile-time constant rather than server state.
pub const GEOSPARQL_COMPILED_IN: bool = cfg!(feature = "geosparql");

/// Build the `sd:Service` description graph for this endpoint.
///
/// `endpoint_url` is an absolute IRI naming the primary query endpoint
/// (`/sparql`, aliased as `/query`) — used as the `sd:endpoint` value.
///
/// `fulltext_enabled`: when `true`, advertises `text:query`/`text:contains`
/// as `sd:extensionFunction`s — set from whether the server was constructed
/// with `Server::with_text_search` (i.e. `--fulltext` was passed and the
/// binary was built with the `fulltext` cargo feature).
///
/// `reasoning_enabled`: when `true`, `sd:defaultEntailmentRegime` advertises
/// `http://www.w3.org/ns/entailment/OWL-RL` instead of `.../Simple` — set
/// from whether the server was constructed with `Server::with_reasoning`
/// (i.e. `--reasoning` was passed).
///
/// `geosparql_enabled`: when `true`, advertises all 43 GeoSPARQL functions
/// (`geof:distance`, `sf:intersects`, etc.) as `sd:extensionFunction`s — set
/// from [`GEOSPARQL_COMPILED_IN`] (a compile-time constant; unlike
/// `fulltext`/`reasoning`, GeoSPARQL functions are pure and need no runtime
/// `--geosparql` flag or server-side state).
pub fn generate_service_description_graph(
    endpoint_url: &str,
    fulltext_enabled: bool,
    reasoning_enabled: bool,
    geosparql_enabled: bool,
) -> Vec<Triple> {
    let root = NamedOrBlankNode::BlankNode(BlankNode::default());
    let mut graph = Vec::new();

    graph.push(Triple::new(root.clone(), rdf::TYPE, sd::service()));
    if let Ok(endpoint) = NamedNode::new(endpoint_url) {
        graph.push(Triple::new(root.clone(), sd::endpoint(), endpoint));
    }

    // Query and Update are both always supported — Nova's server has no
    // read-only/query-only deployment mode (unlike upstream's `--read-only`).
    //
    // Old, version-specific SPARQL 1.1 Service Description REC instances —
    // kept for backwards compatibility with consumers that only understand
    // the SPARQL 1.1 Service Description vocabulary (the SPARQL 1.2 Service
    // Description WD explicitly preserves these "for backwards
    // compatibility", see its § 3.3.3 note).
    graph.push(Triple::new(
        root.clone(),
        sd::supported_language(),
        sd::sparql_10_query(),
    ));
    graph.push(Triple::new(
        root.clone(),
        sd::supported_language(),
        sd::sparql_11_query(),
    ));
    graph.push(Triple::new(
        root.clone(),
        sd::supported_language(),
        sd::sparql_11_update(),
    ));

    // SPARQL 1.2 Service Description Working Draft
    // (https://www.w3.org/TR/sparql12-service-description/) terms: the
    // unified, version-agnostic `sd:SPARQLQuery`/`sd:SPARQLUpdate` languages,
    // paired with `sd:supportedVersion` triples naming every version Nova
    // implements (1.0/1.1/1.2, plus the "1.2-basic" RDF 1.2 Basic
    // conformance level per the spec's own worked example). Nova's
    // `rdf-12`/`sparql-12`-enabled parsing/evaluation stack (`oxrdf`,
    // `oxttl`, `spargebra`, `sparesults`, `sparopt`) passes 265/269 (98%) of
    // the W3C SPARQL 1.2 Working Draft test suite.
    graph.push(Triple::new(
        root.clone(),
        sd::supported_language(),
        sd::sparql_query(),
    ));
    graph.push(Triple::new(
        root.clone(),
        sd::supported_language(),
        sd::sparql_update(),
    ));
    for version in [
        sparql_version::v1_0(),
        sparql_version::v1_1(),
        sparql_version::v1_2(),
        sparql_version::v1_2_basic(),
    ] {
        graph.push(Triple::new(root.clone(), sd::supported_version(), version));
    }

    for iri in RESULT_FORMAT_IRIS {
        graph.push(Triple::new(
            root.clone(),
            sd::result_format(),
            NamedNode::new_unchecked(iri),
        ));
    }
    for iri in RDF_FORMAT_IRIS {
        graph.push(Triple::new(
            root.clone(),
            sd::result_format(),
            NamedNode::new_unchecked(iri),
        ));
    }

    // `sd:EmptyGraphs` — Nova allows a named graph to exist with zero triples
    // (e.g. after Graph Store Protocol `PUT` with an empty body, or `CREATE`),
    // matching upstream. No `sd:UnionDefaultGraph`: Nova has no server-level
    // "union default graph" toggle (upstream's `--union-default-graph` CLI
    // flag) — `FROM`/`FROM NAMED`-driven union semantics are a per-query RDF
    // Dataset construction detail (SPARQL spec, not a `sd:feature`).
    graph.push(Triple::new(root.clone(), sd::feature(), sd::empty_graphs()));

    let entailment_regime = if reasoning_enabled {
        "http://www.w3.org/ns/entailment/OWL-RL"
    } else {
        "http://www.w3.org/ns/entailment/Simple"
    };
    graph.push(Triple::new(
        root.clone(),
        sd::default_entailment_regime(),
        NamedNode::new_unchecked(entailment_regime),
    ));

    if fulltext_enabled {
        for local in ["query", "contains"] {
            graph.push(Triple::new(
                root.clone(),
                sd::extension_function(),
                NamedNode::new_unchecked(format!("{TEXT_FN_NS}{local}")),
            ));
        }
    }

    #[cfg(feature = "geosparql")]
    if geosparql_enabled {
        for (name, _) in spargeo::GEOSPARQL_EXTENSION_FUNCTIONS {
            graph.push(Triple::new(
                root.clone(),
                sd::extension_function(),
                name.into_owned(),
            ));
        }
    }
    #[cfg(not(feature = "geosparql"))]
    let _ = geosparql_enabled;

    graph
}
