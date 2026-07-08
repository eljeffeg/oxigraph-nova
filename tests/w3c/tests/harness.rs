//! W3C SPARQL 1.1 / 1.2 conformance test harness.
//!
//! Fetches the official W3C test suites on first run and caches files under
//! `tests/w3c/data/` (gitignored) — no separate download script needed;
//! `fetch_cached` lazily downloads each file the first time a test needs it.
//!
//! Run everything:               `cargo test -p oxigraph-nova-w3c-harness -- --nocapture`
//! Run only SPARQL 1.1:          `cargo test -p oxigraph-nova-w3c-harness w3c_sparql11_query -- --nocapture`
//! Run only SPARQL 1.2:          `cargo test -p oxigraph-nova-w3c-harness w3c_sparql12_query -- --nocapture`
//!
//! Env vars:
//!   OXIGRAPH_W3C_OFFLINE=1        — use cached data only; fail if a file is missing
//!   OXIGRAPH_W3C11_FAIL=1         — hard-fail (non-zero exit) on unexpected SPARQL 1.1 failures (CI)
//!   OXIGRAPH_W3C12_FAIL=1         — same, for the SPARQL 1.2 Working Draft suite (off by default)
//!   OXIGRAPH_W3C_DEBUG=1          — print got/expected triples on CONSTRUCT mismatches
//!   OXIGRAPH_W3C_SPAREVAL_ORACLE=1 — cross-check Nova's evaluator output against spareval
//!                                    (Oxigraph's own nested-loop/hash-join evaluator) on the
//!                                    same store/query for every QueryEval test case. Purely
//!                                    diagnostic: disagreements are reported in the summary but
//!                                    never turn a PASS into a FAIL.

use anyhow::{Context, Result, anyhow};
use oxigraph_nova_core::{GraphName, Oxigraph, Quad, QuadStore};
use oxigraph_nova_query::{Evaluator, QueryResult, Solution, StoreDataset};
use oxigraph_nova_storage_ring::RingStore;
use oxrdf::{Dataset as OxrdfDataset, NamedNode, NamedOrBlankNode, Term, Triple, Variable};
use oxttl::{NTriplesParser, TriGParser, TurtleParser};
use sparesults::{QueryResultsFormat, QueryResultsParser, ReaderQueryResultsParserOutput};
use spargebra::SparqlParser;
use spareval::{QueryEvaluator as SparevalEvaluator, QueryResults as SparevalResults};

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

// ── Vocabulary helpers ────────────────────────────────────────────────────────

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const QT: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

fn nn(iri: impl Into<String>) -> Term {
    Term::NamedNode(NamedNode::new_unchecked(iri.into()))
}
fn mf(local: &str) -> Term {
    nn(format!("{MF}{local}"))
}
fn qt(local: &str) -> Term {
    nn(format!("{QT}{local}"))
}
fn rdf(local: &str) -> Term {
    nn(format!("{RDF}{local}"))
}
fn rdfs(local: &str) -> Term {
    nn(format!("{RDFS}{local}"))
}

// ── W3C test suite URLs ───────────────────────────────────────────────────────

// Common root for both the SPARQL 1.1 and SPARQL 1.2 test suites — the local
// cache mirrors the upstream path structure below this root (e.g.
// `sparql11/...` and `sparql12/...` coexist without collision under
// `tests/w3c/data/`).
const W3C_TESTS_ROOT: &str = "https://w3c.github.io/rdf-tests/sparql/";

const SPARQL11_QUERY_MANIFEST: &str =
    "https://w3c.github.io/rdf-tests/sparql/sparql11/manifest-sparql11-query.ttl";

// SPARQL 1.2 is still a W3C Working Draft — the suite is fetched and run for
// visibility, but (unlike the 1.1 suite) it does not gate CI by default; see
// `OXIGRAPH_W3C12_FAIL`.
const SPARQL12_MANIFEST: &str = "https://w3c.github.io/rdf-tests/sparql/sparql12/manifest.ttl";

// ── Simple in-memory RDF graph (for manifest navigation) ─────────────────────

#[derive(Default)]

struct RdfGraph {
    /// subject_key → predicate_key → [object_key]
    spo: HashMap<String, HashMap<String, Vec<String>>>,
    /// predicate_key → object_key → [subject_key]
    pos: HashMap<String, HashMap<String, Vec<String>>>,
    /// interned key → owned Term
    terms: HashMap<String, Term>,
}

impl RdfGraph {
    fn intern(&mut self, t: &Term) -> String {
        let k = term_key(t);
        self.terms.entry(k.clone()).or_insert_with(|| t.clone());
        k
    }

    fn add(&mut self, s: &Term, p: &Term, o: &Term) {
        let sk = self.intern(s);
        let pk = self.intern(p);
        let ok = self.intern(o);
        self.spo
            .entry(sk.clone())
            .or_default()
            .entry(pk.clone())
            .or_default()
            .push(ok.clone());
        self.pos
            .entry(pk)
            .or_default()
            .entry(ok)
            .or_default()
            .push(sk);
    }

    fn objects(&self, s: &Term, p: &Term) -> Vec<Term> {
        let sk = term_key(s);
        let pk = term_key(p);
        self.spo
            .get(&sk)
            .and_then(|ps| ps.get(&pk))
            .map(|os| {
                os.iter()
                    .filter_map(|ok| self.terms.get(ok).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn object_first(&self, s: &Term, p: &Term) -> Option<Term> {
        self.objects(s, p).into_iter().next()
    }

    #[allow(dead_code)]
    fn subjects_with_type(&self, t: &Term) -> Vec<Term> {
        let rdf_type_k = term_key(&rdf("type"));
        let tk = term_key(t);
        self.pos
            .get(&rdf_type_k)
            .and_then(|os| os.get(&tk))
            .map(|ss| {
                ss.iter()
                    .filter_map(|sk| self.terms.get(sk).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn term_key(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => format!("<{}>", n.as_str()),
        Term::BlankNode(b) => format!("_:{}", b.as_str()),
        Term::Literal(l) => l.to_string(),
        #[allow(unreachable_patterns)]
        _ => t.to_string(),
    }
}

fn rdf_list(g: &RdfGraph, head: &Term) -> Vec<Term> {
    let nil = rdf("nil");
    let first = rdf("first");
    let rest = rdf("rest");
    let mut out = Vec::new();
    let mut cur = head.clone();
    loop {
        if cur == nil {
            break;
        }
        if let Some(v) = g.object_first(&cur, &first) {
            out.push(v);
        }
        match g.object_first(&cur, &rest) {
            Some(r) => cur = r,
            None => break,
        }
    }
    out
}

// ── HTTP fetch + local cache ──────────────────────────────────────────────────

fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

/// Download `url` once and store it in `data/<relative-path>`.
/// Subsequent calls return the cached path immediately.
fn fetch_cached(url: &str) -> Result<PathBuf> {
    let relative = url
        .strip_prefix(W3C_TESTS_ROOT)
        .ok_or_else(|| anyhow!("URL not under W3C_TESTS_ROOT: {url}"))?;
    let local = cache_dir().join(relative);

    if local.exists() {
        return Ok(local);
    }
    if std::env::var("OXIGRAPH_W3C_OFFLINE").is_ok() {
        return Err(anyhow!("offline mode; file not cached: {url}"));
    }
    fs::create_dir_all(local.parent().unwrap())
        .with_context(|| format!("mkdir {}", local.parent().unwrap().display()))?;

    let body = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("read body for {url}"))?;

    fs::write(&local, &body).with_context(|| format!("write {}", local.display()))?;
    Ok(local)
}

fn read_url(url: &str) -> Result<String> {
    let path = fetch_cached(url)?;
    fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
}

// ── Turtle parsing into RdfGraph ──────────────────────────────────────────────

fn parse_ttl_to_graph(content: &str, base: &str) -> Result<RdfGraph> {
    let mut g = RdfGraph::default();
    let parser = TurtleParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow!("bad base IRI {base}: {e}"))?;
    for res in parser.for_reader(content.as_bytes()) {
        let t = res.map_err(|e| anyhow!("Turtle error: {e}"))?;
        g.add(
            &Term::from(t.subject),
            &Term::NamedNode(t.predicate),
            &t.object,
        );
    }
    Ok(g)
}

// ── Manifest walking ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum TestKind {
    QueryEval,
    PosSyntax,
    NegSyntax,
    /// `mf:PositiveUpdateSyntaxTest` — action is a `.ru` file that must parse
    /// as a SPARQL Update (SPARQL 1.2 `syntax-triple-terms-*` manifests).
    PosUpdateSyntax,
    /// `mf:NegativeUpdateSyntaxTest` — action is a `.ru` file that must fail
    /// to parse as a SPARQL Update.
    NegUpdateSyntax,
    Other,
}

#[derive(Debug, Clone)]
struct TestCase {
    name: String,
    kind: TestKind,
    query_url: Option<String>,
    data_urls: Vec<String>,            // default-graph files
    named_urls: Vec<(String, String)>, // (graph-IRI, file-URL)
    result_url: Option<String>,
}

fn nn_str(t: &Term) -> Option<String> {
    if let Term::NamedNode(n) = t {
        Some(n.as_str().to_string())
    } else {
        None
    }
}
fn lit_str(t: &Term) -> Option<String> {
    if let Term::Literal(l) = t {
        Some(l.value().to_string())
    } else {
        None
    }
}

fn walk_manifest(manifest_url: &str) -> Result<Vec<TestCase>> {
    let content = match read_url(manifest_url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  [skip] {manifest_url}: {e}");
            return Ok(vec![]);
        }
    };
    let g = parse_ttl_to_graph(&content, manifest_url)?;

    let mut all = Vec::new();

    // Find the manifest root node. SPARQL 1.1 manifests use `<>` as the
    // subject, which Turtle resolves against `base` (== manifest_url) to
    // exactly `manifest_url` — so the direct URL match works. SPARQL 1.2
    // manifests instead use a named subject (e.g. `trs:manifest`, which
    // resolves to a `#`-fragment IRI distinct from the bare manifest URL),
    // so fall back to searching for whichever subject is typed
    // `mf:Manifest` when the direct match comes up empty.
    let direct_root = nn(manifest_url);
    let root = if g.object_first(&direct_root, &mf("include")).is_some()
        || g.object_first(&direct_root, &mf("entries")).is_some()
    {
        direct_root
    } else {
        g.subjects_with_type(&mf("Manifest"))
            .into_iter()
            .next()
            .unwrap_or(direct_root)
    };

    if let Some(head) = g.object_first(&root, &mf("include")) {
        for sub in rdf_list(&g, &head) {
            if let Some(sub_url) = nn_str(&sub) {
                all.extend(walk_manifest(&sub_url)?);
            }
        }
    }

    // Collect mf:entries.
    if let Some(head) = g.object_first(&root, &mf("entries")) {
        for entry in rdf_list(&g, &head) {
            if let Some(tc) = extract_test(&g, &entry) {
                all.push(tc);
            }
        }
    }

    Ok(all)
}

fn extract_test(g: &RdfGraph, entry: &Term) -> Option<TestCase> {
    let rdf_type = rdf("type");
    let types = g.objects(entry, &rdf_type);

    let kind = if types.contains(&mf("QueryEvaluationTest")) {
        TestKind::QueryEval
    } else if types.contains(&mf("PositiveSyntaxTest11"))
        || types.contains(&nn(format!("{MF}PositiveSyntaxTest")))
    {
        TestKind::PosSyntax
    } else if types.contains(&mf("NegativeSyntaxTest11"))
        || types.contains(&nn(format!("{MF}NegativeSyntaxTest")))
    {
        TestKind::NegSyntax
    } else if types.contains(&mf("PositiveUpdateSyntaxTest11"))
        || types.contains(&nn(format!("{MF}PositiveUpdateSyntaxTest")))
    {
        TestKind::PosUpdateSyntax
    } else if types.contains(&mf("NegativeUpdateSyntaxTest11"))
        || types.contains(&nn(format!("{MF}NegativeUpdateSyntaxTest")))
    {
        TestKind::NegUpdateSyntax
    } else {
        TestKind::Other
    };

    let name = lit_str(&g.object_first(entry, &mf("name"))?)?;

    let action = g.object_first(entry, &mf("action"))?;

    // For PosSyntax/NegSyntax tests the action IS the query file (a NamedNode).
    // For QueryEval tests the action is a blank node with qt:query / qt:data etc.
    let query_url = if let Term::NamedNode(_) = &action {
        nn_str(&action)
    } else {
        g.object_first(&action, &qt("query"))
            .and_then(|t| nn_str(&t))
    };
    let result_url = g
        .object_first(entry, &mf("result"))
        .and_then(|t| nn_str(&t));

    let data_urls: Vec<String> = g
        .objects(&action, &qt("data"))
        .into_iter()
        .filter_map(|t| nn_str(&t))
        .collect();

    let mut named_urls = Vec::new();
    for gd in g.objects(&action, &qt("graphData")) {
        match &gd {
            Term::NamedNode(n) => {
                // File URL doubles as graph IRI.
                named_urls.push((n.as_str().to_string(), n.as_str().to_string()));
            }
            Term::BlankNode(_) => {
                let file = g.object_first(&gd, &qt("graph")).and_then(|t| nn_str(&t));
                let iri_from_label = g
                    .object_first(&gd, &rdfs("label"))
                    .and_then(|t| lit_str(&t));
                let iri_from_nn = g.object_first(&gd, &qt("graph")).and_then(|t| nn_str(&t));
                let graph_iri = iri_from_label.or(iri_from_nn);
                if let (Some(f), Some(g_iri)) = (file, graph_iri) {
                    named_urls.push((g_iri, f));
                }
            }
            _ => {}
        }
    }

    Some(TestCase {
        name,
        kind,
        query_url,
        data_urls,
        named_urls,
        result_url,
    })
}

// ── Data loading ──────────────────────────────────────────────────────────────

fn load_into_store<S: QuadStore>(store: &S, url: &str, graph: &GraphName) -> Result<Vec<Quad>> {
    // Register the named graph up front — this ensures that even empty graphs
    // (Turtle files with no triples) are returned by named_graphs() and
    // therefore participate in GRAPH ?g enumeration.
    store
        .register_named_graph(graph)
        .map_err(|e| anyhow!("register graph: {e}"))?;
    let content = read_url(url)?;


    // TriG is a distinct format from Turtle: it adds `GRAPH <iri> { ... }`
    // blocks for defining multiple named graphs (plus default-graph triples)
    // within a single file. `TurtleParser` has no notion of these blocks and
    // errors out on the `GRAPH` keyword, so `.trig` files need their own
    // dispatch to `TriGParser`, which yields `Quad`s (each already carrying
    // its own graph name — `GraphName::DefaultGraph` for triples outside any
    // `GRAPH { }` block) rather than plain `Triple`s. Since a single TriG
    // file may define several distinct named graphs, each quad is inserted
    // using its *own* parsed graph name (registering that graph first),
    // rather than being forced into the single `graph` parameter the caller
    // passed in — that parameter only matters for the plain
    // Turtle/N-Triples/RDF-XML formats below, which have no graph notion of
    // their own.
    if url.ends_with(".trig") {
        let quads: Vec<Quad> = TriGParser::new()
            .with_base_iri(url)
            .map_err(|e| anyhow!("{e}"))?
            .for_reader(content.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("{e}"))?
            .into_iter()
            .map(|q: oxrdf::Quad| Quad::new(q.subject, q.predicate, q.object, q.graph_name))
            .collect();
        for q in &quads {
            store
                .register_named_graph(&q.graph_name)
                .map_err(|e| anyhow!("register graph: {e}"))?;
        }
        for q in &quads {
            store.insert(q)?;
        }
        return Ok(quads);
    }


    // Dispatch by file extension: most W3C SPARQL 1.1 test fixtures are
    // Turtle, but the subquery (`sq0*`) tests ship `.rdf` (RDF/XML) data
    // files, which `TurtleParser` cannot parse (RDF/XML is not Turtle — it
    // errors as an IRI-position parse failure on the first newline inside
    // the `<rdf:RDF ...>` opening tag). `.nt` (N-Triples) is also its own
    // format. Route each extension to the correct parser rather than
    // guessing/falling back.
    let triples: Vec<Triple> = if url.ends_with(".nt") {
        NTriplesParser::new()
            .for_reader(content.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("{e}"))?
    } else if url.ends_with(".rdf") {
        oxrdfxml::RdfXmlParser::new()
            .with_base_iri(url)
            .map_err(|e| anyhow!("{e}"))?
            .for_reader(content.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("{e}"))?
    } else {
        TurtleParser::new()
            .with_base_iri(url)
            .map_err(|e| anyhow!("{e}"))?
            .for_reader(content.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("{e}"))?
    };

    let mut quads = Vec::with_capacity(triples.len());
    for t in triples {
        let q = Quad::new(t.subject, t.predicate, t.object, graph.clone());
        store.insert(&q)?;
        quads.push(q);
    }
    Ok(quads)
}


// ── Blank-node-identity-preserving term representation ────────────────────────

/// A term representation that preserves blank-node *identity* (its original
/// label) while normalizing all other terms to a canonical string form.
///
/// Blank node *labels* are never semantically meaningful in RDF/SPARQL — two
/// results that only differ in blank node labeling must compare equal.
/// Keeping the original label around (rather than collapsing every blank
/// node to a single placeholder) lets us run proper Weisfeiler-Lehman color
/// refinement plus a verified bijection search (see `facts_isomorphic`
/// below), which correctly distinguishes *distinct* blank nodes within a
/// result instead of conflating them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum TRepr {
    /// Blank node, keyed by its original label. Only meaningful *within* the
    /// same result set — never compared directly across got/expected labels;
    /// comparison always goes through `facts_isomorphic`.
    Blank(String),
    /// Any other term, in canonical string form (IRIs, literals with
    /// value-normalized numerics/strings already applied).
    Fixed(String),
    /// RDF 1.2 quoted/embedded triple term (subject, predicate, object),
    /// each recursively represented rather than collapsed into an opaque
    /// `Fixed(string)`. A `Term::Triple` binding may contain a blank node
    /// anywhere in its structure (including nested quoted triples), and
    /// that blank node's identity must remain visible to the
    /// Weisfeiler-Lehman isomorphism machinery below, exactly like a
    /// top-level blank-node binding would be.
    Triple(Box<TRepr>, Box<TRepr>, Box<TRepr>),
}

impl TRepr {
    /// Parse a term's string form (e.g. an `rs:value` literal string) into a
    /// `TRepr`, preserving blank-node identity. Only used as a fallback for
    /// terms whose actual `oxrdf::Term` isn't directly available — see
    /// `term_to_repr` below for the preferred, structure-preserving
    /// conversion used everywhere the original `Term` is at hand (which
    /// correctly recurses into `Term::Triple` instead of stringifying it).
    fn from_str(s: &str) -> Self {
        if let Some(label) = s.strip_prefix("_:") {
            return TRepr::Blank(label.to_string());
        }
        if s.starts_with('"')
            && let Some(normalized) = normalize_typed_literal(s)
        {
            return TRepr::Fixed(normalized);
        }
        TRepr::Fixed(s.to_string())
    }
}

/// Convert an actual `oxrdf::Term` into a `TRepr`, recursively preserving
/// blank-node identity even when nested inside a `Term::Triple` (RDF 1.2
/// quoted triple). This must be called on the original `Term` (before it is
/// stringified) so the `Term::Triple` structure is available to recurse
/// into — see the doc comment on `TRepr::Triple` above for why this matters.
fn term_to_repr(t: &Term) -> TRepr {
    match t {
        Term::BlankNode(b) => TRepr::Blank(b.as_str().to_string()),
        Term::Triple(tr) => TRepr::Triple(
            Box::new(named_or_blank_to_repr(&tr.subject)),
            Box::new(TRepr::Fixed(format!("<{}>", tr.predicate.as_str()))),
            Box::new(term_to_repr(&tr.object)),
        ),
        other => TRepr::from_str(&other.to_string()),
    }
}

fn named_or_blank_to_repr(n: &NamedOrBlankNode) -> TRepr {
    match n {
        NamedOrBlankNode::NamedNode(nn) => TRepr::Fixed(format!("<{}>", nn.as_str())),
        NamedOrBlankNode::BlankNode(b) => TRepr::Blank(b.as_str().to_string()),
    }
}

/// Structured representation of a parsed `Triple`'s subject/predicate/object.
type TripleRepr = (TRepr, TRepr, TRepr);

fn triple_to_repr(t: &Triple) -> TripleRepr {
    let s = named_or_blank_to_repr(&t.subject);
    let p = TRepr::Fixed(format!("<{}>", t.predicate.as_str()));
    let o = term_to_repr(&t.object);
    (s, p, o)
}

fn dedup_triples(mut v: Vec<TripleRepr>) -> Vec<TripleRepr> {
    v.sort();
    v.dedup();
    v
}

// ── Blank-node isomorphism (Weisfeiler-Lehman refinement) ─────────────────────
//
// A "fact" generalizes both a CONSTRUCT-query result triple (roles "s"/"p"/"o")
// and a SELECT-query solution row (roles = variable names). Comparing two
// fact-sets for equality *up to blank-node relabeling* requires: (1) computing
// a structural signature for every blank node via iterative color refinement
// (Weisfeiler-Lehman), which is invariant under any consistent relabeling of
// blank nodes; (2) grouping blank nodes by signature to prune the search
// space; (3) a backtracking search for an actual bijection between got's and
// expected's blank nodes (restricted to same-signature candidates) that makes
// the two fact-sets equal as multisets once applied. Step (3) is required for
// full correctness because WL refinement is not a complete isomorphism test
// (it can fail to distinguish some highly symmetric graphs) — but with WL
// pruning first, the backtracking search only has to disambiguate within
// already-small equivalence classes, which is more than adequate for the tiny
// blank-node counts found in W3C SPARQL test data.
type Fact = Vec<(String, TRepr)>;

/// Recursively collect every blank-node label appearing anywhere in a
/// `TRepr`, including inside nested `TRepr::Triple` subject/object slots —
/// this is what makes blank nodes embedded in RDF 1.2 quoted triples visible
/// to the isomorphism machinery instead of being hidden inside an opaque
/// `Fixed(string)`.
fn collect_blanks_in(t: &TRepr, out: &mut BTreeSet<String>) {
    match t {
        TRepr::Blank(l) => {
            out.insert(l.clone());
        }
        TRepr::Fixed(_) => {}
        TRepr::Triple(s, p, o) => {
            collect_blanks_in(s, out);
            collect_blanks_in(p, out);
            collect_blanks_in(o, out);
        }
    }
}

fn blank_labels(facts: &[Fact]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in facts {
        for (_, t) in f {
            collect_blanks_in(t, &mut out);
        }
    }
    out
}

/// Structural string representation of a `TRepr` used as a neighbor-signature
/// component during WL refinement (see `wl_signatures`). Blank nodes
/// contribute their *current color* (not their raw label, which would break
/// the relabeling-invariance the whole scheme depends on); everything else
/// contributes its fixed/nested structural form.
fn repr_wl_component(t: &TRepr, colors: &HashMap<String, String>) -> String {
    match t {
        TRepr::Fixed(s) => format!("F:{s}"),
        TRepr::Blank(l) => format!("C:{}", colors.get(l).cloned().unwrap_or_default()),
        TRepr::Triple(s, p, o) => format!(
            "T({},{},{})",
            repr_wl_component(s, colors),
            repr_wl_component(p, colors),
            repr_wl_component(o, colors)
        ),
    }
}

/// Walk `t`, recording `(blank_label, path_context)` for every blank node
/// found — `path_context` encodes both the path taken to reach it (e.g.
/// `root.s`, `root.o.s` for a blank node nested as the subject of a quoted
/// triple appearing in object position) and the structural signature of
/// every *sibling* slot encountered along that path (via `repr_wl_component`
/// on colors), so blank nodes with different nesting positions or different
/// structural siblings never collapse to the same WL contribution.
fn collect_paths(
    t: &TRepr,
    path: &str,
    colors: &HashMap<String, String>,
    out: &mut Vec<(String, String)>,
) {
    match t {
        TRepr::Blank(l) => out.push((l.clone(), path.to_string())),
        TRepr::Fixed(_) => {}
        TRepr::Triple(s, p, o) => {
            let sc = repr_wl_component(s, colors);
            let pc = repr_wl_component(p, colors);
            let oc = repr_wl_component(o, colors);
            collect_paths(s, &format!("{path}.s(p={pc},o={oc})"), colors, out);
            collect_paths(o, &format!("{path}.o(s={sc},p={pc})"), colors, out);
        }
    }
}

fn hash_str(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Weisfeiler-Lehman color refinement: assign every blank node label a
/// signature string that is stable under any consistent relabeling of blank
/// nodes within `facts`. Two blank nodes (possibly in different fact-sets)
/// that play the same structural role receive identical signatures.
fn wl_signatures(facts: &[Fact]) -> HashMap<String, String> {
    let blanks = blank_labels(facts);
    if blanks.is_empty() {
        return HashMap::new();
    }
    // Initial color: identical for every blank node (unlabeled).
    let mut colors: HashMap<String, String> = blanks
        .iter()
        .map(|b| (b.clone(), "B".to_string()))
        .collect();

    // `|blanks| + 1` rounds is always sufficient for refinement to reach a
    // stable partition (standard WL bound: at most one new distinguishable
    // class can appear per round, bounded by node count).
    let rounds = blanks.len() + 1;
    for _ in 0..rounds {
        let mut neighbor_repr: HashMap<String, Vec<String>> =
            blanks.iter().map(|b| (b.clone(), Vec::new())).collect();

        for fact in facts {
            for (idx, (role, t)) in fact.iter().enumerate() {
                // Collect every blank label reachable from this (role, t)
                // pair — usually just `t` itself, but if `t` is a
                // `TRepr::Triple` containing blank nodes nested inside, each
                // of those needs its own neighbor-signature contribution
                // too (with the *other* fields of the same fact, plus its
                // position within the triple, as context).
                let mut labels_here = BTreeSet::new();
                collect_blanks_in(t, &mut labels_here);
                if labels_here.is_empty() {
                    continue;
                }
                let mut parts: Vec<String> = Vec::new();
                for (oidx, (orole, ot)) in fact.iter().enumerate() {
                    if oidx == idx {
                        continue;
                    }
                    parts.push(format!("{orole}={}", repr_wl_component(ot, &colors)));
                }
                parts.sort();
                let fact_context = format!("[{role}|{}]", parts.join(","));

                // Walk `t` itself to build a per-blank path context, so
                // blank nodes nested at different positions within a
                // `TRepr::Triple` (e.g. subject vs. object of a quoted
                // triple, or nested further inside another quoted triple)
                // get distinguishable signatures instead of all sharing the
                // same generic "found somewhere in t" contribution.
                let mut path_contexts: Vec<(String, String)> = Vec::new();
                collect_paths(t, "root", &colors, &mut path_contexts);
                for (label, path_ctx) in path_contexts {
                    neighbor_repr
                        .get_mut(&label)
                        .unwrap()
                        .push(format!("{fact_context}@{path_ctx}"));
                }
            }
        }

        let mut new_colors = HashMap::new();

        for (label, mut reprs) in neighbor_repr {
            reprs.sort();
            let old = colors.get(&label).cloned().unwrap_or_default();
            let combined = format!("{old}#{}", reprs.join(";"));
            new_colors.insert(label, hash_str(&combined));
        }
        colors = new_colors;
    }
    colors
}

/// Normalize a single fact's (role, term) pairs into a stable order so two
/// structurally-equal facts compare equal regardless of construction order.
fn normalize_fact(f: &Fact) -> Vec<(String, TRepr)> {
    let mut v = f.clone();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn multiset_eq_facts(a: &[Fact], b: &[Fact]) -> bool {
    let mut av: Vec<_> = a.iter().map(normalize_fact).collect();
    let mut bv: Vec<_> = b.iter().map(normalize_fact).collect();
    av.sort();
    bv.sort();
    av == bv
}

/// Recursively apply a blank-node relabeling to a single `TRepr`, including
/// into nested `TRepr::Triple` subject/object slots.
fn apply_mapping_to_repr(t: &TRepr, mapping: &HashMap<String, String>) -> TRepr {
    match t {
        TRepr::Blank(l) => TRepr::Blank(mapping.get(l).cloned().unwrap_or_else(|| l.clone())),
        TRepr::Fixed(s) => TRepr::Fixed(s.clone()),
        TRepr::Triple(s, p, o) => TRepr::Triple(
            Box::new(apply_mapping_to_repr(s, mapping)),
            Box::new(apply_mapping_to_repr(p, mapping)),
            Box::new(apply_mapping_to_repr(o, mapping)),
        ),
    }
}

fn apply_mapping(facts: &[Fact], mapping: &HashMap<String, String>) -> Vec<Fact> {
    facts
        .iter()
        .map(|f| {
            f.iter()
                .map(|(role, t)| (role.clone(), apply_mapping_to_repr(t, mapping)))
                .collect()
        })
        .collect()
}

/// Check whether `got` and `exp` are equal as fact-sets, up to a consistent
/// relabeling of blank nodes (a true isomorphism check, not a lossy collapse).
fn facts_isomorphic(got: &[Fact], exp: &[Fact]) -> bool {
    if got.len() != exp.len() {
        return false;
    }

    let got_blanks: Vec<String> = blank_labels(got).into_iter().collect();
    let exp_blanks_set = blank_labels(exp);
    if got_blanks.len() != exp_blanks_set.len() {
        return false;
    }
    if got_blanks.is_empty() {
        return multiset_eq_facts(got, exp);
    }

    let got_sig = wl_signatures(got);
    let exp_sig = wl_signatures(exp);

    // Group expected blank labels by signature — candidates for each got label
    // are restricted to the matching signature class, dramatically pruning
    // the backtracking search below.
    let mut exp_by_sig: HashMap<String, Vec<String>> = HashMap::new();
    for label in exp_blanks_set {
        exp_by_sig
            .entry(exp_sig.get(&label).cloned().unwrap_or_default())
            .or_default()
            .push(label);
    }

    fn go(
        i: usize,
        got_blanks: &[String],
        got_sig: &HashMap<String, String>,
        exp_by_sig: &HashMap<String, Vec<String>>,
        used: &mut BTreeSet<String>,
        mapping: &mut HashMap<String, String>,
        got: &[Fact],
        exp: &[Fact],
    ) -> bool {
        if i == got_blanks.len() {
            let mapped = apply_mapping(got, mapping);
            return multiset_eq_facts(&mapped, exp);
        }
        let label = &got_blanks[i];
        let sig = got_sig.get(label).cloned().unwrap_or_default();
        let Some(candidates) = exp_by_sig.get(&sig) else {
            return false;
        };
        for cand in candidates {
            if used.contains(cand) {
                continue;
            }
            used.insert(cand.clone());
            mapping.insert(label.clone(), cand.clone());
            if go(
                i + 1,
                got_blanks,
                got_sig,
                exp_by_sig,
                used,
                mapping,
                got,
                exp,
            ) {
                return true;
            }
            used.remove(cand);
            mapping.remove(label);
        }
        false
    }

    let mut used = BTreeSet::new();
    let mut mapping = HashMap::new();
    go(
        0,
        &got_blanks,
        &got_sig,
        &exp_by_sig,
        &mut used,
        &mut mapping,
        got,
        exp,
    )
}

fn triples_to_facts(ts: &[TripleRepr]) -> Vec<Fact> {
    ts.iter()
        .map(|(s, p, o)| {
            vec![
                ("s".to_string(), s.clone()),
                ("p".to_string(), p.clone()),
                ("o".to_string(), o.clone()),
            ]
        })
        .collect()
}

fn rows_to_facts(rows: &[BTreeMap<String, TRepr>]) -> Vec<Fact> {
    rows.iter()
        .map(|row| row.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .collect()
}

// ── Expected-result parsing ───────────────────────────────────────────────────

#[derive(Debug)]
enum Expected {
    Solutions(Vec<BTreeMap<String, TRepr>>),
    Boolean(bool),
    Triples(Vec<TripleRepr>),
}

fn parse_expected(result_url: &str) -> Result<Expected> {
    let content = read_url(result_url)?;
    let ext = result_url.rsplit('.').next().unwrap_or("");
    match ext {
        "srx" | "xml" => parse_qr_reader(content.as_bytes(), QueryResultsFormat::Xml),
        "srj" | "json" => parse_qr_reader(content.as_bytes(), QueryResultsFormat::Json),
        "tsv" => parse_qr_reader(content.as_bytes(), QueryResultsFormat::Tsv),
        "ttl" | "n3" => parse_ttl_expected(&content, result_url),
        "nt" => parse_nt_triples(&content),
        _ => Err(anyhow!("unknown result format: .{ext}")),
    }
}

fn parse_qr_reader(bytes: &[u8], fmt: QueryResultsFormat) -> Result<Expected> {
    match QueryResultsParser::from_format(fmt)
        .for_reader(bytes)
        .map_err(|e| anyhow!("{e}"))?
    {
        ReaderQueryResultsParserOutput::Solutions(mut sols) => {
            let vars: Vec<Variable> = sols.variables().to_vec();
            let mut rows = Vec::new();
            for s in &mut sols {
                let s = s.map_err(|e| anyhow!("{e}"))?;
                let mut row = BTreeMap::new();
                for v in &vars {
                    if let Some(t) = s.get(v.as_ref()) {
                        row.insert(v.as_str().to_string(), term_to_repr(t));
                    }
                }
                rows.push(row);
            }
            Ok(Expected::Solutions(rows))
        }
        ReaderQueryResultsParserOutput::Boolean(b) => Ok(Expected::Boolean(b)),
    }
}

/// Parse a TTL expected-result file. If the file encodes an rs:ResultSet,
/// return Expected::Solutions; otherwise return Expected::Triples.
fn parse_ttl_expected(content: &str, base: &str) -> Result<Expected> {
    // First parse all triples
    let triples: Vec<Triple> = TurtleParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow!("{e}"))?
        .for_reader(content.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))?;

    // Build RdfGraph for navigation
    let mut g = RdfGraph::default();
    for t in &triples {
        let s = match &t.subject {
            NamedOrBlankNode::NamedNode(n) => Term::NamedNode(n.clone()),
            NamedOrBlankNode::BlankNode(b) => Term::BlankNode(b.clone()),
        };
        g.add(&s, &Term::NamedNode(t.predicate.clone()), &t.object);
    }

    const RS: &str = "http://www.w3.org/2001/sw/DataAccess/tests/result-set#";
    let rs_result_set_type = Term::NamedNode(NamedNode::new_unchecked(format!("{}ResultSet", RS)));
    let subjects = g.subjects_with_type(&rs_result_set_type);

    if let Some(rs_node) = subjects.into_iter().next() {
        // This is an rs:ResultSet — parse as Expected::Solutions
        let rs_solution = Term::NamedNode(NamedNode::new_unchecked(format!("{}solution", RS)));
        let rs_binding = Term::NamedNode(NamedNode::new_unchecked(format!("{}binding", RS)));
        let rs_variable = Term::NamedNode(NamedNode::new_unchecked(format!("{}variable", RS)));
        let rs_value = Term::NamedNode(NamedNode::new_unchecked(format!("{}value", RS)));

        let mut rows = Vec::new();
        for sol_node in g.objects(&rs_node, &rs_solution) {
            let mut row = BTreeMap::new();
            for binding_node in g.objects(&sol_node, &rs_binding) {
                let vars = g.objects(&binding_node, &rs_variable);
                let vals = g.objects(&binding_node, &rs_value);
                if let (Some(var_t), Some(val_t)) =
                    (vars.into_iter().next(), vals.into_iter().next())
                {
                    let var_name = match &var_t {
                        Term::Literal(l) => l.value().to_string(),
                        _ => continue,
                    };
                    row.insert(var_name, term_to_repr(&val_t));
                }
            }
            if !row.is_empty() {
                rows.push(row);
            }
        }
        return Ok(Expected::Solutions(rows));
    }

    // Not an rs:ResultSet — treat as plain triples
    Ok(Expected::Triples(dedup_triples(
        triples.iter().map(triple_to_repr).collect(),
    )))
}

#[allow(dead_code)]
fn parse_ttl_triples(content: &str, base: &str) -> Result<Expected> {
    let triples: Vec<Triple> = TurtleParser::new()
        .with_base_iri(base)
        .map_err(|e| anyhow!("{e}"))?
        .for_reader(content.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))?;
    Ok(Expected::Triples(dedup_triples(
        triples.iter().map(triple_to_repr).collect(),
    )))
}

fn parse_nt_triples(content: &str) -> Result<Expected> {
    let triples: Vec<Triple> = NTriplesParser::new()
        .for_reader(content.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("{e}"))?;
    Ok(Expected::Triples(dedup_triples(
        triples.iter().map(triple_to_repr).collect(),
    )))
}

/// Normalize a typed-literal term string to a canonical form so that
/// lexically-different but value-equal terms compare equal.
fn normalize_typed_literal(s: &str) -> Option<String> {
    // Expected format: "value"^^<datatype-iri>
    let inner = s.strip_prefix('"')?;
    let caret_pos = inner.rfind("\"^^<")?;
    let value = &inner[..caret_pos];
    let datatype = inner[caret_pos + 4..].strip_suffix('>')?;
    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
    let local = datatype.strip_prefix(XSD)?;
    let canonical = match local {
        "double" | "float" => {
            let f: f64 = value.parse().ok()?;
            harness_canonical_double(f)
        }
        "decimal" => {
            let f: f64 = value.parse().ok()?;
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}.0", f as i64)
            } else {
                format!("{f}")
            }
        }
        "integer" | "long" | "int" | "short" | "byte" | "nonNegativeInteger"
        | "nonPositiveInteger" | "positiveInteger" | "negativeInteger" | "unsignedLong"
        | "unsignedInt" | "unsignedShort" | "unsignedByte" => {
            let i: i64 = value.trim().parse().ok()?;
            i.to_string()
        }
        // xsd:string and plain literals are equivalent in SPARQL 1.1 — strip the datatype
        "string" => return Some(format!("\"{}\"", value)),
        _ => return None,
    };
    Some(format!("\"{}\"^^<{}>", canonical, datatype))
}

/// Canonical XSD double format (scientific notation, uppercase E).
fn harness_canonical_double(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == f64::INFINITY {
        return "INF".to_string();
    }
    if v == f64::NEG_INFINITY {
        return "-INF".to_string();
    }
    if v == 0.0 {
        return "0.0E0".to_string();
    }
    let s = format!("{:e}", v);
    let (mantissa_str, exp_str) = s.split_once('e').unwrap();
    let exp_num: i32 = exp_str.parse().unwrap_or(0);
    let mantissa_canonical = if mantissa_str.contains('.') {
        let trimmed = mantissa_str.trim_end_matches('0');
        if trimmed.ends_with('.') {
            format!("{}0", trimmed)
        } else {
            trimmed.to_string()
        }
    } else {
        format!("{}.0", mantissa_str)
    };
    format!("{}E{}", mantissa_canonical, exp_num)
}

// ── Result comparison ─────────────────────────────────────────────────────────

fn solutions_to_rows(sols: &[Solution], vars: &[Variable]) -> Vec<BTreeMap<String, TRepr>> {
    sols.iter()
        .map(|sol| {
            let mut row = BTreeMap::new();
            for v in vars {
                if let Some(t) = sol.get(v) {
                    row.insert(v.as_str().to_string(), term_to_repr(t));
                }
            }
            row
        })
        .collect()
}

/// Cross-check Nova's own evaluator result against spareval (Oxigraph's
/// independent nested-loop/hash-join evaluator) run on the same quads/query.
/// Purely diagnostic — never affects PASS/FAIL, only prints a warning to
/// stderr on disagreement. Gated by `OXIGRAPH_W3C_SPAREVAL_ORACLE=1` at the
/// call site in `run_test`.
fn run_spareval_oracle(
    test_name: &str,
    quads: &[Quad],
    query: &spargebra::Query,
    nova_result: &QueryResult,
) {
    // oxrdf::Dataset has a blanket `spareval::QueryableDataset` impl (for
    // `&Dataset`), so mirroring the same quads already loaded into Nova's
    // store into a plain owned `oxrdf::Dataset` is all that's needed here —
    // no custom trait adapter over Nova's own `Dataset`/`QuadStore` traits.
    let dataset: OxrdfDataset = quads.iter().cloned().collect();
    let evaluator = SparevalEvaluator::new();
    let spareval_result = match evaluator.prepare(query).execute(&dataset) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[spareval-oracle] {test_name}: evaluation error: {e}");
            return;
        }
    };

    let matches = match (nova_result, spareval_result) {
        (QueryResult::Boolean(got), SparevalResults::Boolean(exp)) => *got == exp,
        (QueryResult::Solutions(sols), SparevalResults::Solutions(sol_iter)) => {
            let vars = sol_iter.variables().to_vec();
            let mut exp_rows = Vec::new();
            for sol in sol_iter {
                let sol = match sol {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[spareval-oracle] {test_name}: solution error: {e}");
                        return;
                    }
                };
                let mut row = BTreeMap::new();
                for (v, t) in sol.iter() {
                    row.insert(v.as_str().to_string(), term_to_repr(t));
                }
                exp_rows.push(row);
            }
            let got_rows = solutions_to_rows(sols, &vars);
            facts_isomorphic(&rows_to_facts(&got_rows), &rows_to_facts(&exp_rows))
        }
        (QueryResult::Triples(ts), SparevalResults::Graph(triple_iter)) => {
            let mut exp = Vec::new();
            for t in triple_iter {
                let t = match t {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("[spareval-oracle] {test_name}: triple error: {e}");
                        return;
                    }
                };
                exp.push(triple_to_repr(&t));
            }
            let exp = dedup_triples(exp);
            let got = dedup_triples(ts.iter().map(triple_to_repr).collect());
            facts_isomorphic(&triples_to_facts(&got), &triples_to_facts(&exp))
        }
        // Mismatched result shapes (e.g. Nova errored before producing a
        // comparable QueryResult) aren't directly comparable — skip silently
        // rather than reporting a spurious oracle disagreement.
        _ => true,
    };

    if !matches {
        eprintln!("[spareval-oracle] MISMATCH on {test_name}");
    }
}

fn projected_vars(query: &spargebra::Query) -> Vec<Variable> {
    use spargebra::algebra::GraphPattern;

    fn extract(p: &GraphPattern) -> Vec<Variable> {
        match p {
            GraphPattern::Project { variables, .. } => variables.clone(),
            GraphPattern::Distinct { inner } => extract(inner),
            GraphPattern::Reduced { inner } => extract(inner),
            GraphPattern::OrderBy { inner, .. } => extract(inner),
            GraphPattern::Slice { inner, .. } => extract(inner),
            _ => vec![],
        }
    }
    if let spargebra::Query::Select { pattern, .. } = query {
        extract(pattern)
    } else {
        vec![]
    }
}

// ── Individual test runner ────────────────────────────────────────────────────

/// Build a `SparqlParser` with its base IRI set to `query_url`. Per the SPARQL
/// grammar (and every W3C test query that uses a relative IRI, e.g. `<ng-01.ttl>`
/// or `<data.ttl>` in a `FROM`/`GRAPH` clause), relative IRIs in a SPARQL query
/// resolve against the query document's own URL as base — *not* against no base
/// at all. Omitting this (as a bare `SparqlParser::new()` does) makes every
/// relative-IRI-bearing query fail to parse with a misleading "expected IRI"
/// error that looks like an upstream parser limitation but is actually just a
/// missing base IRI on our side.
fn sparql_parser_for(query_url: &str) -> SparqlParser {
    SparqlParser::new()
        .with_base_iri(query_url)
        .unwrap_or_else(|_| SparqlParser::new())
}

/// A `QuadStore` that can also be freshly constructed and explicitly
/// compacted (merging its LSM delta into the read-optimized index) —
/// implemented for every Ring-family backend under test so `run_test` can be
/// generic over which storage engine is being exercised by the W3C harness.
trait TestStore: QuadStore + Default {
    fn compact_store(&self) -> Result<(), Oxigraph>;
}

impl TestStore for RingStore {
    fn compact_store(&self) -> Result<(), Oxigraph> {
        self.compact()
    }
}

fn run_test<S: TestStore + 'static>(tc: &TestCase) -> Result<bool> {
    match tc.kind {
        TestKind::PosSyntax => {
            let url = tc.query_url.as_deref().unwrap_or("");
            let qtext = read_url(url)?;
            Ok(sparql_parser_for(url).parse_query(&qtext).is_ok())
        }
        TestKind::NegSyntax => {
            let url = tc.query_url.as_deref().unwrap_or("");
            let qtext = read_url(url)?;
            Ok(sparql_parser_for(url).parse_query(&qtext).is_err())
        }
        TestKind::PosUpdateSyntax => {
            let url = tc.query_url.as_deref().unwrap_or("");
            let utext = read_url(url)?;
            Ok(sparql_parser_for(url).parse_update(&utext).is_ok())
        }
        TestKind::NegUpdateSyntax => {
            let url = tc.query_url.as_deref().unwrap_or("");
            let utext = read_url(url)?;
            Ok(sparql_parser_for(url).parse_update(&utext).is_err())
        }

        TestKind::QueryEval => {
            let query_url = tc
                .query_url
                .as_deref()
                .ok_or_else(|| anyhow!("no qt:query"))?;
            let result_url = tc
                .result_url
                .as_deref()
                .ok_or_else(|| anyhow!("no mf:result"))?;

            let qtext = read_url(query_url)?;
            let expected = parse_expected(result_url)?;

            // Build store and load data. `all_quads` mirrors everything
            // inserted into `store`, purely so the spareval oracle (below)
            // can build an independent `oxrdf::Dataset` from the exact same
            // data without a second network fetch/parse.
            let store = Arc::new(S::default());
            let mut all_quads: Vec<Quad> = Vec::new();
            for url in &tc.data_urls {
                all_quads.extend(load_into_store(store.as_ref(), url, &GraphName::DefaultGraph)?);
            }
            for (graph_iri, url) in &tc.named_urls {
                let gname = GraphName::NamedNode(NamedNode::new_unchecked(graph_iri.clone()));
                all_quads.extend(load_into_store(store.as_ref(), url, &gname)?);
            }

            // Compact the delta into the Ring so LFTJ can fire.
            // Without this, lftj_has_delta() returns true and every query falls
            // back to the nested-loop evaluator — LFTJ would never be exercised.
            store.compact_store().map_err(|e| anyhow!("compact: {e}"))?;

            // Parse and evaluate. See `sparql_parser_for` — the base IRI must
            // be the query's own URL so relative IRIs in FROM/GRAPH clauses
            // resolve correctly.
            let query = sparql_parser_for(query_url)
                .parse_query(&qtext)
                .map_err(|e| anyhow!("parse: {e}"))?;

            let ds = StoreDataset::new(Arc::clone(&store));
            let result = Evaluator::new(&ds).evaluate(&query)?;

            // Optional differential-testing oracle: cross-check Nova's own
            // result against spareval (Oxigraph's independent nested-loop/
            // hash-join evaluator) on the exact same quads/query. Purely
            // diagnostic — never affects the PASS/FAIL verdict below.
            if std::env::var("OXIGRAPH_W3C_SPAREVAL_ORACLE").is_ok() {
                run_spareval_oracle(&tc.name, &all_quads, &query, &result);
            }

            // Compare — blank-node-bearing results are compared via a true
            // isomorphism check (Weisfeiler-Lehman refinement + verified
            // bijection search), not a lossy label collapse.
            Ok(match (&result, expected) {
                (QueryResult::Boolean(got), Expected::Boolean(exp)) => *got == exp,
                (QueryResult::Solutions(sols), Expected::Solutions(exp_rows)) => {
                    let vars = projected_vars(&query);
                    let got_rows = solutions_to_rows(sols, &vars);
                    let ok = facts_isomorphic(&rows_to_facts(&got_rows), &rows_to_facts(&exp_rows));
                    if !ok && std::env::var("OXIGRAPH_W3C_DEBUG").is_ok() {
                        eprintln!("DEBUG {} got={:?}", tc.name, got_rows);
                        eprintln!("DEBUG {} exp={:?}", tc.name, exp_rows);
                    }
                    ok
                }

                (QueryResult::Triples(ts), Expected::Triples(exp)) => {
                    let got = dedup_triples(ts.iter().map(triple_to_repr).collect());
                    let ok = facts_isomorphic(&triples_to_facts(&got), &triples_to_facts(&exp));
                    if !ok && std::env::var("OXIGRAPH_W3C_DEBUG").is_ok() {
                        eprintln!("DEBUG {} got={:?}", tc.name, got);
                        eprintln!("DEBUG {} exp={:?}", tc.name, exp);
                    }
                    ok
                }

                _ => false,
            })
        }
        TestKind::Other => Ok(true), // skip unsupported test types
    }
}

// ── Main test entry point ─────────────────────────────────────────────────────

/// Result of running an entire manifest through `run_test`. Every test is
/// either a PASS or a FAIL — no further classification.
struct RunSummary {
    total: usize,
    n_pass: usize,
    /// Failing test names. `Ok(false)` (result mismatch) entries are just the
    /// test name; `Err(e)` (parse error, network issue, etc.) entries have
    /// the error text appended for diagnostic purposes.
    failures: Vec<String>,
}

/// Run every test case in `tests`, recording a plain pass/fail outcome for
/// each. Shared between the SPARQL 1.1 and SPARQL 1.2 harness entry points.
/// Generic over the storage engine under test (kept generic for possible
/// future engines even though `RingStore` is the sole production backend).
fn run_all<S: TestStore + 'static>(tests: &[TestCase]) -> RunSummary {
    let mut n_pass = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for tc in tests {
        match run_test::<S>(tc) {
            Ok(true) => {
                n_pass += 1;
            }
            Ok(false) => {
                failures.push(tc.name.clone());
            }
            Err(e) => {
                failures.push(format!("{} [err: {}]", tc.name, e));
            }
        }
    }

    RunSummary {
        total: tests.len(),
        n_pass,
        failures,
    }
}

/// Print the boxed summary + per-test FAIL lines for a `RunSummary`, under
/// the given report `title`. Returns `(n_fail, pct)` for callers that need to
/// gate CI on the result.
fn report_summary(title: &str, s: &RunSummary) -> (usize, usize) {
    let RunSummary {
        total,
        n_pass,
        failures,
    } = s;
    let n_fail = failures.len();
    let pct = (100 * n_pass).checked_div(*total).unwrap_or(0);

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════╗");
    eprintln!("║  {title:<48}║");
    eprintln!("╠══════════════════════════════════════════════════╣");
    eprintln!("║  Total tests    : {total:>5}                          ║");
    eprintln!("║  PASS           : {n_pass:>5}  ({pct:>3}%)                  ║");
    eprintln!("║  FAIL           : {n_fail:>5}                          ║");
    eprintln!("╚══════════════════════════════════════════════════╝");

    for name in failures {
        eprintln!("  FAIL   {name}");
    }

    eprintln!();
    eprintln!("{n_pass}/{total} PASS ({pct}%), {n_fail} FAIL");

    (n_fail, pct)
}

#[test]
fn w3c_sparql11_query() {
    eprintln!("\n[W3C] Loading SPARQL 1.1 query test manifest …");

    let tests = match walk_manifest(SPARQL11_QUERY_MANIFEST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[W3C] Could not fetch manifest (network unavailable?): {e}");
            eprintln!("[W3C] Run with OXIGRAPH_W3C_OFFLINE=1 after caching data, or");
            eprintln!("[W3C] make sure https://www.w3.org is reachable.");
            return; // soft-skip — don't fail if there's no network
        }
    };

    eprintln!("[W3C] {} test cases found", tests.len());

    let summary = run_all::<RingStore>(&tests);
    let (n_fail, _pct) = report_summary(
        "W3C SPARQL 1.1 Query Conformance Results (RingStore)",
        &summary,
    );

    // Hard-fail in CI only when the env var is set.
    if std::env::var("OXIGRAPH_W3C11_FAIL").is_ok() {
        assert!(n_fail == 0, "{n_fail} failures — see output above");
    }
}

/// SPARQL 1.2 is still a W3C Working Draft. This entry point downloads and
/// runs the official `sparql12` manifest tree (grouping, codepoint-escapes,
/// triple-terms syntax/eval, expression, lang-basedir, rdf11, version,
/// syntax) for visibility, mirroring the SPARQL 1.1 harness above. Unlike
/// the 1.1 suite, it does not hard-fail CI by default even when
/// `OXIGRAPH_W3C11_FAIL` is set — set `OXIGRAPH_W3C12_FAIL` instead once the
/// suite is stable enough to gate on.
///
/// A handful of tests fail due to grammar limitations in the pinned
/// `spargebra` 0.4.6 parser dependency rather than anything in this crate's
/// evaluator or harness.
#[test]
fn w3c_sparql12_query() {
    eprintln!("\n[W3C] Loading SPARQL 1.2 (Working Draft) test manifest …");

    let tests = match walk_manifest(SPARQL12_MANIFEST) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[W3C] Could not fetch manifest (network unavailable?): {e}");
            eprintln!("[W3C] Run with OXIGRAPH_W3C_OFFLINE=1 after caching data, or");
            eprintln!("[W3C] make sure https://www.w3.org is reachable.");
            return; // soft-skip — don't fail if there's no network
        }
    };

    eprintln!("[W3C] {} test cases found", tests.len());

    let summary = run_all::<RingStore>(&tests);
    let (n_fail, _pct) = report_summary(
        "W3C SPARQL 1.2 Query Conformance Results (WD, RingStore)",
        &summary,
    );

    // SPARQL 1.2 is a Working Draft — never hard-fail CI on it unless
    // explicitly opted in via a distinct env var.
    if std::env::var("OXIGRAPH_W3C12_FAIL").is_ok() {
        assert!(n_fail == 0, "{n_fail} failures — see output above");
    }
}
