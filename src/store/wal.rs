//! Segmented write-ahead log (PoC step 4 generalizes step 3's single file).
//!
//! The WAL is a directory of zero-padded segment files (`<seqno>.wal`). New records
//! are appended (and fsynced) to the highest-numbered *active* segment. When the active
//! segment fills, it is *sealed* and a fresh segment is opened; the compactor flushes
//! sealed segments to the cold tier and then deletes them — that is how the WAL is
//! bounded (truncated) over time.
//!
//! Each record is `[u32 len][u32 crc32][JSON payload]`. JSON keeps the PoC debuggable;
//! the on-disk format is provisional until the GA freeze.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::NormalizedSpan;

const FRAME_HEADER_LEN: usize = 8;
const SEG_EXT: &str = "wal";

/// The active (currently-appended) WAL segment plus its directory.
pub struct Wal {
    dir: PathBuf,
    active_seqno: u64,
    active: fs::File,
    active_count: usize,
}

impl Wal {
    /// Open the WAL directory and create a fresh active segment numbered `active_seqno`.
    pub fn open(dir: &Path, active_seqno: u64) -> io::Result<Wal> {
        fs::create_dir_all(dir)?;
        let active = open_segment_file(dir, active_seqno)?;
        Ok(Wal {
            dir: dir.to_path_buf(),
            active_seqno,
            active,
            active_count: 0,
        })
    }

    /// Seqno of the segment currently being appended to.
    pub fn active_seqno(&self) -> u64 {
        self.active_seqno
    }

    /// Number of spans written to the active segment since it was opened/rotated.
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// Append a batch to the active segment and fsync — the durability point. Returns
    /// the segment seqno the batch landed in. The production writer group-commits via
    /// [`Wal::append_nosync`] + one [`Wal::sync`]; this composed form remains for tests.
    #[cfg(test)]
    pub fn append(&mut self, spans: &[NormalizedSpan]) -> io::Result<u64> {
        let seqno = self.append_nosync(spans)?;
        if !spans.is_empty() {
            self.sync()?;
        }
        Ok(seqno)
    }

    /// Append without the fsync — the batch is NOT durable until [`Wal::sync`] returns.
    /// Lets the writer group-commit: several queued batches share one fsync, and every
    /// caller is still only ACKed after that fsync (same guarantee, amortized cost).
    pub fn append_nosync(&mut self, spans: &[NormalizedSpan]) -> io::Result<u64> {
        if !spans.is_empty() {
            let mut buf = Vec::new();
            for span in spans {
                encode_record(span, &mut buf)?;
            }
            self.active.write_all(&buf)?;
            self.active_count += spans.len();
        }
        Ok(self.active_seqno)
    }

    /// fsync the active segment — the durability point for everything appended so far.
    pub fn sync(&mut self) -> io::Result<()> {
        self.active.flush()?;
        self.active.sync_data()
    }

    /// Seal the active segment and open the next one. Returns the sealed seqno.
    pub fn seal_and_rotate(&mut self) -> io::Result<u64> {
        let sealed = self.active_seqno;
        let next = sealed + 1;
        let new_active = open_segment_file(&self.dir, next)?;
        self.active = new_active; // the sealed segment's file handle drops (closed)
        self.active_seqno = next;
        self.active_count = 0;
        Ok(sealed)
    }
}

/// Create (or reopen) a segment file and fsync the parent directory so the new
/// segment's directory entry is durable. `sync_data()` on the file persists its
/// contents but NOT the directory entry linking name -> inode, so a brand-new segment
/// could otherwise be lost after a power failure even though its writes were ACKed.
/// Both `open` and `seal_and_rotate` go through here, so both creation and rotation
/// make the directory entry durable.
fn open_segment_file(dir: &Path, seqno: u64) -> io::Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(segment_path(dir, seqno))?;
    fsync_dir(dir)?;
    Ok(file)
}

/// fsync a directory so prior create/rename/unlink operations in it are durable.
///
/// Windows: opening a directory as a `File` needs `FILE_FLAG_BACKUP_SEMANTICS`, which
/// `std` doesn't set, so `File::open(dir)` fails with `Access is denied` (os error 5).
/// NTFS journals metadata operations itself, so directory-entry durability doesn't
/// need an explicit fsync there — skip it, as RocksDB/LevelDB do.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Path of a segment file: `<dir>/<seqno:020>.wal`.
pub fn segment_path(dir: &Path, seqno: u64) -> PathBuf {
    dir.join(format!("{seqno:020}.{SEG_EXT}"))
}

/// All segment seqnos present on disk, ascending.
pub fn list_segment_seqnos(dir: &Path) -> io::Result<Vec<u64>> {
    let mut seqnos = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(seqnos),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(&format!(".{SEG_EXT}")) {
            if let Ok(n) = stem.parse::<u64>() {
                seqnos.push(n);
            }
        }
    }
    seqnos.sort_unstable();
    Ok(seqnos)
}

/// Read and decode a segment, truncating any torn/corrupt trailing record so the file
/// ends on a record boundary. A missing segment reads as empty.
pub fn read_segment(dir: &Path, seqno: u64) -> io::Result<Vec<NormalizedSpan>> {
    let path = segment_path(dir, seqno);
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let (spans, good_len) = decode_records(&data);
    if good_len < data.len() as u64 {
        tracing::warn!(
            seqno,
            truncated_bytes = data.len() as u64 - good_len,
            "WAL segment had a torn/corrupt tail — truncating to the last good record"
        );
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(good_len)?;
        file.sync_all()?;
    }
    Ok(spans)
}

/// Delete a segment file (idempotent — a missing file is fine).
pub fn delete_segment(dir: &Path, seqno: u64) -> io::Result<()> {
    match fs::remove_file(segment_path(dir, seqno)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Append one framed record (`[u32 len][u32 crc32][JSON payload]`) to `buf`.
fn encode_record(span: &NormalizedSpan, buf: &mut Vec<u8>) -> io::Result<()> {
    let payload =
        serde_json::to_vec(span).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "span exceeds WAL frame size"))?;
    let crc = crc32fast::hash(&payload);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(())
}

/// Decode framed records, stopping at the first incomplete or corrupt one. Returns the
/// decoded spans and the byte offset of the last good record boundary.
fn decode_records(data: &[u8]) -> (Vec<NormalizedSpan>, u64) {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while cursor + FRAME_HEADER_LEN <= data.len() {
        let len = u32::from_le_bytes(data[cursor..cursor + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(data[cursor + 4..cursor + 8].try_into().unwrap());
        let start = cursor + FRAME_HEADER_LEN;
        let end = match start.checked_add(len) {
            Some(end) if end <= data.len() => end,
            _ => break,
        };
        let payload = &data[start..end];
        if crc32fast::hash(payload) != crc {
            break;
        }
        match serde_json::from_slice::<NormalizedSpan>(payload) {
            Ok(span) => spans.push(span),
            Err(_) => break,
        }
        cursor = end;
    }
    (spans, cursor as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;

    #[test]
    fn append_seal_recover() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(dir.path(), 1).unwrap();
        assert_eq!(wal.append(&[test_span("aa", "01", 1)]).unwrap(), 1);
        assert_eq!(wal.append(&[test_span("bb", "02", 2)]).unwrap(), 1);
        let sealed = wal.seal_and_rotate().unwrap();
        assert_eq!(sealed, 1);
        assert_eq!(wal.active_seqno(), 2);
        wal.append(&[test_span("cc", "03", 3)]).unwrap();

        assert_eq!(list_segment_seqnos(dir.path()).unwrap(), vec![1, 2]);
        assert_eq!(read_segment(dir.path(), 1).unwrap().len(), 2);
        assert_eq!(read_segment(dir.path(), 2).unwrap().len(), 1);
    }

    #[test]
    fn recover_truncates_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut wal = Wal::open(dir.path(), 7).unwrap();
            wal.append(&[test_span("aa", "01", 1), test_span("bb", "02", 2)])
                .unwrap();
        }
        let path = segment_path(dir.path(), 7);
        let good_len = fs::metadata(&path).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&1000u32.to_le_bytes()).unwrap(); // claims 1000-byte payload
            f.write_all(&0u32.to_le_bytes()).unwrap();
            f.write_all(b"short").unwrap();
            f.sync_all().unwrap();
        }
        assert_eq!(read_segment(dir.path(), 7).unwrap().len(), 2);
        assert_eq!(fs::metadata(&path).unwrap().len(), good_len);
    }

    #[test]
    fn recover_drops_crc_corrupt_record() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut wal = Wal::open(dir.path(), 1).unwrap();
            wal.append(&[test_span("aa", "01", 1)]).unwrap();
        }
        let path = segment_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        assert!(read_segment(dir.path(), 1).unwrap().is_empty());
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        Wal::open(dir.path(), 1).unwrap();
        delete_segment(dir.path(), 1).unwrap();
        delete_segment(dir.path(), 1).unwrap(); // already gone — fine
        delete_segment(dir.path(), 999).unwrap();
    }
}
