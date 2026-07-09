//! `oxigraph-nova-fulltext` — Tantivy-backed full-text index for Oxigraph Nova.
//!
//! One `tantivy::Index` per store (not one per predicate): all indexed
//! literal objects share a single index, with `predicate_id` (`INDEXED |
//! FAST`) as an extra filterable field. This is simpler to reason about
//! (one writer, one `.commit()` per compaction) than an index-per-predicate
//! design, and predicate filtering becomes a cheap additional term query
//! rather than an index-selection problem.
//!
//! ## Schema
//!
//! | Field | Type | Options | Purpose |
//! |---|---|---|---|
//! | `text` | text | `TEXT` | Tokenized literal object value — the searchable content |
//! | `lang` | text | `STRING \| STORED` | Literal's language tag, if any (exact-match, not tokenized) |
//! | `predicate_id` | u64 | `INDEXED \| FAST \| STORED` | `TermId::as_u64()` of the quad's predicate — cheap filter |
//! | `subject_id` | u64 | `FAST \| STORED` | `TermId::as_u64()` of the quad's subject |
//! | `object_id` | u64 | `INDEXED \| FAST \| STORED` | `TermId::as_u64()` of the quad's (literal) object — indexed so a targeted `TermQuery` can confirm a specific object without scanning |
//! | `graph_id` | u64 | `FAST \| STORED` | `GraphId::as_u8()` of the quad's graph |
//! | `quad_key` | bytes | `INDEXED \| STORED` | Packed u128 SPOG key (big-endian bytes) — the delete-term for tombstones |
//!
//! `quad_key` is what makes incremental indexing possible: on a tombstone,
//! the caller deletes exactly the document whose `quad_key` term matches the
//! removed quad's packed key, without needing to know its Tantivy doc id.
//!
//! ## Consistency model
//!
//! See [`oxigraph_nova_core::TextSearch`]'s module docs — indexing is
//! intended to run once per LSM compaction cycle, not per write.

use oxigraph_nova_core::{TextMatch, TextSearch};
use std::path::Path;
use std::sync::Mutex;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// Memory budget (bytes) handed to each `IndexWriter`. Compaction is
/// infrequent and single-threaded (see module docs), so a modest fixed
/// budget is sufficient; this is not on any query hot path.
const WRITER_MEMORY_BUDGET: usize = 50_000_000;

struct Fields {
    text: Field,
    lang: Field,
    predicate_id: Field,
    subject_id: Field,
    object_id: Field,
    graph_id: Field,
    quad_key: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = b.add_text_field("text", TEXT);
    let lang = b.add_text_field("lang", STRING | STORED);
    let predicate_id = b.add_u64_field("predicate_id", INDEXED | FAST | STORED);
    let subject_id = b.add_u64_field("subject_id", FAST | STORED);
    let object_id = b.add_u64_field("object_id", INDEXED | FAST | STORED);

    let graph_id = b.add_u64_field("graph_id", FAST | STORED);
    let quad_key = b.add_bytes_field("quad_key", INDEXED | STORED);
    let schema = b.build();
    (
        schema,
        Fields {
            text,
            lang,
            predicate_id,
            subject_id,
            object_id,
            graph_id,
            quad_key,
        },
    )
}

/// A Tantivy-backed full-text index over RDF literal objects.
///
/// One `FulltextIndex` per store. Cheap to clone-share via `Arc` — all
/// methods take `&self` (Tantivy's `IndexWriter::add_document`/
/// `delete_term` are internally synchronized; `commit` additionally takes
/// this struct's own `Mutex` to serialize the flush).
pub struct FulltextIndex {
    #[allow(dead_code)] // kept alive for its Directory; not read directly
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
}

impl FulltextIndex {
    /// Create a fresh, purely in-memory index (mirrors `RingStore::new()` —
    /// no disk persistence).
    pub fn create_in_ram() -> anyhow::Result<Self> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        Self::from_index(index, fields)
    }

    /// Open (or create) a persistent index rooted at `dir` (created if it
    /// doesn't exist). Mirrors `RingStore::open`'s `<data_dir>/fulltext/`
    /// convention — callers pass that subdirectory directly.
    pub fn open_or_create(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = build_schema();
        let index = Index::open_or_create(tantivy::directory::MmapDirectory::open(dir)?, schema)?;
        Self::from_index(index, fields)
    }

    fn from_index(index: Index, fields: Fields) -> anyhow::Result<Self> {
        let writer: IndexWriter = index.writer(WRITER_MEMORY_BUDGET)?;
        // `Manual` reload: we control exactly when readers see new data —
        // right after each `commit()` call, via `reader.reload()` — rather
        // than polling for new segment files.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            writer: Mutex::new(writer),
            reader,
            fields,
        })
    }

    /// Index one literal object. `lang` is the literal's language tag, if
    /// any (`None` for plain/typed literals). Buffered in the writer until
    /// the next [`Self::commit`] — callers should batch an entire
    /// compaction's worth of adds/removes before committing once.
    ///
    /// Idempotent with respect to `quad_key`: any previously-indexed
    /// document for this exact key is deleted first (in the same writer
    /// batch), so re-inserting the same quad (e.g. remove-then-reinsert
    /// between compactions, or a rebuild over an already-populated index)
    /// never leaves duplicate documents behind.
    pub fn add_literal(
        &self,
        quad_key: u128,
        subject_id: u64,
        predicate_id: u64,
        object_id: u64,
        graph_id: u8,
        text: &str,
        lang: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_bytes(self.fields.quad_key, &quad_key.to_be_bytes());

        doc.add_u64(self.fields.subject_id, subject_id);
        doc.add_u64(self.fields.predicate_id, predicate_id);
        doc.add_u64(self.fields.object_id, object_id);
        doc.add_u64(self.fields.graph_id, graph_id as u64);
        doc.add_text(self.fields.text, text);
        if let Some(l) = lang {
            doc.add_text(self.fields.lang, l);
        }
        let term = Term::from_field_bytes(self.fields.quad_key, &quad_key.to_be_bytes());
        let writer = self.writer.lock().unwrap();
        writer.delete_term(term);
        writer.add_document(doc)?;
        Ok(())
    }

    /// Delete the (at most one) document previously indexed for this exact
    /// packed quad key — the tombstone counterpart to
    /// [`Self::add_literal`]. Safe to call even if no document was ever
    /// indexed for this key (e.g. a non-literal-object quad being removed).
    pub fn remove_by_key(&self, quad_key: u128) -> anyhow::Result<()> {
        let term = Term::from_field_bytes(self.fields.quad_key, &quad_key.to_be_bytes());
        let writer = self.writer.lock().unwrap();
        writer.delete_term(term);
        Ok(())
    }

    /// Flush all buffered adds/removes and make them visible to `search`.
    /// Call once per compaction cycle after all this cycle's
    /// `add_literal`/`remove_by_key` calls.
    pub fn commit(&self) -> anyhow::Result<()> {
        {
            let mut writer = self.writer.lock().unwrap();
            writer.commit()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// Delete every document currently in the index (buffered, like
    /// `add_literal`/`remove_by_key` -- not visible to `search` until the
    /// next [`Self::commit`]). Used by a full rebuild (e.g.
    /// `RingStore::enable_fulltext`'s stale-marker path) to guarantee a
    /// clean slate before re-indexing every literal object, so a rebuild
    /// over an index that already has *some* documents (e.g. a previous
    /// partial/aborted rebuild, or `enable_fulltext` being called again
    /// after the marker went stale for an already-populated index) never
    /// leaves duplicate documents behind.
    pub fn clear(&self) -> anyhow::Result<()> {
        let writer = self.writer.lock().unwrap();
        writer.delete_all_documents()?;
        Ok(())
    }

    fn doc_field_u64(doc: &TantivyDocument, field: Field) -> Option<u64> {
        doc.get_first(field).and_then(|v| v.as_u64())
    }
}

impl TextSearch for FulltextIndex {
    fn search(&self, query: &str, predicate_id: Option<u64>, limit: usize) -> Vec<TextMatch> {
        // `TopDocs::with_limit` panics (`assert_ne!(limit, 0)`) rather than
        // returning an empty collector, so a caller-supplied `limit == 0`
        // (a legitimate, if odd, request for "no results") must be guarded
        // here rather than allowed to panic across the `TextSearch` trait
        // boundary.
        if limit == 0 {
            return Vec::new();
        }

        let searcher = self.reader.searcher();
        let query_parser = QueryParser::for_index(&self.index, vec![self.fields.text]);
        let text_query: Box<dyn Query> = match query_parser.parse_query(query) {
            Ok(q) => Box::new(q),
            Err(_) => return Vec::new(),
        };

        let combined: Box<dyn Query> = if let Some(pred) = predicate_id {
            let term = Term::from_field_u64(self.fields.predicate_id, pred);
            let pred_query: Box<dyn Query> =
                Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, text_query),
                (Occur::Must, pred_query),
            ]))
        } else {
            text_query
        };

        let top_docs = match searcher.search(
            &combined,
            &tantivy::collector::TopDocs::with_limit(limit).order_by_score(),
        ) {
            Ok(docs) => docs,
            Err(_) => return Vec::new(),
        };

        top_docs
            .into_iter()
            .filter_map(|(_score, addr)| {
                let doc: TantivyDocument = searcher.doc(addr).ok()?;
                let object_id = Self::doc_field_u64(&doc, self.fields.object_id)?;
                let subject_id = Self::doc_field_u64(&doc, self.fields.subject_id)?;
                let predicate_id = Self::doc_field_u64(&doc, self.fields.predicate_id)?;
                let graph_id = Self::doc_field_u64(&doc, self.fields.graph_id)? as u8;
                Some(TextMatch {
                    object_id,
                    subject_id,
                    predicate_id,
                    graph_id,
                })
            })
            .collect()
    }

    fn text_search_ready(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_commit_search_round_trip() {
        let idx = FulltextIndex::create_in_ram().unwrap();
        idx.add_literal(1u128, 10, 20, 30, 0, "the quick brown fox", None)
            .unwrap();
        idx.add_literal(2u128, 11, 20, 31, 0, "a lazy dog sleeps", None)
            .unwrap();
        idx.commit().unwrap();

        let hits = idx.search("fox", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].object_id, 30);

        let hits = idx.search("dog", Some(20), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].object_id, 31);

        let hits = idx.search("dog", Some(999), 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn tombstone_removes_document() {
        let idx = FulltextIndex::create_in_ram().unwrap();
        idx.add_literal(42u128, 1, 2, 3, 0, "hello world", None)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("hello", None, 10).len(), 1);

        idx.remove_by_key(42u128).unwrap();
        idx.commit().unwrap();
        assert!(idx.search("hello", None, 10).is_empty());
    }

    #[test]
    fn persistent_index_round_trips_via_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let idx = FulltextIndex::open_or_create(dir.path()).unwrap();
            idx.add_literal(7u128, 1, 2, 3, 0, "persistent content", None)
                .unwrap();
            idx.commit().unwrap();
        }
        let idx2 = FulltextIndex::open_or_create(dir.path()).unwrap();
        let hits = idx2.search("persistent", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].object_id, 3);
    }

    /// Fix #1 regression: re-indexing the exact same `quad_key` (e.g. a
    /// stale-marker rebuild re-processing a quad already in the index, or a
    /// remove-then-reinsert between compactions) must replace the existing
    /// document, never leave a duplicate behind.
    #[test]
    fn reinsert_same_quad_key_replaces_not_duplicates() {
        let idx = FulltextIndex::create_in_ram().unwrap();
        idx.add_literal(99u128, 1, 2, 3, 0, "the quick brown fox", None)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("fox", None, 10).len(), 1);

        // Re-add the SAME quad_key (same document identity) without an
        // intervening remove_by_key call.
        idx.add_literal(99u128, 1, 2, 3, 0, "the quick brown fox", None)
            .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("fox", None, 10);
        assert_eq!(
            hits.len(),
            1,
            "re-inserting the same quad_key must not duplicate the document"
        );
    }

    /// Fix #2 regression: `clear()` must remove every document, so a
    /// subsequent rebuild starting from a cleared index never sees stale
    /// documents from a previous population.
    #[test]
    fn clear_removes_all_documents() {
        let idx = FulltextIndex::create_in_ram().unwrap();
        idx.add_literal(1u128, 10, 20, 30, 0, "the quick brown fox", None)
            .unwrap();
        idx.add_literal(2u128, 11, 20, 31, 0, "a lazy dog sleeps", None)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("fox", None, 10).len(), 1);
        assert_eq!(idx.search("dog", None, 10).len(), 1);

        idx.clear().unwrap();
        idx.commit().unwrap();
        assert!(idx.search("fox", None, 10).is_empty());
        assert!(idx.search("dog", None, 10).is_empty());

        // Re-populating after a clear works normally.
        idx.add_literal(1u128, 10, 20, 30, 0, "the quick brown fox", None)
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search("fox", None, 10).len(), 1);
    }
}
