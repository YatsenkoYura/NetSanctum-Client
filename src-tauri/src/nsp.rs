use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::{AppError, AppResult};

const FOOTER_LENGTH: u64 = 12;
const MAX_INDEX_LENGTH: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NspEntry {
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
    pub mime: String,
}

pub struct NspIndex {
    path: PathBuf,
    entries: HashMap<String, RawEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawEntry {
    offset: u64,
    length: u64,
    mime: String,
    #[serde(default)]
    sha256: Option<String>,
}

impl NspIndex {
    pub async fn open(path: &Path) -> AppResult<Self> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let file_length = file
            .metadata()
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?
            .len();
        if file_length < FOOTER_LENGTH {
            return Err(AppError::InvalidPackage("NSP footer is missing".into()));
        }
        file.seek(std::io::SeekFrom::Start(file_length - FOOTER_LENGTH))
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut footer = [0_u8; FOOTER_LENGTH as usize];
        file.read_exact(&mut footer)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        if &footer[8..] != b"NSPK" {
            return Err(AppError::InvalidPackage("invalid NSP magic".into()));
        }
        let index_offset = u64::from_be_bytes(
            footer[..8]
                .try_into()
                .map_err(|_| AppError::InvalidPackage("invalid NSP footer".into()))?,
        );
        let index_end = file_length - FOOTER_LENGTH;
        if index_offset > index_end || index_end - index_offset > MAX_INDEX_LENGTH {
            return Err(AppError::InvalidPackage("invalid NSP index offset".into()));
        }
        file.seek(std::io::SeekFrom::Start(index_offset))
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut index_bytes = vec![0_u8; (index_end - index_offset) as usize];
        file.read_exact(&mut index_bytes)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut entries: HashMap<String, RawEntry> = serde_json::from_slice(&index_bytes)
            .map_err(|error| AppError::InvalidPackage(format!("invalid NSP index: {error}")))?;
        for (url, entry) in &mut entries {
            if !url.starts_with('/')
                || entry.length > index_offset
                || entry.offset > index_offset.saturating_sub(entry.length)
                || entry.mime.len() > 256
            {
                return Err(AppError::InvalidPackage(
                    "invalid NSP resource entry".into(),
                ));
            }
            if let Some(sha256) = &entry.sha256 {
                if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(AppError::InvalidPackage("invalid NSP entry hash".into()));
                }
                // The manifest contract compares lowercase hexadecimal; keep
                // the index entry in the same shape so callers never case-fold.
                entry.sha256 = Some(sha256.to_ascii_lowercase());
            }
        }
        Ok(Self {
            path: path.to_owned(),
            entries,
        })
    }

    pub fn get(&self, url: &str) -> Option<NspEntry> {
        self.entries.get(url).map(|entry| self.entry_at(entry))
    }

    pub fn find_equivalent(&self, url: &str) -> Option<NspEntry> {
        let expected = normalize_url(url)?;
        self.entries.iter().find_map(|(candidate, entry)| {
            (normalize_url(candidate).as_deref() == Some(expected.as_str()))
                .then(|| self.entry_at(entry))
        })
    }

    /// Hash every entry that carries an index hash and compare it against the
    /// bytes on disk. This is the integrity pass for a container whose own
    /// manifest identity is missing: a dynamic NSP is rebuilt on every node
    /// request, so no `sha256` exists for the file as a whole and the
    /// per-entry hashes in its index are the only record of what the node
    /// actually packed.
    pub async fn verify_all(&self) -> AppResult<()> {
        let mut file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut buffer = vec![0_u8; 64 * 1024];
        for (url, entry) in &self.entries {
            let Some(expected) = &entry.sha256 else {
                continue;
            };
            file.seek(std::io::SeekFrom::Start(entry.offset))
                .await
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let mut hasher = Sha256::new();
            let mut remaining = entry.length;
            while remaining > 0 {
                let limit = buffer.len().min(remaining as usize);
                file.read_exact(&mut buffer[..limit])
                    .await
                    .map_err(|error| {
                        AppError::InvalidPackage(format!("cannot read NSP entry {url}: {error}"))
                    })?;
                hasher.update(&buffer[..limit]);
                remaining -= limit as u64;
            }
            if format!("{:x}", hasher.finalize()) != *expected {
                return Err(AppError::InvalidPackage(format!(
                    "NSP entry hash mismatch: {url}"
                )));
            }
        }
        Ok(())
    }

    fn entry_at(&self, entry: &RawEntry) -> NspEntry {
        NspEntry {
            path: self.path.clone(),
            offset: entry.offset,
            length: entry.length,
            mime: entry.mime.clone(),
        }
    }
}

fn normalize_url(value: &str) -> Option<String> {
    let base = url::Url::parse("http://offline.invalid/").ok()?;
    let mut url = base.join(value).ok()?;
    let mut query = url
        .query_pairs()
        .filter(|(key, _)| key != "package_id" && key != "sort_by" && key != "v")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    query.sort();
    url.set_query(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Some(match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::NspIndex;

    /// SHA-256 of `hello` — the bytes the fixture packs as one entry.
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn write_container(data: &[u8], index: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();
        file.write_all(index.as_bytes()).unwrap();
        file.write_all(&(data.len() as u64).to_be_bytes()).unwrap();
        file.write_all(b"NSPK").unwrap();
        file.flush().unwrap();
        file
    }

    #[tokio::test]
    async fn reads_valid_nsp_index() {
        let file = write_container(
            b"hello",
            r#"{"/hello":{"offset":0,"length":5,"mime":"text/plain"}}"#,
        );

        let reader = NspIndex::open(file.path()).await.unwrap();
        let entry = reader.get("/hello").unwrap();
        assert_eq!(0, entry.offset);
        assert_eq!(5, entry.length);
        assert_eq!("text/plain", entry.mime);
        assert!(
            reader
                .find_equivalent("/hello?package_id=anything")
                .is_some()
        );
        assert!(reader.find_equivalent("/hello?v=cache-buster").is_some());
    }

    #[tokio::test]
    async fn reads_and_normalizes_entry_hash() {
        let file = write_container(
            b"hello",
            &format!(
                r#"{{"/hello":{{"offset":0,"length":5,"mime":"text/plain","sha256":"{}"}}}}"#,
                HELLO_SHA256.to_uppercase()
            ),
        );

        let reader = NspIndex::open(file.path()).await.unwrap();
        // Uppercase input arrives normalized, so verification never has to
        // case-fold.
        reader.verify_all().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_malformed_entry_hash() {
        let file = write_container(
            b"hello",
            r#"{"/hello":{"offset":0,"length":5,"mime":"text/plain","sha256":"zz"}}"#,
        );

        let error = NspIndex::open(file.path())
            .await
            .err()
            .expect("a malformed entry hash must not open");
        assert!(
            error.to_string().contains("entry hash"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn verify_all_rejects_corrupted_entry() {
        // The body no longer matches the hash the index declares for it.
        let file = write_container(
            b"hellX",
            &format!(
                r#"{{"/hello":{{"offset":0,"length":5,"mime":"text/plain","sha256":"{HELLO_SHA256}"}}}}"#
            ),
        );

        let reader = NspIndex::open(file.path()).await.unwrap();
        let error = reader.verify_all().await.unwrap_err();
        assert!(
            error.to_string().contains("/hello"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn verify_all_skips_entries_without_hashes() {
        let file = write_container(
            b"hello",
            r#"{"/hello":{"offset":0,"length":5,"mime":"text/plain"}}"#,
        );

        let reader = NspIndex::open(file.path()).await.unwrap();
        reader.verify_all().await.unwrap();
    }
}
