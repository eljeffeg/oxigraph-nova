//! Tantivy full-text indexing glue, gated behind the `fulltext` cargo
//! feature — the only place in this crate that touches
//! `oxigraph-nova-fulltext`.
//!
//! ## What gets indexed
//!
//! Only quads whose *object* is a literal (string-typed or lang-tagged) are
//! indexed — non-literal objects (IRIs, blank nodes) have no meaningful
//! free-text content and are silently skipped.
//!
//! ## Consistency model
//!
//! See [`oxigraph_nova_core::TextSearch`]'s module docs. In short: the
//! index is updated incrementally, once per [`crate::store::LoudsStoreInner::compact_locked`]
//! call, by walking exactly the delta entries that compaction is already
//! merging into the Ring — never on the write hot path.
//!
//! ## Generation marker
//!
//! For a persistent store, `<data_dir>/fulltext/GENERATION` records the
//! `snapshot_gen` the Tantivy index was last brought up to date with. If it
//! doesn't match the store's current `snapshot_gen` at
//! [`crate::store::LoudsStore::enable_fulltext`] time (e.g. the feature is
//! being turned on for a pre-existing database, or a crash happened between
//! the Ring snapshot commit and the Tantivy `.commit()`), a one-time full
//! rebuild is triggered by walking every graph's `spo_triples()` rather than
//! silently serving a stale or empty index.

use crate::ring::GraphRingHandle;
use oxigraph_nova_core::{Dictionary, GraphId, Oxigraph, Term, TermId};
use oxigraph_nova_fulltext::FulltextIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Subdirectory (under a persistent store's data dir) holding the Tantivy
/// index files.
pub fn index_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("fulltext")
}

fn marker_path(data_dir: &Path) -> PathBuf {
    index_dir(data_dir).join("GENERATION")
}

/// Read the snapshot generation the fulltext index was last synced to, if
/// any (absent or unparseable ⇒ `None`, treated as "needs rebuild").
pub fn read_marker(data_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(marker_path(data_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Atomically record that the fulltext index now reflects `generation`.
pub fn write_marker(data_dir: &Path, generation: u64) -> Result<(), Oxigraph> {
    let dir = index_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let path = marker_path(data_dir);
    let tmp = dir.join("GENERATION.tmp");
    std::fs::write(&tmp, generation.to_string())
        .map_err(|e| Oxigraph::Storage(format!("fulltext marker write failed: {e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| Oxigraph::Storage(format!("fulltext marker rename failed: {e}")))?;
    Ok(())
}

/// If `term` is a literal, return its `(value, language)` — the pair
/// actually indexed. `None` for any non-literal term (IRIs, blank nodes),
/// which are simply not indexed.
fn literal_text(term: &Term) -> Option<(String, Option<String>)> {
    match term {
        Term::Literal(lit) => Some((
            lit.value().to_string(),
            lit.language().map(|l| l.to_string()),
        )),
        _ => None,
    }
}

/// Index a single inserted quad, if its object is a literal. Safe no-op
/// (returns `Ok(())`) if the object `TermId` can't be decoded (shouldn't
/// happen in practice) or isn't a literal.
pub fn index_quad_insert(
    ft: &FulltextIndex,
    dict: &Dictionary,
    g_id: GraphId,
    s_id: TermId,
    p_id: TermId,
    o_id: TermId,
) -> Result<(), Oxigraph> {
    let Some(obj) = dict.get_term_arc(o_id) else {
        return Ok(());
    };
    let Some((text, lang)) = literal_text(&obj) else {
        return Ok(());
    };
    let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);
    ft.add_literal(
        key,
        s_id.as_u64(),
        p_id.as_u64(),
        o_id.as_u64(),
        g_id.as_u8(),
        &text,
        lang.as_deref(),
    )
    .map_err(|e| Oxigraph::Storage(format!("fulltext add_literal failed: {e}")))
}

/// Remove a quad's document from the fulltext index (a safe no-op if it was
/// never indexed, e.g. its object wasn't a literal).
pub fn index_quad_remove(
    ft: &FulltextIndex,
    g_id: GraphId,
    s_id: TermId,
    p_id: TermId,
    o_id: TermId,
) -> Result<(), Oxigraph> {
    let key = Dictionary::pack_quad(g_id, s_id, p_id, o_id);
    ft.remove_by_key(key)
        .map_err(|e| Oxigraph::Storage(format!("fulltext remove_by_key failed: {e}")))
}

/// Full rebuild from external SPO triples (backend-agnostic).
///
/// Walks every `(GraphId, triples)` pair and (re-)indexes every literal
/// object. Used by both LOUDS (`spo_triples()`) and Ring
/// (`enumerate_spo_external()`) when the generation marker is missing/stale
/// at `enable_fulltext` time.
///
/// Deliberately does **not** call `ft.commit()`; the caller does that once.
///
/// Clears the index first (buffered — not visible until the caller's
/// `commit()`), so a rebuild against an index that already has *some*
/// documents starts from a clean slate instead of duplicating every
/// re-indexed literal alongside its stale copy.
pub fn rebuild_from_spo_triples<I, T>(
    ft: &FulltextIndex,
    dict: &Dictionary,
    graphs: I,
) -> Result<(), Oxigraph>
where
    I: IntoIterator<Item = (GraphId, T)>,
    T: IntoIterator<Item = [u64; 3]>,
{
    ft.clear()
        .map_err(|e| Oxigraph::Storage(format!("fulltext clear (rebuild) failed: {e}")))?;
    for (g_id, triples) in graphs {
        for [s, p, o] in triples {
            let (s_id, p_id, o_id) = (
                TermId::new(s).map_err(|_| Oxigraph::IdSpaceExhausted)?,
                TermId::new(p).map_err(|_| Oxigraph::IdSpaceExhausted)?,
                TermId::new(o).map_err(|_| Oxigraph::IdSpaceExhausted)?,
            );
            index_quad_insert(ft, dict, g_id, s_id, p_id, o_id)?;
        }
    }
    Ok(())
}

/// Full rebuild: walk every LOUDS graph's current triples (post-compaction
/// Ring state) and (re-)index every literal object. Thin wrapper over
/// [`rebuild_from_spo_triples`] for the LOUDS `GraphRingHandle` map.
///
/// Deliberately does **not** call `ft.commit()`; the caller does that once.
pub(crate) fn rebuild_all(
    ft: &FulltextIndex,
    dict: &Dictionary,
    graphs: &HashMap<GraphId, GraphRingHandle>,
) -> Result<(), Oxigraph> {
    rebuild_from_spo_triples(
        ft,
        dict,
        graphs
            .iter()
            .map(|(&g_id, handle)| (g_id, handle.spo_triples())),
    )
}
