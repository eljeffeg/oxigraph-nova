//! Crash-recovery integration test.
//!
//! Spawns a real `nova_serve` subprocess backed by a persistent
//! (`--location`) `RingStore`, hammers it with a burst of `INSERT DATA`
//! updates over HTTP from a background thread, `kill -9`s the process
//! *while that burst is still in flight* (simulating a real crash, not a
//! graceful shutdown), restarts a fresh `nova_serve` process pointed at the
//! same data directory, and asserts that the reloaded store contains
//! **exactly** the set of quads the client received a successful HTTP
//! response for before the kill — no more (nothing "phantom" appears) and no
//! less (nothing acknowledged is lost, modulo the single unavoidable
//! "in-doubt" boundary write explained below). This is the only way to
//! actually *prove* durability rather than assume it from the WAL/snapshot
//! design.
//!
//! Uses `std::process::Child::kill()`, which sends `SIGKILL` on Unix — the
//! same abrupt, no-cleanup termination a real `kill -9` performs (no
//! destructors run, no chance for a graceful flush).

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Total number of `INSERT DATA` requests the background thread attempts.
/// Kept modest so the test runs quickly, but large enough that the kill
/// reliably lands mid-burst rather than after the last request completes.
///
/// Bumped from 400 → 4000 after a store-side performance optimization
/// (removing a redundant sort/dedup + copy pass from `RingBuilder`
/// construction) made bulk compaction/insert throughput fast enough that
/// 400 inserts could complete well within the 60ms window below, making
/// the test spuriously fail its "kill actually landed mid-burst" sanity
/// check rather than exercising real crash-recovery behavior.
const TOTAL_INSERTS: usize = 4000;

fn nova_serve_bin() -> &'static str {
    env!("CARGO_BIN_EXE_nova_serve")
}

/// Pick a (hopefully) free local port deterministically from the PID so
/// concurrent test binaries/processes don't collide.
fn pick_port() -> u16 {
    let pid = std::process::id();
    20000 + (pid % 10000) as u16
}

/// Spawn `nova_serve --location <dir> --bind 127.0.0.1:<port>` and wait
/// until it's accepting connections (retrying an `ASK {}` probe) or panic
/// after a timeout.
fn spawn_server(dir: &std::path::Path, port: u16) -> Child {
    let mut child = Command::new(nova_serve_bin())
        .arg("--location")
        .arg(dir)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn nova_serve");

    let url = format!("http://127.0.0.1:{port}/sparql?query=ASK%20%7B%7D");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("nova_serve did not become ready within 10s");
        }
        match ureq::get(&url).call() {
            Ok(_) => break,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    child
}

/// `INSERT DATA { <http://ex/s{i}> <http://ex/p> "{i}" . }` for quad index `i`.
fn insert_update_body(i: usize) -> String {
    format!("INSERT DATA {{ <http://ex/s{i}> <http://ex/p> \"{i}\" . }}")
}

fn send_insert(port: u16, i: usize) -> bool {
    let url = format!("http://127.0.0.1:{port}/sparql/update");
    ureq::post(&url)
        .header("Content-Type", "application/sparql-update")
        .send(insert_update_body(i))
        .is_ok()
}

/// Query the running server for the full set of `s{i}` indices currently
/// present (via `<http://ex/p>` object literals), by asking one `ASK` query
/// per candidate index in `0..TOTAL_INSERTS` — simple and robust against any
/// particular SPARQL Results serialization format.
fn recovered_indices(port: u16) -> Vec<usize> {
    let mut present = Vec::new();
    for i in 0..TOTAL_INSERTS {
        let query = format!("ASK {{ <http://ex/s{i}> <http://ex/p> \"{i}\" . }}");
        let url = format!("http://127.0.0.1:{port}/sparql?query={}", urlencode(&query));
        let mut resp = ureq::get(&url)
            .call()
            .unwrap_or_else(|e| panic!("ASK query for index {i} failed: {e}"));
        let body_str = resp
            .body_mut()
            .read_to_string()
            .expect("ASK response body was not UTF-8 text");
        let body: serde_json::Value =
            serde_json::from_str(&body_str).expect("ASK response was not JSON");
        if body.get("boolean").and_then(|b| b.as_bool()) == Some(true) {
            present.push(i);
        }
    }
    present
}

/// Minimal percent-encoding sufficient for the fixed ASK-query shapes used
/// above (spaces, braces, angle brackets, quotes).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[test]
fn kill_nine_mid_burst_then_restart_recovers_exactly_acknowledged_writes() {
    let dir = std::env::temp_dir().join(format!("nova_crash_recovery_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let port = pick_port();

    let mut child = spawn_server(&dir, port);

    // Track exactly which indices the client received a successful HTTP
    // response for — this is the ground truth the reloaded store must match,
    // modulo the single "in-doubt" boundary write explained below.
    let acknowledged: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    // The single index that was *in flight* (request sent, response not yet
    // observed by the client) at the moment of the kill, if any. Because the
    // writer thread sends requests strictly sequentially (never
    // concurrently), at most one write can be "in doubt": the server may
    // have fully applied it and fsynced the WAL *and* written the complete
    // HTTP response before the SIGKILL landed, with the client-side read of
    // that response racing the process teardown and losing — i.e. the write
    // genuinely happened and will be recovered, but the client has no proof
    // it did. This is a fundamental, unavoidable ambiguity of "crash exactly
    // during response delivery" (the same ambiguity every at-least-once RPC
    // system has), not a durability bug — so the test tolerates it for
    // exactly one index rather than asserting byte-for-byte equality.
    let in_doubt: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let next_index = Arc::new(AtomicUsize::new(0));

    let writer_ack = Arc::clone(&acknowledged);
    let writer_doubt = Arc::clone(&in_doubt);
    let writer_next = Arc::clone(&next_index);
    let writer = std::thread::spawn(move || {
        loop {
            let i = writer_next.fetch_add(1, Ordering::SeqCst);
            if i >= TOTAL_INSERTS {
                break;
            }
            *writer_doubt.lock().unwrap() = Some(i);
            // A request may fail simply because the server got killed
            // mid-request (connection reset) — that's expected once the main
            // thread kills the process; just stop trying.
            if send_insert(port, i) {
                writer_ack.lock().unwrap().push(i);
                *writer_doubt.lock().unwrap() = None;
            } else {
                break;
            }
        }
    });

    // Let the burst get partway through, then simulate a hard crash.
    std::thread::sleep(Duration::from_millis(60));
    child.kill().expect("failed to SIGKILL nova_serve");
    let _ = child.wait();

    // Let the writer thread notice the dead connection and stop.
    let _ = writer.join();

    let acknowledged: Vec<usize> = {
        let mut v = acknowledged.lock().unwrap().clone();
        v.sort_unstable();
        v
    };
    let in_doubt_index: Option<usize> = *in_doubt.lock().unwrap();

    assert!(
        !acknowledged.is_empty(),
        "test setup issue: no inserts were acknowledged before the kill \
         (burst finished or failed too fast — consider increasing the sleep)"
    );
    assert!(
        acknowledged.len() < TOTAL_INSERTS,
        "test setup issue: the entire burst completed before the kill landed \
         (nothing was actually interrupted mid-flight) — consider increasing \
         TOTAL_INSERTS or decreasing the sleep"
    );

    // Restart against the same data directory — this is the real recovery
    // path (`RingStore::open` replaying the WAL / loading the snapshot).
    let mut restarted = spawn_server(&dir, port);
    let mut recovered = recovered_indices(port);
    recovered.sort_unstable();

    let _ = restarted.kill();
    let _ = restarted.wait();
    let _ = std::fs::remove_dir_all(&dir);

    if recovered != acknowledged {
        // The only tolerated discrepancy: the recovered set equals
        // acknowledged ∪ {in_doubt_index} — i.e. exactly the one write that
        // was in flight when the kill landed also made it to durable
        // storage before the process died, even though the client didn't
        // get to observe the response.
        let mut expanded = acknowledged.clone();
        if let Some(idx) = in_doubt_index {
            expanded.push(idx);
            expanded.sort_unstable();
            expanded.dedup();
        }
        assert_eq!(
            recovered, expanded,
            "reloaded store's contents must exactly match every write the \
             client received a successful HTTP response for before the kill \
             -9 (plus, at most, the single in-flight write that was racing \
             the kill, index {in_doubt_index:?}) — no acknowledged write may \
             be lost, and no other phantom write may appear"
        );
    }
}
