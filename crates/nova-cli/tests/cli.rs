//! End-to-end integration tests for the `oxigraph` CLI binary.
//!
//! Exercises the 6 subcommands added on top of the original `load`/`backup`/
//! `serve` trio — `query`, `update`, `dump`, `convert`, `optimize`, and
//! `serve-read-only` — by actually spawning the compiled `oxigraph` binary
//! (via `CARGO_BIN_EXE_oxigraph`, the same mechanism `nova-server`'s
//! `tests/crash_recovery.rs` uses for `nova_serve`) against a real temporary
//! `RingStore` directory, rather than calling any internal function
//! directly. This is deliberate: it validates the actual argument-parsing
//! (`cli.rs`) and process wiring (`main.rs`) end to end, the same surface a
//! human operator or script actually touches.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn oxigraph_bin() -> &'static str {
    env!("CARGO_BIN_EXE_oxigraph")
}

/// A fresh temp directory, cleaned up on `Drop`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nova_cli_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(oxigraph_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `oxigraph {args:?}`: {e}"))
}

fn run_stdin(args: &[&str], stdin_data: &str) -> Output {
    let mut child = Command::new(oxigraph_bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn `oxigraph {args:?}`: {e}"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_data.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_success(out: &Output, context: &str) {
    assert!(
        out.status.success(),
        "{context} failed (status {:?}):\nstdout: {}\nstderr: {}",
        out.status,
        stdout_str(out),
        stderr_str(out)
    );
}

const TURTLE_FIXTURE: &str = r#"
@prefix ex: <http://ex/> .
ex:alice ex:knows ex:bob .
ex:bob ex:knows ex:carol .
ex:alice ex:name "Alice" .
"#;

/// `load` a small Turtle fixture into a fresh store, returning the temp dir
/// (kept alive so callers can chain further subcommands against it).
fn load_fixture() -> TempDir {
    let dir = TempDir::new("query");
    let ttl_path = dir.path().join("fixture.ttl");
    std::fs::write(&ttl_path, TURTLE_FIXTURE).unwrap();

    let out = run(&[
        "load",
        "--location",
        dir.path().to_str().unwrap(),
        "--file",
        ttl_path.to_str().unwrap(),
    ]);
    assert_success(&out, "load");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "expected load to report 3 triples total, got: {}",
        stdout_str(&out)
    );
    dir
}

#[test]
fn load_multiple_files_merges_into_single_bulk_load() {
    let dir = TempDir::new("multi_file_load");
    let a = dir.path().join("a.ttl");
    let b = dir.path().join("b.ttl");
    std::fs::write(
        &a,
        "@prefix ex: <http://ex/> .\nex:alice ex:knows ex:bob .\n",
    )
    .unwrap();
    std::fs::write(
        &b,
        "@prefix ex: <http://ex/> .\nex:bob ex:knows ex:carol .\nex:carol ex:knows ex:dave .\n",
    )
    .unwrap();

    let out = run(&[
        "load",
        "--location",
        dir.path().to_str().unwrap(),
        "--file",
        a.to_str().unwrap(),
        "--file",
        b.to_str().unwrap(),
    ]);
    assert_success(&out, "load (multi-file)");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "expected combined 3 triples across both files, got: {}",
        stdout_str(&out)
    );

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "SELECT ?o WHERE { <http://ex/carol> <http://ex/knows> ?o }",
    ]);
    assert_success(&out, "query (post multi-file load)");
    assert!(
        stdout_str(&out).contains("http://ex/dave"),
        "body: {}",
        stdout_str(&out)
    );
}

#[test]
fn load_multiple_files_space_separated_single_flag() {
    // Same as `load_multiple_files_merges_into_single_bulk_load`, but using
    // clap's `num_args = 0..` space-separated syntax
    // (`--file a.ttl b.ttl`) in a single flag occurrence, rather than
    // repeating `--file` for each path — verifies parity with upstream
    // `oxigraph-cli`'s `--file` flag, which supports both forms.
    let dir = TempDir::new("multi_file_load_space_sep");
    let a = dir.path().join("a.ttl");
    let b = dir.path().join("b.ttl");
    std::fs::write(
        &a,
        "@prefix ex: <http://ex/> .\nex:alice ex:knows ex:bob .\n",
    )
    .unwrap();
    std::fs::write(
        &b,
        "@prefix ex: <http://ex/> .\nex:bob ex:knows ex:carol .\nex:carol ex:knows ex:dave .\n",
    )
    .unwrap();

    let out = run(&[
        "load",
        "--location",
        dir.path().to_str().unwrap(),
        "--file",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
    ]);
    assert_success(&out, "load (multi-file, space-separated)");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "expected combined 3 triples across both files, got: {}",
        stdout_str(&out)
    );

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "SELECT ?o WHERE { <http://ex/carol> <http://ex/knows> ?o }",
    ]);
    assert_success(&out, "query (post space-separated multi-file load)");
    assert!(
        stdout_str(&out).contains("http://ex/dave"),
        "body: {}",
        stdout_str(&out)
    );
}

#[test]
fn load_from_stdin_requires_format() {
    let dir = TempDir::new("stdin_load_no_format");

    let out = run_stdin(
        &["load", "--location", dir.path().to_str().unwrap()],
        TURTLE_FIXTURE,
    );

    assert!(
        !out.status.success(),
        "expected load with no --file/--format to fail"
    );
    assert!(
        stderr_str(&out).contains("--format"),
        "expected error to mention --format, got: {}",
        stderr_str(&out)
    );
}

#[test]
fn load_from_stdin_with_format_succeeds() {
    let dir = TempDir::new("stdin_load");

    let out = run_stdin(
        &[
            "load",
            "--location",
            dir.path().to_str().unwrap(),
            "--format",
            "ttl",
        ],
        TURTLE_FIXTURE,
    );
    assert_success(&out, "load (stdin)");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "expected 3 triples from stdin fixture, got: {}",
        stdout_str(&out)
    );
}

#[test]
fn load_with_base_resolves_relative_iris() {
    let dir = TempDir::new("base_load");
    let ttl_path = dir.path().join("relative.ttl");
    // A relative IRI subject/object, resolved against --base.
    std::fs::write(
        &ttl_path,
        "<rel-subject> <http://ex/knows> <rel-object> .\n",
    )
    .unwrap();

    let out = run(&[
        "load",
        "--location",
        dir.path().to_str().unwrap(),
        "--file",
        ttl_path.to_str().unwrap(),
        "--base",
        "http://ex/base/",
    ]);
    assert_success(&out, "load --base");
    assert!(
        stdout_str(&out).contains("1 triples"),
        "{}",
        stdout_str(&out)
    );

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "ASK { <http://ex/base/rel-subject> <http://ex/knows> <http://ex/base/rel-object> }",
    ]);
    assert_success(&out, "query (post --base load)");
    assert!(
        stdout_str(&out).contains("true"),
        "expected relative IRIs resolved against --base, body: {}",
        stdout_str(&out)
    );
}

#[test]
fn load_parse_error_propagates_nonzero_exit() {
    let dir = TempDir::new("parse_error_load");
    let bad_path = dir.path().join("bad.ttl");
    std::fs::write(&bad_path, "this is not valid turtle @@@ !!!\n").unwrap();

    let out = run(&[
        "load",
        "--location",
        dir.path().to_str().unwrap(),
        "--file",
        bad_path.to_str().unwrap(),
    ]);
    assert!(
        !out.status.success(),
        "expected load of malformed Turtle to fail"
    );
    assert!(
        stderr_str(&out).to_lowercase().contains("parse"),
        "expected error message to mention parsing, got: {}",
        stderr_str(&out)
    );
}

#[test]
fn load_then_query_select_json() {
    let dir = load_fixture();

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "SELECT ?s ?o WHERE { ?s <http://ex/knows> ?o }",
    ]);
    assert_success(&out, "query (select, json)");
    let body = stdout_str(&out);
    assert!(body.contains("\"http://ex/alice\""), "body: {body}");
    assert!(body.contains("\"http://ex/bob\""), "body: {body}");
}

#[test]
fn load_then_query_select_csv() {
    let dir = load_fixture();

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "SELECT ?name WHERE { ?s <http://ex/name> ?name }",
        "--results-format",
        "csv",
    ]);
    assert_success(&out, "query (select, csv)");
    let body = stdout_str(&out);
    assert!(body.contains("name"), "csv header missing, body: {body}");
    assert!(body.contains("Alice"), "csv row missing, body: {body}");
}

#[test]
fn load_then_query_ask() {
    let dir = load_fixture();

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "ASK { <http://ex/alice> <http://ex/knows> <http://ex/bob> }",
    ]);
    assert_success(&out, "query (ask)");
    let body = stdout_str(&out);
    assert!(body.contains("true"), "ASK result body: {body}");
}

#[test]
fn load_then_query_construct_triples() {
    let dir = load_fixture();

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "CONSTRUCT { ?s <http://ex/knows> ?o } WHERE { ?s <http://ex/knows> ?o }",
        "--results-format",
        "nt",
    ]);
    assert_success(&out, "query (construct)");
    let body = stdout_str(&out);
    assert!(
        body.contains("<http://ex/alice>") && body.contains("<http://ex/bob>"),
        "construct body: {body}"
    );
}

#[test]
fn update_insert_then_query_confirms() {
    let dir = load_fixture();

    let out = run(&[
        "update",
        "--location",
        dir.path().to_str().unwrap(),
        "--update",
        "INSERT DATA { <http://ex/dave> <http://ex/knows> <http://ex/alice> }",
    ]);
    assert_success(&out, "update");

    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "ASK { <http://ex/dave> <http://ex/knows> <http://ex/alice> }",
    ]);
    assert_success(&out, "query (confirm update)");
    assert!(stdout_str(&out).contains("true"));
}

#[test]
fn dump_without_graph_requires_dataset_format_for_plain_triple_format() {
    let dir = load_fixture();

    // Plain triple format (ttl) with no --graph and no --format: should
    // fail with a clear error, per Command::Dump's documented behavior.
    let out = run(&[
        "dump",
        "--location",
        dir.path().to_str().unwrap(),
        "--format",
        "ttl",
    ]);
    assert!(
        !out.status.success(),
        "expected dump --format ttl (no --graph) to fail"
    );
    assert!(
        stderr_str(&out).contains("--graph"),
        "expected error message to mention --graph, got: {}",
        stderr_str(&out)
    );
}

#[test]
fn dump_with_graph_plain_format_succeeds() {
    let dir = load_fixture();

    let out = run(&[
        "dump",
        "--location",
        dir.path().to_str().unwrap(),
        "--format",
        "nt",
        "--graph",
        // The fixture was loaded into the default graph; dumping a named
        // graph that doesn't exist should just produce empty output, not
        // an error — separately verify with the default-graph-only path
        // below.
        "http://ex/does-not-exist",
    ]);
    assert_success(&out, "dump --graph (nonexistent named graph)");
    assert_eq!(stdout_str(&out).trim(), "");
}

#[test]
fn dump_dataset_format_no_graph_dumps_everything() {
    let dir = load_fixture();

    let out = run(&[
        "dump",
        "--location",
        dir.path().to_str().unwrap(),
        "--format",
        "nq",
    ]);
    assert_success(&out, "dump --format nq (whole store)");
    let body = stdout_str(&out);
    // 3 quads from the fixture, one per non-blank line.
    let lines: Vec<_> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 3, "dump body:\n{body}");
    assert!(body.contains("http://ex/alice"), "dump body: {body}");
}

#[test]
fn dump_to_file_roundtrips_through_load() {
    let dir = load_fixture();
    let dump_path = dir.path().join("dump.nq");

    // Dataset format (nq) with no --graph dumps every graph (here, just the
    // default graph the fixture was loaded into) to a file.
    let out = run(&[
        "dump",
        "--location",
        dir.path().to_str().unwrap(),
        "--format",
        "nq",
        "--file",
        dump_path.to_str().unwrap(),
    ]);
    assert_success(&out, "dump --file");
    let contents = std::fs::read_to_string(&dump_path).unwrap();
    assert!(contents.contains("http://ex/alice"), "dump.nq: {contents}");

    // Load the dump back into a fresh store and confirm it round-trips to
    // the same triple count.
    let fresh = TempDir::new("dump_roundtrip");
    let out = run(&[
        "load",
        "--location",
        fresh.path().to_str().unwrap(),
        "--file",
        dump_path.to_str().unwrap(),
    ]);
    assert_success(&out, "load (roundtrip)");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "expected roundtrip load to also report 3 triples, got: {}",
        stdout_str(&out)
    );
}

#[test]
fn convert_stdin_to_stdout_ttl_to_nt() {
    let out = run_stdin(
        &["convert", "--from-format", "ttl", "--to-format", "nt"],
        TURTLE_FIXTURE,
    );
    assert_success(&out, "convert (stdin->stdout)");
    let body = stdout_str(&out);
    assert!(body.contains("<http://ex/alice>"), "convert body: {body}");
    assert!(stderr_str(&out).contains("Converted 3 quads"));
}

#[test]
fn convert_file_to_file() {
    let dir = TempDir::new("convert");
    let from = dir.path().join("in.ttl");
    let to = dir.path().join("out.nq");
    std::fs::write(&from, TURTLE_FIXTURE).unwrap();

    let out = run(&[
        "convert",
        "--from-file",
        from.to_str().unwrap(),
        "--to-file",
        to.to_str().unwrap(),
    ]);
    assert_success(&out, "convert (file->file)");
    let contents = std::fs::read_to_string(&to).unwrap();
    assert!(contents.contains("http://ex/alice"), "out.nq: {contents}");
    // N-Quads lines end with the default graph having no 4th term.
    assert_eq!(contents.lines().filter(|l| !l.is_empty()).count(), 3);
}

#[test]
fn convert_to_graph_remaps_default_graph_quads() {
    let out = run_stdin(
        &[
            "convert",
            "--from-format",
            "ttl",
            "--to-format",
            "nq",
            "--to-graph",
            "http://ex/target-graph",
        ],
        TURTLE_FIXTURE,
    );
    assert_success(&out, "convert --to-graph");
    let body = stdout_str(&out);
    assert!(
        body.contains("<http://ex/target-graph>"),
        "expected every line remapped to the target graph, body: {body}"
    );
}

#[test]
fn optimize_compacts_store() {
    let dir = load_fixture();

    let out = run(&["optimize", "--location", dir.path().to_str().unwrap()]);
    assert_success(&out, "optimize");
    assert!(
        stdout_str(&out).contains("3 triples"),
        "{}",
        stdout_str(&out)
    );

    // Store should still be fully queryable after compaction.
    let out = run(&[
        "query",
        "--location",
        dir.path().to_str().unwrap(),
        "--query",
        "ASK { <http://ex/alice> <http://ex/name> \"Alice\" }",
    ]);
    assert_success(&out, "query (post-optimize)");
    assert!(stdout_str(&out).contains("true"));
}

/// Spawn `oxigraph serve-read-only`, confirm GET /sparql works (200) and
/// any write attempt (`POST /update`) is rejected with 403 Forbidden — the
/// same write-gate assertion `nova-server`'s own unit tests make against
/// `Server::read_only`, but exercised here through the actual CLI
/// subcommand/process wiring.
#[test]
fn serve_read_only_rejects_writes_over_http() {
    let dir = load_fixture();
    let port = 21000 + (std::process::id() % 9000) as u16;
    let bind = format!("127.0.0.1:{port}");

    let mut child = Command::new(oxigraph_bin())
        .args([
            "serve-read-only",
            "--location",
            dir.path().to_str().unwrap(),
            "--bind",
            &bind,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn `oxigraph serve-read-only`");

    // Wait for it to come up.
    let ask_url = format!("http://{bind}/sparql?query=ASK%20%7B%7D");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("oxigraph serve-read-only did not become ready within 10s");
        }
        match ureq::get(&ask_url).call() {
            Ok(_) => break,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }

    // GET /sparql must succeed (read allowed).
    let resp = ureq::get(&ask_url).call();
    assert!(resp.is_ok(), "GET /sparql should succeed: {resp:?}");

    // POST /update must be rejected with 403 (write forbidden).
    let update_url = format!("http://{bind}/update");
    let err = ureq::post(&update_url)
        .header("Content-Type", "application/sparql-update")
        .send("INSERT DATA { <http://ex/x> <http://ex/y> <http://ex/z> }");
    match err {
        Ok(resp) => panic!(
            "expected write to be rejected, got status {}",
            resp.status()
        ),
        Err(ureq::Error::StatusCode(code)) => {
            assert_eq!(
                code, 403,
                "expected 403 Forbidden for write on read-only server"
            );
        }
        Err(other) => panic!("unexpected transport error: {other}"),
    }

    let _ = child.kill();
    let _ = child.wait();
}
