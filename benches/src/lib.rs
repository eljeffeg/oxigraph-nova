//! Shared dataset-generation logic for external comparative benchmarks.
//!
//! This mirrors the synthetic BSBM-style dataset defined in
//! `benches/bsbm_large.rs` so that Nova, Oxigraph, and QLever are all
//! benchmarked against byte-identical N-Triples input.

use oxigraph_nova_core::{GraphName, Literal, NamedNode, Quad, Subject, Term};

// ── Namespace constants (mirrors bsbm_large.rs) ────────────────────────────

pub const NS_ENTITY: &str = "https://www.wikidata.org/entity/";
pub const P_P31: &str = "https://www.wikidata.org/prop/direct/P31"; // instance-of
pub const P_P131: &str = "https://www.wikidata.org/prop/direct/P131"; // located-in
pub const P_P2: &str = "https://www.wikidata.org/prop/direct/P2"; // has-feature
pub const P_REL: &str = "https://www.wikidata.org/prop/direct/related"; // related-to

/// Features per entity.  Must be >= SIDECAR_THRESH=16 to activate Nova's L-opt B.
pub const FAN_OUT_FEATURES: usize = 20;

/// Related-to edges per entity (for triangle queries).
pub const FAN_OUT_RELATED: usize = 3;

/// Number of distinct class values.
pub const N_CLASSES: usize = 50;

/// Number of distinct region values.
pub const N_REGIONS: usize = 500;

/// Size of the feature value pool.
pub const N_FEATURES: usize = 2_000;

#[inline]
pub fn entity_iri(i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}Q{i}"))
}
#[inline]
pub fn class_iri(j: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}class{j}"))
}
#[inline]
pub fn region_iri(k: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}region{k}"))
}
#[inline]
pub fn feature_iri(f: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}feature{f}"))
}

/// Total triples generated for `n` entities: n * (2 + FAN_OUT_FEATURES + FAN_OUT_RELATED).
pub fn triple_count(n: usize) -> usize {
    n * (2 + FAN_OUT_FEATURES + FAN_OUT_RELATED)
}

/// Generate `n` entities in BSBM product-feature style (see `bsbm_large.rs` for
/// the full rationale of the sidecar-activation dataset design).
pub fn generate_quads_large(n: usize) -> Vec<Quad> {
    assert!(
        n > FAN_OUT_RELATED,
        "need at least FAN_OUT_RELATED+1 entities to avoid self-edges"
    );

    let p31 = NamedNode::new_unchecked(P_P31);
    let p131 = NamedNode::new_unchecked(P_P131);
    let p_feat = NamedNode::new_unchecked(P_P2);
    let p_rel = NamedNode::new_unchecked(P_REL);
    let dg = GraphName::DefaultGraph;

    let triples_per = 2 + FAN_OUT_FEATURES + FAN_OUT_RELATED;
    let mut quads = Vec::with_capacity(n * triples_per);

    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));

        quads.push(Quad::new(
            subj.clone(),
            p31.clone(),
            Term::NamedNode(class_iri(i % N_CLASSES)),
            dg.clone(),
        ));

        quads.push(Quad::new(
            subj.clone(),
            p131.clone(),
            Term::NamedNode(region_iri(i % N_REGIONS)),
            dg.clone(),
        ));

        for k in 0..FAN_OUT_FEATURES {
            let f = (i * FAN_OUT_FEATURES + k) % N_FEATURES;
            quads.push(Quad::new(
                subj.clone(),
                p_feat.clone(),
                Term::NamedNode(feature_iri(f)),
                dg.clone(),
            ));
        }

        for j in 1..=FAN_OUT_RELATED {
            quads.push(Quad::new(
                subj.clone(),
                p_rel.clone(),
                Term::NamedNode(entity_iri((i + j) % n)),
                dg.clone(),
            ));
        }
    }
    quads
}

// ── Realistic (label/text-heavy) corpus ─────────────────────────────────────
//
// `generate_quads_large` is deliberately IRI-only and prefix-heavy so Front-
// Coding already collapses the dictionary. Real user data is dominated by:
// - near-unique natural-language labels / descriptions (low FC LCP),
// - long or opaque IRIs without a shared template,
// - a high distinct-term : triple ratio.
//
// This generator models that shape so dict size and lz4 residual can be
// measured without needing an external dump.

pub const NS_RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
pub const NS_SCHEMA_DESC: &str = "http://schema.org/description";
pub const NS_RDFS_COMMENT: &str = "http://www.w3.org/2000/01/rdf-schema#comment";
pub const NS_REAL_ENTITY: &str = "https://data.example.org/id/";

/// English-ish word pool used to build free-text literals. Real words (not
/// random letters) so lz4 sees natural-language redundancy while Front-Coding
/// still gets almost no left-anchored LCP between adjacent unique labels.
const REAL_WORDS: &[&str] = &[
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "capital",
    "city",
    "river",
    "mountain",
    "region",
    "country",
    "population",
    "language",
    "culture",
    "history",
    "museum",
    "university",
    "bridge",
    "harbor",
    "cathedral",
    "market",
    "square",
    "palace",
    "garden",
    "library",
    "station",
    "airport",
    "district",
    "province",
    "republic",
    "kingdom",
    "empire",
    "federation",
    "coast",
    "island",
    "valley",
    "forest",
    "desert",
    "lake",
    "ocean",
    "climate",
    "economy",
    "industry",
    "agriculture",
    "tourism",
    "architecture",
    "monument",
    "festival",
    "tradition",
    "cuisine",
    "music",
    "literature",
    "science",
    "technology",
    "education",
    "healthcare",
    "transport",
    "railway",
    "highway",
    "port",
    "border",
    "alliance",
    "treaty",
    "revolution",
    "independence",
    "constitution",
    "parliament",
    "government",
    "mayor",
    "governor",
    "president",
    "minister",
    "citizen",
    "resident",
    "visitor",
    "merchant",
    "artisan",
    "scholar",
    "explorer",
    "navigator",
    "cartographer",
    "historian",
    "philosopher",
    "poet",
    "composer",
    "painter",
    "sculptor",
    "architect",
    "engineer",
    "inventor",
    "mathematician",
    "astronomer",
    "biologist",
    "chemist",
    "physicist",
    "geologist",
    "botanist",
    "zoologist",
];

/// Opaque-looking hex nibble alphabet for UUID-style entity IRIs (no shared
/// template suffix — only the fixed host prefix is FC-friendly).
fn hex_nibble(n: u8) -> char {
    b"0123456789abcdef"[n as usize] as char
}

/// Deterministic pseudo-UUID string from entity index (16 hex groups, no
/// dashes — enough entropy that adjacent entities share almost no suffix).
fn opaque_id(i: usize) -> String {
    // Mix i across 32 hex chars with a cheap LCG-ish scramble so consecutive
    // ids do not share long common prefixes beyond the host.
    let mut x = (i as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xA5A5_5A5A);
    let mut s = String::with_capacity(32);
    for _ in 0..32 {
        s.push(hex_nibble((x & 0xf) as u8));
        x = x
            .rotate_right(5)
            .wrapping_mul(0x85EB_CA6B)
            .wrapping_add(i as u64);
    }
    s
}

/// Build a free-text English-like sentence of ~`word_count` words, unique per
/// `(seed, salt)` so labels/descriptions are near-unique across entities.
fn free_text(seed: usize, salt: u64, word_count: usize) -> String {
    let mut x = (seed as u64)
        .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        .wrapping_add(salt);
    let mut out = String::with_capacity(word_count * 8);
    for k in 0..word_count {
        if k > 0 {
            out.push(' ');
        }
        let idx = (x as usize) % REAL_WORDS.len();
        out.push_str(REAL_WORDS[idx]);
        // Advance state; inject k so runs don't cycle short.
        x = x
            .wrapping_mul(0x1656_67B1)
            .wrapping_add(0x27D4_EB2F + k as u64);
    }
    // Capitalize first letter for mild natural-language shape.
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out.push('.');
    out
}

/// Triple count for the realistic generator: per entity
/// `p31 + p131 + label + description + comment + FAN_OUT_FEATURES + FAN_OUT_RELATED`.
pub fn triple_count_realistic(n: usize) -> usize {
    n * (5 + FAN_OUT_FEATURES + FAN_OUT_RELATED)
}

/// Generate `n` entities with **realistic dictionary pressure**:
/// - opaque UUID-style subject IRIs (low FC LCP beyond host),
/// - one unique `rdfs:label` (~6 words, lang=en) per entity,
/// - one unique `schema:description` (~40 words, lang=en) per entity,
/// - one unique `rdfs:comment` (~20 words) per entity,
/// - plus the same structural edges as [`generate_quads_large`] so the index
///   still has comparable shape.
///
/// Distinct terms ≈ `n` entities + ~`3n` unique literals + small class/region/
/// feature pools + a few predicates — dictionary bytes scale ~linearly with
/// `n` and are dominated by free text (poor Front-Coding, good lz4).
pub fn generate_quads_realistic(n: usize) -> Vec<Quad> {
    assert!(
        n > FAN_OUT_RELATED,
        "need at least FAN_OUT_RELATED+1 entities to avoid self-edges"
    );

    let p31 = NamedNode::new_unchecked(P_P31);
    let p131 = NamedNode::new_unchecked(P_P131);
    let p_feat = NamedNode::new_unchecked(P_P2);
    let p_rel = NamedNode::new_unchecked(P_REL);
    let p_label = NamedNode::new_unchecked(NS_RDFS_LABEL);
    let p_desc = NamedNode::new_unchecked(NS_SCHEMA_DESC);
    let p_comment = NamedNode::new_unchecked(NS_RDFS_COMMENT);
    let dg = GraphName::DefaultGraph;

    let triples_per = 5 + FAN_OUT_FEATURES + FAN_OUT_RELATED;
    let mut quads = Vec::with_capacity(n * triples_per);

    for i in 0..n {
        // Opaque entity IRI — only host is shared; suffix is high-entropy.
        let entity = NamedNode::new_unchecked(format!("{NS_REAL_ENTITY}{}", opaque_id(i)));
        let subj = Subject::NamedNode(entity);

        quads.push(Quad::new(
            subj.clone(),
            p31.clone(),
            Term::NamedNode(class_iri(i % N_CLASSES)),
            dg.clone(),
        ));

        quads.push(Quad::new(
            subj.clone(),
            p131.clone(),
            Term::NamedNode(region_iri(i % N_REGIONS)),
            dg.clone(),
        ));

        // Unique label (~6 words, language-tagged).
        let label = free_text(i, 0x4C42_4C00, 6); // "LBL\0"
        quads.push(Quad::new(
            subj.clone(),
            p_label.clone(),
            Term::Literal(Literal::new_language_tagged_literal_unchecked(label, "en")),
            dg.clone(),
        ));

        // Unique description (~40 words) — the main dict-byte driver.
        let desc = free_text(i, 0x4445_5343, 40); // "DESC"
        quads.push(Quad::new(
            subj.clone(),
            p_desc.clone(),
            Term::Literal(Literal::new_language_tagged_literal_unchecked(desc, "en")),
            dg.clone(),
        ));

        // Unique comment (~20 words).
        let comment = free_text(i, 0x434D_5400, 20); // "CMT\0"

        quads.push(Quad::new(
            subj.clone(),
            p_comment.clone(),
            Term::Literal(Literal::new_simple_literal(comment)),
            dg.clone(),
        ));

        for k in 0..FAN_OUT_FEATURES {
            let f = (i * FAN_OUT_FEATURES + k) % N_FEATURES;
            quads.push(Quad::new(
                subj.clone(),
                p_feat.clone(),
                Term::NamedNode(feature_iri(f)),
                dg.clone(),
            ));
        }

        for j in 1..=FAN_OUT_RELATED {
            let other =
                NamedNode::new_unchecked(format!("{NS_REAL_ENTITY}{}", opaque_id((i + j) % n)));
            quads.push(Quad::new(
                subj.clone(),
                p_rel.clone(),
                Term::NamedNode(other),
                dg.clone(),
            ));
        }
    }
    quads
}

/// Write a triple (default graph, so N-Triples not N-Quads) to `w` in N-Triples syntax.
pub fn write_nt_line<W: std::io::Write>(w: &mut W, q: &Quad) -> std::io::Result<()> {
    let s = match &q.subject {
        Subject::NamedNode(n) => format!("<{}>", n.as_str()),
        Subject::BlankNode(b) => format!("_:{}", b.as_str()),
        #[allow(unreachable_patterns)]
        _ => panic!("unsupported subject term in synthetic dataset"),
    };
    let p = format!("<{}>", q.predicate.as_str());
    let o = match &q.object {
        Term::NamedNode(n) => format!("<{}>", n.as_str()),
        Term::BlankNode(b) => format!("_:{}", b.as_str()),
        Term::Literal(l) => {
            format!(
                "\"{}\"",
                l.value().replace('\\', "\\\\").replace('"', "\\\"")
            )
        }
        #[allow(unreachable_patterns)]
        _ => panic!("unsupported object term in synthetic dataset"),
    };
    writeln!(w, "{s} {p} {o} .")
}
