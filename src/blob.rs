//! Externalized blob storage for oversized span payloads.
//!
//! Large `input.value` / `output.value` payloads — RAG contexts, big tool outputs, base64
//! blobs — would bloat every WAL segment, every Parquet block, and every query response, and
//! freeze the browser on read. Instead, any field
//! over a size cap is offloaded to a content-addressed file store and only a compact
//! `evald-blob:<key>` reference is left on the span, so both tiers stay bounded and the bytes
//! are fetched lazily via `GET /v1/blobs/{key}`.
//!
//! The offload runs at ingest, **before** the WAL append — the durability/commit protocol is
//! untouched, it just receives spans that already carry references instead of megabyte
//! strings. A blob write that fails leaves the payload inline (never drops the span).

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as Json;

use crate::model::NormalizedSpan;

/// Prefix marking a span field whose value was offloaded; the suffix is the blob key.
pub const BLOB_REF_PREFIX: &str = "evald-blob:";

/// How many leading chars of an offloaded value to keep as an inline preview (so a reader can
/// show something without a blob fetch). Small on purpose — the point is to keep spans light.
const PREVIEW_CHARS: usize = 256;

/// Monotonic disambiguator for temp-file names, so concurrent puts never share a temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The on-disk blob store: a flat `blobs/` directory of content-addressed files. Writes are
/// atomic (temp → rename) and idempotent — a key already present with matching bytes is left
/// as-is, giving natural dedup (the same RAG context reused across spans is stored once).
#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) the blob store rooted at `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Content key for `bytes`: byte length + a 64-bit content hash, both hex. Length-prefixing
    /// shrinks the collision surface to same-length payloads; [`Self::put`] additionally guards
    /// against the (astronomically unlikely) hash collision, so a read never returns the wrong
    /// payload. The hash is `std`'s fixed-key SipHash — deterministic across processes (no dep),
    /// non-cryptographic (this is a local content store, not a security boundary).
    fn key_for(bytes: &[u8]) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        format!("{:016x}{:016x}", bytes.len() as u64, h.finish())
    }

    /// Offload `bytes`, returning the reference to store on the span (`evald-blob:<key>`).
    /// Idempotent + content-addressed. On a key collision with *different* bytes the input is
    /// stored under a probed key (`<key>-1`, `-2`, …) recorded in the returned reference, so a
    /// later [`Self::get`] on that reference always yields the exact bytes written.
    pub fn put(&self, bytes: &[u8]) -> io::Result<String> {
        let base = Self::key_for(bytes);
        let mut key = base.clone();
        let mut n = 0u32;
        loop {
            let path = self.dir.join(&key);
            match fs::read(&path) {
                Ok(existing) if existing == bytes => break, // already stored (dedup hit)
                Ok(_) => {
                    n += 1;
                    key = format!("{base}-{n}"); // collision: probe a fresh key
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.write_atomic(&path, bytes)?;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(format!("{BLOB_REF_PREFIX}{key}"))
    }

    /// Fetch a blob by the key in an `evald-blob:<key>` reference (or a bare key). Returns
    /// `None` for an unknown key. A key that isn't a plain content key is rejected (`None`)
    /// so a request can never traverse out of the blob directory.
    pub fn get(&self, key_or_ref: &str) -> io::Result<Option<Vec<u8>>> {
        let key = key_or_ref
            .strip_prefix(BLOB_REF_PREFIX)
            .unwrap_or(key_or_ref);
        if !is_safe_key(key) {
            return Ok(None);
        }
        match fs::read(self.dir.join(key)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write `bytes` to `path` atomically: a unique temp file, fsync, rename into place, fsync
    /// the directory. A crash leaves at most a stale `*.tmp` (harmless; overwritten by the next
    /// put of the key). The trailing directory fsync matters too: without it, a crash right
    /// after `rename` can lose the directory entry even though the file's own bytes are durable,
    /// leaving a committed `evald-blob:<key>` reference pointing at nothing.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!(
            ".{}.{seq}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        fs::File::open(&self.dir)?.sync_all()
    }
}

/// A blob key is a content key: hex digits with optional `-<n>` collision suffix. Anything else
/// (path separators, `..`, non-hex) is rejected so `get` can't escape the blob directory.
fn is_safe_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Offload every span `input_value` / `output_value` longer than `cap` bytes to `store`,
/// replacing the field with a compact `evald-blob:<key>` reference and recording the original
/// size + a short inline preview under an `evald.blob.{input,output}` raw attribute (so a
/// reader sees something without a fetch). `cap == 0` disables offloading. Returns the number
/// of fields offloaded. A blob-write error leaves that field inline — never drops the span.
pub fn offload_large_payloads(
    spans: &mut [NormalizedSpan],
    store: &BlobStore,
    cap: usize,
) -> usize {
    if cap == 0 {
        return 0;
    }
    let mut offloaded = 0;
    for span in spans.iter_mut() {
        offloaded += offload_field(
            &mut span.input_value,
            &mut span.raw_attributes,
            "evald.blob.input",
            store,
            cap,
        );
        offloaded += offload_field(
            &mut span.output_value,
            &mut span.raw_attributes,
            "evald.blob.output",
            store,
            cap,
        );
        // Normalization preserves every attribute losslessly, so the *raw* `input.value` /
        // `output.value` (and any other oversized string attribute) would still bloat the
        // Parquet `raw_attributes_json` column. Cap those too — content-addressing dedups
        // them onto the same blob the normalized field already points at.
        offloaded += offload_raw_attributes(&mut span.raw_attributes, store, cap);
    }
    offloaded
}

/// Offload oversized *string* values in `raw_attributes`, replacing each with its
/// `evald-blob:<key>` reference. Skips our own `evald.blob.*` metadata and values that are
/// already references. Non-string values (nested arrays/objects) are left as-is — a first cut
/// covers the common big-payload string case (`input.value` / `output.value`).
fn offload_raw_attributes(
    attrs: &mut BTreeMap<String, Json>,
    store: &BlobStore,
    cap: usize,
) -> usize {
    let mut offloaded = 0;
    for (key, value) in attrs.iter_mut() {
        if key.starts_with("evald.blob.") {
            continue;
        }
        if let Json::String(s) = value {
            if s.len() > cap && !s.starts_with(BLOB_REF_PREFIX) {
                match store.put(s.as_bytes()) {
                    Ok(reference) => {
                        *value = Json::String(reference);
                        offloaded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(%e, key, "blob offload failed; leaving attribute inline")
                    }
                }
            }
        }
    }
    offloaded
}

/// Offload one field if it exceeds `cap`. Disjoint borrows of the two span fields, so the
/// caller passes `&mut span.input_value` and `&mut span.raw_attributes` directly.
fn offload_field(
    field: &mut Option<String>,
    attrs: &mut BTreeMap<String, Json>,
    attr_key: &str,
    store: &BlobStore,
    cap: usize,
) -> usize {
    let Some(val) = field.as_ref() else {
        return 0;
    };
    if val.len() <= cap {
        return 0;
    }
    match store.put(val.as_bytes()) {
        Ok(reference) => {
            let preview: String = val.chars().take(PREVIEW_CHARS).collect();
            attrs.insert(
                attr_key.to_string(),
                serde_json::json!({
                    "ref": reference,
                    "bytes": val.len(),
                    "preview": preview,
                }),
            );
            *field = Some(reference);
            1
        }
        Err(e) => {
            tracing::warn!(%e, attr_key, "blob offload failed; leaving payload inline");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;

    fn store() -> (BlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (BlobStore::open(dir.path().join("blobs")).unwrap(), dir)
    }

    #[test]
    fn put_get_roundtrips_and_dedups() {
        let (bs, _dir) = store();
        let payload = vec![b'x'; 10_000];
        let r1 = bs.put(&payload).unwrap();
        let r2 = bs.put(&payload).unwrap();
        // Content-addressed: identical bytes → identical reference (dedup).
        assert_eq!(r1, r2);
        assert!(r1.starts_with(BLOB_REF_PREFIX));
        assert_eq!(bs.get(&r1).unwrap().as_deref(), Some(payload.as_slice()));
        // A bare key (no prefix) resolves too.
        let key = r1.strip_prefix(BLOB_REF_PREFIX).unwrap();
        assert_eq!(bs.get(key).unwrap().as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn get_rejects_unknown_and_traversal_keys() {
        let (bs, _dir) = store();
        assert_eq!(bs.get("deadbeef").unwrap(), None); // unknown
        assert_eq!(bs.get("../etc/passwd").unwrap(), None); // traversal → rejected
        assert_eq!(bs.get("").unwrap(), None);
    }

    #[test]
    fn offload_replaces_only_oversized_fields() {
        let (bs, _dir) = store();
        let big = "A".repeat(5_000);
        let small = "hi".to_string();
        let mut span = test_span("aa", "01", 1_000);
        span.input_value = Some(big.clone());
        span.output_value = Some(small.clone());

        let n = offload_large_payloads(std::slice::from_mut(&mut span), &bs, 1_024);
        assert_eq!(n, 1, "only the oversized input is offloaded");

        // input_value became a reference; output_value stayed inline.
        let reference = span.input_value.clone().unwrap();
        assert!(reference.starts_with(BLOB_REF_PREFIX));
        assert_eq!(span.output_value.as_deref(), Some("hi"));

        // The reference resolves to the original bytes.
        assert_eq!(bs.get(&reference).unwrap().as_deref(), Some(big.as_bytes()));
        // Metadata (size + preview) was recorded for a fetch-free read.
        let meta = &span.raw_attributes["evald.blob.input"];
        assert_eq!(meta["bytes"], 5_000);
        assert_eq!(meta["ref"], reference);
        assert_eq!(meta["preview"].as_str().unwrap().len(), PREVIEW_CHARS);
        assert!(!span.raw_attributes.contains_key("evald.blob.output"));
    }

    #[test]
    fn cap_zero_disables_offload() {
        let (bs, _dir) = store();
        let mut span = test_span("aa", "01", 1_000);
        span.input_value = Some("A".repeat(100_000));
        assert_eq!(
            offload_large_payloads(std::slice::from_mut(&mut span), &bs, 0),
            0
        );
        assert_eq!(span.input_value.as_ref().unwrap().len(), 100_000); // untouched
    }
}
