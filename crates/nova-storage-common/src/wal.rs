//! Write-Ahead Log (WAL) — generic crash-safe append/replay engine, plus RDF
//! quad/term encoders reusable by any `QuadStore` backend.
//!
//! ## Design
//!
//! Every mutating `QuadStore` call (`insert`, `remove`, `register_named_graph`)
//! is logged to an append-only file **before** it is applied to in-memory
//! state — standard write-ahead logging discipline: log the intent durably,
//! then apply it, so a crash between the two can always be recovered by
//! replaying the log.
//!
//! Records are **self-contained**: each one carries the fully-encoded
//! `Quad` (or `GraphName`) using plain strings for terms, not backend-internal
//! integer ids. This sidesteps any ordering dependency on a backend's term
//! dictionary — replay simply calls the exact same apply-insert/apply-remove
//! logic used at write time, letting the dictionary reassign identical ids
//! because interning is a deterministic function of call order.
//!
//! ## On-disk framing
//!
//! ```text
//! [u32 LE payload_len][payload bytes][u32 LE crc32(payload)]
//! ```
//!
//! `payload` is `[op_tag: u8][record-specific fields]` (see [`WalRecord`]).
//! This framing/replay engine has no knowledge of any particular index
//! structure (LOUDS/CLTJ/Ring, RocksDB, etc.) — it operates purely on bytes
//! and a generic `apply: FnMut(WalRecord)` callback, making it reusable by
//! any storage backend that wants crash-safe durability for RDF quad writes.
//!
//! ## Crash tolerance
//!
//! [`replay`] reads records sequentially and stops — without returning an
//! error — at the first sign of a torn write (an incomplete length prefix,
//! an incomplete payload/CRC, or a CRC mismatch). This is exactly what a
//! `kill -9` mid-`write`/`fsync` looks like: the tail of the file is garbage
//! or missing, but everything before it is intact. After detecting a torn
//! tail, `replay` truncates the file to the last valid record boundary so
//! future appends start cleanly (no gap, no leftover garbage after the
//! truncation point).
//!
//! ## Streaming replay
//!
//! [`replay`] reads through a `BufReader` and reuses a single payload
//! buffer across records rather than loading the whole segment into memory
//! up front — a WAL tail with many records (or a few very large ones) never
//! requires more transient memory than one buffered chunk plus the largest
//! single record, which matters on startup when replaying a long
//! not-yet-compacted tail. Callers still consume records one at a time via
//! the `apply` callback, so the lazy-iterator property holds all the way
//! from disk to the caller.
//!
//! ## Fsync policy
//!
//! [`WalWriter::append`] calls `File::sync_data()` after every write — the
//! simplest correct policy (every acknowledged write is durable) at the cost
//! of one fsync per operation. Two group-commit alternatives are now also
//! available for callers that want to trade a small window of durability for
//! much higher write throughput:
//!
//! - [`WalWriter::append_batch`] writes an entire batch of records with a
//!   **single** `fsync` at the end — used by `LoudsStore::extend` (and thus
//!   any multi-quad bulk insert / SPARQL `INSERT DATA` with many triples) so
//!   that every record in the batch is either fully durable or the whole
//!   batch is torn identically to a single-record torn write (see [`replay`]
//!   — the per-record framing means a batch's records are indistinguishable
//!   from independently-appended ones at replay time).
//! - [`WalWriter::append_no_sync`] writes without fsyncing at all, paired
//!   with a caller-managed background thread that calls
//!   [`WalWriter::sync`]/[`WalWriter::try_clone_file`] periodically (see
//!   `oxigraph_nova_storage_ring::store::SyncPolicy::Interval`). This is the
//!   "group commit" pattern used by most production databases (e.g.
//!   RocksDB's default WAL mode, MongoDB's periodic journal commit): writes
//!   return immediately, and a background flusher batches many writers'
//!   fsyncs into one periodic syscall. The cost is a small durability
//!   window — writes acknowledged since the last flush are lost on a crash
//!   (not corrupted; `replay`'s torn-tail handling covers this exactly the
//!   same way as any other incomplete/torn write).

use oxigraph_nova_core::{BlankNode, GraphName, Literal, NamedNode, Oxigraph, Quad, Subject, Term};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::Path;

// ── WalRecord ────────────────────────────────────────────────────────────────

/// One durable, self-contained WAL record.
#[derive(Debug, Clone, PartialEq)]
pub enum WalRecord {
    InsertQuad(Quad),
    RemoveQuad(Quad),
    RegisterGraph(GraphName),
}

impl WalRecord {
    fn tag(&self) -> u8 {
        match self {
            WalRecord::InsertQuad(_) => 1,
            WalRecord::RemoveQuad(_) => 2,
            WalRecord::RegisterGraph(_) => 3,
        }
    }
}

// ── Primitive encoding ─────────────────────────────────────────────────────────

pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

pub fn read_string(buf: &[u8], pos: &mut usize) -> Result<String, Oxigraph> {
    if *pos + 4 > buf.len() {
        return Err(Oxigraph::Storage("WAL: truncated string length".into()));
    }
    let len = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    if *pos + len > buf.len() {
        return Err(Oxigraph::Storage("WAL: truncated string content".into()));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|e| Oxigraph::Storage(format!("WAL: invalid utf8: {e}")))?
        .to_string();
    *pos += len;
    Ok(s)
}

pub fn read_tag(buf: &[u8], pos: &mut usize) -> Result<u8, Oxigraph> {
    if *pos >= buf.len() {
        return Err(Oxigraph::Storage("WAL: truncated tag byte".into()));
    }
    let tag = buf[*pos];
    *pos += 1;
    Ok(tag)
}

// ── Term / Subject / GraphName encoding ───────────────────────────────────────
//
// Term tags: 0=NamedNode 1=BlankNode 2=Literal(simple) 3=Literal(lang-tagged)
//            4=Literal(directional lang-tagged) 5=Literal(typed)
// Subject/GraphName reuse tags 0/1 (NamedNode/BlankNode); GraphName adds 2=DefaultGraph.

fn write_term(buf: &mut Vec<u8>, term: &Term) {
    match term {
        Term::NamedNode(n) => {
            buf.push(0);
            write_string(buf, n.as_str());
        }
        Term::BlankNode(b) => {
            buf.push(1);
            write_string(buf, b.as_str());
        }
        Term::Literal(l) => {
            if let Some(direction) = l.direction() {
                buf.push(4);
                write_string(buf, l.value());
                write_string(buf, l.language().unwrap_or(""));
                buf.push(match direction {
                    oxrdf::BaseDirection::Ltr => 0,
                    oxrdf::BaseDirection::Rtl => 1,
                });
            } else if let Some(lang) = l.language() {
                buf.push(3);
                write_string(buf, l.value());
                write_string(buf, lang);
            } else if l.datatype() == oxrdf::vocab::xsd::STRING {
                buf.push(2);
                write_string(buf, l.value());
            } else {
                buf.push(5);
                write_string(buf, l.value());
                write_string(buf, l.datatype().as_str());
            }
        }
        Term::Triple(_) => {
            // `oxrdf::Quad::subject`/`object` never carry a quoted-triple
            // *subject* through `QuadStore::insert`/`remove` (RDF-star
            // quoted-triple objects, however, DO reach here — handle below).
            unreachable!(
                "write_term called on a quoted-triple subject; \
                 quoted-triple objects are handled by the Triple arm below"
            );
        }
    }
}

/// Object terms may legally be `Term::Triple` (RDF-star quoted triples as
/// objects). Encode those recursively via tag 6.
pub fn write_object_term(buf: &mut Vec<u8>, term: &Term) {
    if let Term::Triple(t) = term {
        buf.push(6);
        write_subject(buf, &t.subject);
        write_string(buf, t.predicate.as_str());
        write_object_term(buf, &t.object);
    } else {
        write_term(buf, term);
    }
}

pub fn read_object_term(buf: &[u8], pos: &mut usize) -> Result<Term, Oxigraph> {
    let tag = read_tag(buf, pos)?;
    match tag {
        6 => {
            let subject = read_subject_from_tag(buf, pos)?;
            let predicate = NamedNode::new_unchecked(read_string(buf, pos)?);
            let object = read_object_term(buf, pos)?;
            Ok(Term::Triple(Box::new(oxrdf::Triple {
                subject,
                predicate,
                object,
            })))
        }
        other => read_term_from_tag(other, buf, pos),
    }
}

fn read_term_from_tag(tag: u8, buf: &[u8], pos: &mut usize) -> Result<Term, Oxigraph> {
    match tag {
        0 => Ok(Term::NamedNode(NamedNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        1 => Ok(Term::BlankNode(BlankNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        2 => Ok(Term::Literal(Literal::new_simple_literal(read_string(
            buf, pos,
        )?))),
        3 => {
            let value = read_string(buf, pos)?;
            let lang = read_string(buf, pos)?;
            Ok(Term::Literal(
                Literal::new_language_tagged_literal_unchecked(value, lang),
            ))
        }
        4 => {
            let value = read_string(buf, pos)?;
            let lang = read_string(buf, pos)?;
            let dir_byte = read_tag(buf, pos)?;
            let direction = match dir_byte {
                0 => oxrdf::BaseDirection::Ltr,
                1 => oxrdf::BaseDirection::Rtl,
                _ => return Err(Oxigraph::Storage("WAL: invalid direction byte".into())),
            };
            Ok(Term::Literal(
                Literal::new_directional_language_tagged_literal_unchecked(value, lang, direction),
            ))
        }
        5 => {
            let value = read_string(buf, pos)?;
            let datatype = read_string(buf, pos)?;
            Ok(Term::Literal(Literal::new_typed_literal(
                value,
                NamedNode::new_unchecked(datatype),
            )))
        }
        other => Err(Oxigraph::Storage(format!("WAL: unknown term tag {other}"))),
    }
}

fn write_subject(buf: &mut Vec<u8>, s: &Subject) {
    match s {
        Subject::NamedNode(n) => {
            buf.push(0);
            write_string(buf, n.as_str());
        }
        Subject::BlankNode(b) => {
            buf.push(1);
            write_string(buf, b.as_str());
        }
    }
}

fn read_subject_from_tag(buf: &[u8], pos: &mut usize) -> Result<Subject, Oxigraph> {
    let tag = read_tag(buf, pos)?;
    match tag {
        0 => Ok(Subject::NamedNode(NamedNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        1 => Ok(Subject::BlankNode(BlankNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        other => Err(Oxigraph::Storage(format!(
            "WAL: unknown subject tag {other}"
        ))),
    }
}

pub fn write_graph_name(buf: &mut Vec<u8>, g: &GraphName) {
    match g {
        GraphName::DefaultGraph => buf.push(2),
        GraphName::NamedNode(n) => {
            buf.push(0);
            write_string(buf, n.as_str());
        }
        GraphName::BlankNode(b) => {
            buf.push(1);
            write_string(buf, b.as_str());
        }
    }
}

pub fn read_graph_name(buf: &[u8], pos: &mut usize) -> Result<GraphName, Oxigraph> {
    let tag = read_tag(buf, pos)?;
    match tag {
        0 => Ok(GraphName::NamedNode(NamedNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        1 => Ok(GraphName::BlankNode(BlankNode::new_unchecked(read_string(
            buf, pos,
        )?))),
        2 => Ok(GraphName::DefaultGraph),
        other => Err(Oxigraph::Storage(format!(
            "WAL: unknown graph-name tag {other}"
        ))),
    }
}

// ── Quad encoding (graph, subject, predicate, object) ─────────────────────────

fn write_quad(buf: &mut Vec<u8>, quad: &Quad) {
    write_graph_name(buf, &quad.graph_name);
    write_subject(buf, &quad.subject);
    write_string(buf, quad.predicate.as_str());
    write_object_term(buf, &quad.object);
}

fn read_quad(buf: &[u8], pos: &mut usize) -> Result<Quad, Oxigraph> {
    let graph_name = read_graph_name(buf, pos)?;
    let subject = read_subject_from_tag(buf, pos)?;
    let predicate = NamedNode::new_unchecked(read_string(buf, pos)?);
    let object = read_object_term(buf, pos)?;
    Ok(Quad::new(subject, predicate, object, graph_name))
}

// ── Record encoding ────────────────────────────────────────────────────────────

fn encode_record(record: &WalRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(record.tag());
    match record {
        WalRecord::InsertQuad(q) | WalRecord::RemoveQuad(q) => write_quad(&mut buf, q),
        WalRecord::RegisterGraph(g) => write_graph_name(&mut buf, g),
    }
    buf
}

fn decode_record(buf: &[u8]) -> Result<WalRecord, Oxigraph> {
    let mut pos = 0usize;
    let tag = read_tag(buf, &mut pos)?;
    match tag {
        1 => Ok(WalRecord::InsertQuad(read_quad(buf, &mut pos)?)),
        2 => Ok(WalRecord::RemoveQuad(read_quad(buf, &mut pos)?)),
        3 => Ok(WalRecord::RegisterGraph(read_graph_name(buf, &mut pos)?)),
        other => Err(Oxigraph::Storage(format!(
            "WAL: unknown record tag {other}"
        ))),
    }
}

// ── WalWriter ──────────────────────────────────────────────────────────────────

/// Append-only WAL file handle.
pub struct WalWriter {
    file: File,
}

impl WalWriter {
    /// Open (creating if necessary) the WAL file at `path` for appending.
    pub fn create_or_open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// Frame one record: `[u32 LE payload_len][payload][u32 LE crc32(payload)]`.
    fn frame_record(record: &WalRecord) -> Vec<u8> {
        let payload = encode_record(record);
        let crc = crc32fast::hash(&payload);
        let mut framed = Vec::with_capacity(4 + payload.len() + 4);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed.extend_from_slice(&crc.to_le_bytes());
        framed
    }

    /// Append one record, framed with a length prefix and CRC32 checksum,
    /// and `fsync` before returning (see module docs on fsync policy).
    pub fn append(&mut self, record: &WalRecord) -> io::Result<()> {
        let framed = Self::frame_record(record);
        self.file.write_all(&framed)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Append every record in `records`, each independently framed exactly
    /// as [`append`] would, but with a **single** `fsync` after the whole
    /// batch is written — group commit. Either the entire batch is durable
    /// after this returns `Ok`, or (on a crash before the fsync completes)
    /// none of it is guaranteed durable; because each record keeps its own
    /// length+CRC frame, a partially-flushed batch is torn identically to
    /// any other torn write and is handled the same way by [`replay`] (stop
    /// at the first bad frame, truncate to the last good boundary — no
    /// record is ever left half-applied).
    ///
    /// Empty `records` is a no-op (no write, no fsync).
    pub fn append_batch<'a>(
        &mut self,
        records: impl IntoIterator<Item = &'a WalRecord>,
    ) -> io::Result<()> {
        let mut buf = Vec::new();
        for record in records {
            buf.extend_from_slice(&Self::frame_record(record));
        }
        if buf.is_empty() {
            return Ok(());
        }
        self.file.write_all(&buf)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Append one record like [`append`], but **without** fsyncing —
    /// intended for use with `SyncPolicy::Interval`'s background flusher
    /// (see module docs). The caller is responsible for ensuring
    /// [`WalWriter::sync`] is called periodically (or before shutdown) so
    /// writes eventually become durable; until then, a crash can lose
    /// anything written via this method since the last `sync`.
    pub fn append_no_sync(&mut self, record: &WalRecord) -> io::Result<()> {
        let framed = Self::frame_record(record);
        self.file.write_all(&framed)?;
        Ok(())
    }

    /// Explicitly `fsync` (`sync_data`) the underlying file — used by a
    /// background flusher thread under `SyncPolicy::Interval` to durably
    /// commit everything written via [`append_no_sync`] since the last call.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Clone the underlying `File` handle (shares the same OS file
    /// description/offset), so a background flusher thread can call
    /// [`WalWriter::sync`]-equivalent (`File::sync_data`) concurrently with
    /// the writer thread continuing to `write_all`/`append_no_sync` — Unix
    /// `write`s to the same fd are safe to interleave with `fsync` from
    /// another thread (the kernel serializes them), and this avoids needing
    /// a lock shared between the writer and flusher for the common case
    /// where the flusher only ever calls `sync_data`.
    pub fn try_clone_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }
}

// ── Replay ─────────────────────────────────────────────────────────────────────

/// Fills `buf` from `reader` as far as possible, tolerating
/// [`ErrorKind::Interrupted`] retries. Returns the number of bytes actually
/// read before hitting EOF — `buf.len()` on a full read, or fewer on a torn
/// tail. This (rather than [`Read::read_exact`]) is what lets [`replay`]
/// distinguish a clean end-of-file at a record boundary (0 bytes read) from
/// a torn partial read (1..`buf.len()` bytes read) without treating both as
/// the same "unexpected EOF" error.
fn fill_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Replay all valid records in the WAL file at `path`, calling `apply` for
/// each one in order.
///
/// If `path` doesn't exist yet, this is a no-op (fresh store). Stops
/// gracefully (without returning an error) at the first torn/corrupted
/// record, truncating the file to the last valid record boundary so future
/// appends start cleanly. Returns the number of records successfully
/// replayed.
///
/// Reads through a `BufReader` and reuses one payload buffer across records
/// (see the "Streaming replay" module doc) rather than materializing the
/// whole segment in memory before starting.
pub fn replay(path: &Path, mut apply: impl FnMut(WalRecord)) -> io::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    let mut count = 0usize;
    let mut valid_len = 0u64;
    let mut payload = Vec::new();

    loop {
        // Length prefix.
        let mut len_bytes = [0u8; 4];
        let n = fill_or_eof(&mut reader, &mut len_bytes)?;
        if n == 0 {
            break; // clean EOF exactly at a record boundary — nothing torn
        }
        if n < 4 {
            break; // torn tail: not enough bytes for the length prefix
        }
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Sanity-check `len` against bytes actually remaining in the file
        // *before* allocating anything: a torn/corrupt length prefix (e.g.
        // a crash mid-write of the u32 itself, or a flipped bit) must not
        // be trusted enough to drive a multi-gigabyte `resize`. A valid
        // record's payload plus its trailing 4-byte CRC can never exceed
        // what's left on disk after this record's own length prefix.
        let remaining = file_len - valid_len - 4;
        if (len as u64) + 4 > remaining {
            break; // torn/corrupt tail: implausible length — stop, don't allocate
        }

        // Payload, reusing `payload`'s backing allocation across records.
        payload.resize(len, 0);
        let n = fill_or_eof(&mut reader, &mut payload)?;
        if n < len {
            break; // torn tail: incomplete payload
        }

        // Trailing CRC32.

        let mut crc_bytes = [0u8; 4];
        let n = fill_or_eof(&mut reader, &mut crc_bytes)?;
        if n < 4 {
            break; // torn tail: incomplete CRC
        }
        let stored_crc = u32::from_le_bytes(crc_bytes);
        let actual_crc = crc32fast::hash(&payload);
        if actual_crc != stored_crc {
            break; // torn write: CRC mismatch — stop at this crash-safe boundary
        }

        match decode_record(&payload) {
            Ok(record) => {
                apply(record);
                count += 1;
            }
            Err(_) => break, // CRC matched but decode failed — extremely unlikely; stop defensively
        }
        valid_len += 4 + len as u64 + 4;
    }

    if valid_len < file_len {
        // Torn tail (or trailing garbage) detected — truncate so future
        // appends start cleanly, with no gap and no leftover garbage.
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(valid_len)?;
    }

    Ok(count)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{GraphName, Literal as CoreLiteral, NamedNode as CoreNamedNode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("nova_wal_test_{pid}_{n}_{name}"))
    }

    fn nn(s: &str) -> Subject {
        Subject::NamedNode(CoreNamedNode::new_unchecked(s))
    }
    fn pred(s: &str) -> NamedNode {
        CoreNamedNode::new_unchecked(s)
    }
    fn lit(s: &str) -> Term {
        Term::Literal(CoreLiteral::new_simple_literal(s))
    }

    #[test]
    fn term_roundtrip_all_kinds() {
        let cases = vec![
            Term::NamedNode(CoreNamedNode::new_unchecked("http://ex/a")),
            Term::BlankNode(BlankNode::new_unchecked("b1")),
            Term::Literal(CoreLiteral::new_simple_literal("hello")),
            Term::Literal(CoreLiteral::new_language_tagged_literal("bonjour", "fr").unwrap()),
            Term::Literal(CoreLiteral::new_typed_literal(
                "42",
                CoreNamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer"),
            )),
        ];
        for term in cases {
            let mut buf = Vec::new();
            write_object_term(&mut buf, &term);
            let mut pos = 0usize;
            let decoded = read_object_term(&buf, &mut pos).unwrap();
            assert_eq!(decoded, term);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn quoted_triple_object_roundtrip() {
        let inner = oxrdf::Triple {
            subject: nn("http://ex/s"),
            predicate: pred("http://ex/p"),
            object: lit("v"),
        };
        let term = Term::Triple(Box::new(inner));
        let mut buf = Vec::new();
        write_object_term(&mut buf, &term);
        let mut pos = 0usize;
        let decoded = read_object_term(&buf, &mut pos).unwrap();
        assert_eq!(decoded, term);
    }

    #[test]
    fn write_and_replay_basic() {
        let path = temp_path("basic.log");
        let _ = std::fs::remove_file(&path);

        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::InsertQuad(q1.clone())).unwrap();
            w.append(&WalRecord::InsertQuad(q2.clone())).unwrap();
            w.append(&WalRecord::RemoveQuad(q1.clone())).unwrap();
        }

        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(count, 3);
        assert_eq!(replayed[0], WalRecord::InsertQuad(q1.clone()));
        assert_eq!(replayed[1], WalRecord::InsertQuad(q2));
        assert_eq!(replayed[2], WalRecord::RemoveQuad(q1));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_does_not_truncate_a_cleanly_written_log() {
        // A streaming replay implementation is uniquely prone to conflating
        // "clean EOF exactly at a record boundary" with "torn tail" (both
        // look like a short read at first glance) — assert the file's
        // length is completely unchanged after replaying a log with no
        // torn records at all.
        let path = temp_path("clean_no_truncate.log");
        let _ = std::fs::remove_file(&path);

        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::InsertQuad(q1.clone())).unwrap();
            w.append(&WalRecord::InsertQuad(q2.clone())).unwrap();
        }

        let len_before = std::fs::metadata(&path).unwrap().len();
        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            replayed,
            vec![WalRecord::InsertQuad(q1), WalRecord::InsertQuad(q2)]
        );

        let len_after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            len_before, len_after,
            "replaying a cleanly-written log must not truncate it"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_handles_many_records_spanning_bufreader_refills() {
        // Write enough records to force the streaming reader through
        // several internal buffer refills (BufReader's default capacity is
        // 8 KiB), proving records that straddle a refill boundary still
        // decode correctly.
        let path = temp_path("many_records.log");
        let _ = std::fs::remove_file(&path);

        let quads: Vec<Quad> = (0..2000)
            .map(|i| {
                Quad::new(
                    nn(&format!("http://ex/s{i}")),
                    pred("http://ex/p"),
                    lit(&format!(
                        "value-{i}-with-some-extra-padding-to-vary-record-size"
                    )),
                    GraphName::DefaultGraph,
                )
            })
            .collect();

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            for q in &quads {
                w.append(&WalRecord::InsertQuad(q.clone())).unwrap();
            }
        }

        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(count, quads.len());
        for (i, q) in quads.iter().enumerate() {
            assert_eq!(replayed[i], WalRecord::InsertQuad(q.clone()));
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_missing_file_is_noop() {
        let path = temp_path("missing.log");
        let _ = std::fs::remove_file(&path);
        let count = replay(&path, |_| panic!("should not be called")).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn torn_tail_is_tolerated_and_truncated() {
        let path = temp_path("torn.log");
        let _ = std::fs::remove_file(&path);

        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::InsertQuad(q1.clone())).unwrap();
            w.append(&WalRecord::InsertQuad(q2)).unwrap();
        }

        let full_len = std::fs::metadata(&path).unwrap().len();
        // Simulate a crash mid-write of the second record: truncate the last
        // 5 bytes off the file (torn tail, incomplete CRC or payload).
        {
            let file = OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(full_len - 5).unwrap();
        }

        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(count, 1, "only the first (intact) record should replay");
        assert_eq!(replayed[0], WalRecord::InsertQuad(q1));

        // File should now be truncated to exactly the valid boundary —
        // replaying it again should yield the same single record, not an error.
        let mut replayed2 = Vec::new();
        let count2 = replay(&path, |r| replayed2.push(r)).unwrap();
        assert_eq!(count2, 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn implausible_length_prefix_stops_replay_without_allocating() {
        // A corrupt/torn length prefix must never be trusted enough to
        // drive a huge `Vec::resize` before it's validated against what's
        // actually left in the file. Simulate this by flipping the first
        // record's length prefix to an enormous value (larger than the
        // entire file), and assert replay stops cleanly (as if it were any
        // other torn tail) instead of erroring or hanging on a giant alloc.
        let path = temp_path("implausible_len.log");
        let _ = std::fs::remove_file(&path);

        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::InsertQuad(q1)).unwrap();
            w.append(&WalRecord::InsertQuad(q2)).unwrap();
        }

        // Corrupt the very first record's 4-byte length prefix (file offset
        // 0..4) to an implausibly large value.
        let mut data = std::fs::read(&path).unwrap();
        data[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(
            count, 0,
            "an implausible length prefix must stop replay at record 0, not error/hang"
        );

        // The file should be truncated to zero (nothing before the corrupt
        // record was valid), so future appends start cleanly.
        let len_after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len_after, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupted_middle_crc_stops_replay() {
        let path = temp_path("corrupt.log");
        let _ = std::fs::remove_file(&path);

        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::InsertQuad(q1.clone())).unwrap();
            w.append(&WalRecord::InsertQuad(q2)).unwrap();
        }

        // Flip a byte inside the second record's payload to break its CRC.
        let mut data = std::fs::read(&path).unwrap();
        let flip_at = data.len() - 6; // inside the trailing CRC/payload area
        data[flip_at] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        let mut replayed = Vec::new();
        let count = replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(count, 1);
        assert_eq!(replayed[0], WalRecord::InsertQuad(q1));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn register_graph_record_roundtrip() {
        let path = temp_path("register.log");
        let _ = std::fs::remove_file(&path);
        let g = GraphName::NamedNode(CoreNamedNode::new_unchecked("http://ex/g"));

        {
            let mut w = WalWriter::create_or_open(&path).unwrap();
            w.append(&WalRecord::RegisterGraph(g.clone())).unwrap();
        }

        let mut replayed = Vec::new();
        replay(&path, |r| replayed.push(r)).unwrap();
        assert_eq!(replayed, vec![WalRecord::RegisterGraph(g)]);

        let _ = std::fs::remove_file(&path);
    }
}
