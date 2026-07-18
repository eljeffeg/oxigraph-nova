//! Transactional isolation semantics integration test.
//!
//! Upstream Oxigraph documents a "repeatable read" isolation guarantee: any
//! single read operation, or read/write operation, observes a fixed snapshot
//! of the store for its *entire* duration, and only fully-committed changes
//! are ever visible. `oxigraph_nova_storage_ring::LoudsStore` gives per-call
//! atomicity for free (each individual `QuadStore` method call takes its
//! single `Mutex<LoudsStoreInner>` exactly once), but does **not** implement
//! that stronger whole-operation snapshot guarantee: a multi-statement
//! `Update` (or a multi-triple-pattern query) is really a *sequence* of
//! independent lock acquisitions, so a concurrent write that lands between
//! two of them is visible to the later one.
//!
//! This test demonstrates the gap directly and deterministically (using
//! channels for happens-before ordering, not sleep-based timing races): a
//! background "slow query" thread does two `quads_for_pattern` scans with a
//! deliberate pause between them (simulating a long-running multi-pattern
//! query), and a concurrent `execute_update` (`INSERT DATA`) is made to land
//! *exactly* in that pause. The second scan observes the concurrent
//! insert — proving the store does not hand out a single fixed snapshot for
//! the whole "query".
//!
//! See `crates/nova-storage-louds/src/store.rs`'s module doc comment
//! ("Isolation semantics") and `crates/nova-query/src/update.rs`'s module
//! doc comment ("Atomicity") for the corresponding design documentation.

use oxigraph_nova_core::QuadStore;
use oxigraph_nova_storage_ring::LoudsStore;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn nn(s: &str) -> NamedNode {
    NamedNode::new_unchecked(s)
}

fn quad(s: &str, o: &str) -> Quad {
    Quad::new(
        nn(s),
        nn("http://ex/p"),
        Term::Literal(Literal::new_simple_literal(o)),
        GraphName::DefaultGraph,
    )
}

#[test]
fn concurrent_update_is_visible_mid_multi_pattern_query_no_repeatable_read() {
    let store = Arc::new(LoudsStore::new());
    store.insert(&quad("http://ex/existing", "e")).unwrap();

    // Two rendezvous channels give us a deterministic happens-before
    // ordering without any sleep-based timing guesswork:
    //   1. query thread signals "I've done my first scan, about to pause"
    //   2. main thread runs the concurrent Update, then signals "done"
    //   3. query thread proceeds to its second scan only after that signal
    let (scan1_done_tx, scan1_done_rx) = mpsc::channel::<()>();
    let (update_done_tx, update_done_rx) = mpsc::channel::<()>();

    let query_store = Arc::clone(&store);
    let query_thread = std::thread::spawn(move || {
        // "First pattern" of a hypothetical multi-pattern query.
        let first: Vec<_> = query_store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        scan1_done_tx.send(()).unwrap();
        // Wait until the concurrent Update has definitely committed before
        // doing the "second pattern" scan — this is what makes the test
        // deterministic rather than a timing-dependent race.
        update_done_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // "Second pattern" of the *same* logical query.
        let second: Vec<_> = query_store
            .quads_for_pattern(None, None, None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        (first.len(), second.len())
    });

    // Wait for the query thread's first scan to complete before mutating.
    scan1_done_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // Concurrent write landing strictly between the query's two scans.
    store.insert(&quad("http://ex/new", "n")).unwrap();
    update_done_tx.send(()).unwrap();

    let (first_count, second_count) = query_thread.join().unwrap();

    assert_eq!(
        first_count, 1,
        "first scan should only see the pre-existing quad"
    );
    assert_eq!(
        second_count, 2,
        "second scan (of the SAME logical multi-pattern query) observes the quad \
         inserted after the query began — this is the absence of a repeatable-read/ \
         fixed-snapshot guarantee across multiple store calls, documented in \
         LoudsStore's module doc comment. A true repeatable-read \
         guarantee would require `second_count == first_count == 1`."
    );
}

/// Same phenomenon from the other direction: a multi-row `DELETE/INSERT
/// ... WHERE` Update (via `oxigraph_nova_query::update::execute_update`) can
/// be observed *partially applied* by a concurrent reader, because each
/// matched solution row issues its own independent `remove`/`insert` lock
/// acquisition (see `nova-query/src/update.rs`'s `delete_insert` function).
#[test]
fn concurrent_reader_can_observe_update_partially_applied() {
    use oxigraph_nova_query::update::execute_update;
    use spargebra::SparqlParser;

    let store = Arc::new(LoudsStore::new());
    // Seed 50 quads that a DELETE/INSERT ... WHERE will rewrite one-by-one.
    for i in 0..50 {
        store
            .insert(&quad(&format!("http://ex/s{i}"), "old"))
            .unwrap();
    }

    let (start_tx, start_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();

    // Reader thread: waits for the Update to start, gives it a moment to
    // get partway through its per-row remove/insert loop, then scans.
    let reader_store = Arc::clone(&store);
    let reader = std::thread::spawn(move || {
        start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let old_count = reader_store
            .quads_for_pattern(
                None,
                None,
                Some(&Term::Literal(Literal::new_simple_literal("old"))),
                None,
            )
            .unwrap()
            .count();
        let new_count = reader_store
            .quads_for_pattern(
                None,
                None,
                Some(&Term::Literal(Literal::new_simple_literal("new"))),
                None,
            )
            .unwrap()
            .count();
        release_tx.send(()).unwrap();
        (old_count, new_count)
    });

    let update_store = Arc::clone(&store);
    start_tx.send(()).unwrap();
    let update = SparqlParser::new()
        .parse_update("DELETE { ?s <http://ex/p> \"old\" } INSERT { ?s <http://ex/p> \"new\" } WHERE { ?s <http://ex/p> \"old\" }")
        .unwrap();
    execute_update(&update_store, &update).unwrap();

    let (old_count, new_count) = reader.join().unwrap();
    let _ = release_rx.recv_timeout(Duration::from_secs(5));

    // We don't assert a *specific* split (the exact timing of the reader's
    // scan relative to the update's per-row loop is inherently
    // non-deterministic — that's the whole point). What we do assert is
    // that the store remains internally consistent (every quad seen is
    // either "old" or "new", total 50 either way) and document that a
    // reader observing a mix (both old_count > 0 and new_count > 0) is
    // possible — i.e. no fixed pre-/post-Update snapshot is guaranteed.
    assert_eq!(
        old_count + new_count,
        50,
        "every seeded quad must be accounted for as either old or new — no data lost \
         or duplicated even though isolation is not repeatable-read"
    );
    // Final state after the Update completes must be fully migrated.
    let final_new = store
        .quads_for_pattern(
            None,
            None,
            Some(&Term::Literal(Literal::new_simple_literal("new"))),
            None,
        )
        .unwrap()
        .count();
    assert_eq!(
        final_new, 50,
        "after execute_update returns, all 50 rows must be migrated"
    );
}
