//! Filesystem storage for audio blobs.
//!
//! The store knows filesystem paths under the data directory but nothing about
//! HTTP. Uploads are streamed to a temp file under `tmp/`, then atomically
//! renamed into `recordings/<recording_id>.wav`. Stored blob paths are
//! data-dir-relative so the data directory remains movable.

use std::io;
use std::path::{Path, PathBuf};

use tokio::fs;

const TMP_DIR: &str = "tmp";
const RECORDINGS_DIR: &str = "recordings";
const TEMP_SUFFIX: &str = "part";

/// Filesystem-backed blob storage rooted at a data directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    data_dir: PathBuf,
}

impl BlobStore {
    /// Create the store, ensuring `tmp/` and `recordings/` exist and removing
    /// any stale `tmp/*.part` files left by interrupted uploads.
    pub async fn new(data_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(data_dir.join(TMP_DIR)).await?;
        fs::create_dir_all(data_dir.join(RECORDINGS_DIR)).await?;
        clean_stale_temp(&data_dir).await?;
        Ok(Self { data_dir })
    }

    /// The root data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Absolute path of the temp file for an in-progress upload.
    pub fn temp_path(&self, recording_id: &str) -> PathBuf {
        self.data_dir
            .join(TMP_DIR)
            .join(format!("upload-{recording_id}.{TEMP_SUFFIX}"))
    }

    /// The data-dir-relative blob path stored in the database.
    pub fn relative_path(recording_id: &str) -> String {
        format!("{RECORDINGS_DIR}/{recording_id}.wav")
    }

    /// Absolute path for a data-dir-relative blob path.
    pub fn full_path(&self, relative: &str) -> PathBuf {
        self.data_dir.join(relative)
    }

    /// Atomically move a finished temp file into its final location.
    pub async fn finalize(&self, temp: &Path, relative: &str) -> io::Result<()> {
        fs::rename(temp, self.full_path(relative)).await
    }

    /// Best-effort removal of a temp file; missing files are ignored.
    pub async fn remove_temp(&self, temp: &Path) {
        let _ = fs::remove_file(temp).await;
    }

    /// Best-effort removal of a finalized blob; missing files are ignored.
    pub async fn remove_blob(&self, relative: &str) {
        let _ = fs::remove_file(self.full_path(relative)).await;
    }
}

async fn clean_stale_temp(data_dir: &Path) -> io::Result<()> {
    let mut entries = fs::read_dir(data_dir.join(TMP_DIR)).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some(TEMP_SUFFIX) {
            let _ = fs::remove_file(&path).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn new_creates_subdirectories() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).await.unwrap();
        assert!(store.data_dir().join("tmp").is_dir());
        assert!(store.data_dir().join("recordings").is_dir());
    }

    #[tokio::test]
    async fn new_removes_stale_temp_files() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("tmp")).await.unwrap();
        let stale = dir.path().join("tmp").join("upload-old.part");
        fs::write(&stale, b"leftover").await.unwrap();
        let keep = dir.path().join("tmp").join("notes.txt");
        fs::write(&keep, b"keep me").await.unwrap();

        BlobStore::new(dir.path()).await.unwrap();

        assert!(!stale.exists(), "stale .part file should be removed");
        assert!(keep.exists(), "non-.part files should be left alone");
    }

    #[tokio::test]
    async fn finalize_renames_into_recordings() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).await.unwrap();
        let temp = store.temp_path("rec-1");
        fs::write(&temp, b"audio").await.unwrap();

        let relative = BlobStore::relative_path("rec-1");
        store.finalize(&temp, &relative).await.unwrap();

        assert!(!temp.exists());
        assert_eq!(relative, "recordings/rec-1.wav");
        assert_eq!(
            fs::read(store.full_path(&relative)).await.unwrap(),
            b"audio"
        );
    }
}
