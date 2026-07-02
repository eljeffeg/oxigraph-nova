//! Shared dataset-generation logic for external comparative benchmarks.
//!
//! This mirrors the synthetic BSBM-style dataset defined in
//! `benches/bsbm_large.rs` so that Nova, Oxigraph, and QLever are all
//! benchmarked against byte-identical N-Triples input.

use oxigraph_nova_core::{GraphName, NamedNode, Quad, Subject, Term};

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
