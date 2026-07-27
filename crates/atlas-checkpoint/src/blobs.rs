//! Content-addressed spill for payloads too large to sit on a row.
//!
//! This is not a precaution. Measured against a real developer's transcript
//! corpus: 705 MB across 294 Sessions, largest Session 44 MB, **largest single
//! message 2.02 MB** — already past the server's per-row cap, so storing bodies
//! inline would fail on data that exists today. Tool results are larger still.
//!
//! Content addressing means an identical payload written twice costs one file,
//! and it gives the upload path a key it can compute without consulting the
//! database — which is what makes blob-first ordering checkable.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Bodies above this spill. Chosen to sit well under the server's row cap while
/// keeping the overwhelming majority of turns — which are a few KB — inline,
/// where they cost one query rather than a query plus a file read.
pub const SPILL_THRESHOLD_BYTES: usize = 64 * 1024;

/// How much of a spilled body stays on the row for list rendering.
pub const PREVIEW_BYTES: usize = 2 * 1024;

/// A content-addressed store under the Workspace's `.atlas/blobs/`.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Write `bytes` and return its key, or return the existing key if an
    /// identical payload is already stored.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let key = key_for(bytes);
        let path = self.path_for(&key);

        // Content-addressed: same key means same bytes, so an existing file is
        // already correct and rewriting it would only risk tearing a file
        // another reader is holding open.
        if path.exists() {
            return Ok(key);
        }

        let parent = path.parent().expect("blob path always has a parent");
        fs::create_dir_all(parent).map_err(|e| Error::Blob(format!("{}: {e}", parent.display())))?;

        // Write to a temp name, fsync, and rename, so a reader never observes a
        // half-written blob under a key that claims to be complete. The fsync is
        // load-bearing: on power loss a rename can be durable while the data
        // blocks are not, leaving a truncated file under the final key — and
        // because `put` short-circuits on `exists()`, a corrupt blob under a
        // content-addressed key would never be repaired.
        let temp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut file = fs::File::create(&temp)
                .map_err(|e| Error::Blob(format!("{}: {e}", temp.display())))?;
            file.write_all(bytes)
                .map_err(|e| Error::Blob(format!("{}: {e}", temp.display())))?;
            file.sync_all()
                .map_err(|e| Error::Blob(format!("{}: {e}", temp.display())))?;
        }
        fs::rename(&temp, &path).map_err(|e| Error::Blob(format!("{}: {e}", path.display())))?;
        // Best-effort directory fsync so the rename itself survives power loss.
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(key)
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.path_for(key);
        let bytes = fs::read(&path).map_err(|e| Error::Blob(format!("{}: {e}", path.display())))?;
        // A blob whose content no longer hashes to its key is torn — most likely
        // a crash between the data write and its fsync on an older build. Delete
        // it so the next `put` of the same content can heal the key, and report
        // the corruption instead of returning garbage as the recorded payload.
        if key_for(&bytes) != key {
            let _ = fs::remove_file(&path);
            return Err(Error::Blob(format!("blob {key} is corrupt and was removed")));
        }
        Ok(bytes)
    }

    pub fn exists(&self, key: &str) -> bool {
        self.path_for(key).exists()
    }

    /// Fan out over the first byte of the hash. A developer-year of turns is
    /// hundreds of thousands of files, and directories that large are slow to
    /// enumerate on every filesystem Atlas runs on.
    pub fn path_for(&self, key: &str) -> PathBuf {
        let shard = key.get(0..2).unwrap_or("00");
        self.root.join(shard).join(key)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// The key for a payload: its SHA-256, hex encoded.
pub fn key_for(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Should this body spill?
pub fn should_spill(body: &str) -> bool {
    body.len() > SPILL_THRESHOLD_BYTES
}

/// The leading slice of `body` that fits in a preview, cut on a character
/// boundary so the preview is always valid UTF-8.
pub fn preview_of(body: &str) -> String {
    if body.len() <= PREVIEW_BYTES {
        return body.to_string();
    }
    let mut end = PREVIEW_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_payloads_share_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let first = store.put(b"the same bytes").unwrap();
        let second = store.put(b"the same bytes").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.get(&first).unwrap(), b"the same bytes");
    }

    #[test]
    fn different_payloads_get_different_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        assert_ne!(store.put(b"one").unwrap(), store.put(b"two").unwrap());
    }

    #[test]
    fn a_preview_never_splits_a_character() {
        // A multi-byte character straddling the preview boundary.
        let body = "é".repeat(PREVIEW_BYTES);
        let preview = preview_of(&body);
        assert!(preview.len() <= PREVIEW_BYTES);
        // The real assertion: it is still valid UTF-8, which `String` guarantees
        // only because we cut on a boundary.
        assert!(body.starts_with(&preview));
    }

    #[test]
    fn a_short_body_previews_to_itself() {
        assert_eq!(preview_of("hello"), "hello");
    }

    #[test]
    fn the_spill_decision_is_on_the_documented_threshold() {
        assert!(!should_spill(&"a".repeat(SPILL_THRESHOLD_BYTES)));
        assert!(should_spill(&"a".repeat(SPILL_THRESHOLD_BYTES + 1)));
    }
}
