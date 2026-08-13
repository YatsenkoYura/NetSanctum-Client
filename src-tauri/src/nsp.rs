use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
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
        let entries: HashMap<String, RawEntry> = serde_json::from_slice(&index_bytes)
            .map_err(|error| AppError::InvalidPackage(format!("invalid NSP index: {error}")))?;
        for (url, entry) in &entries {
            if !url.starts_with('/')
                || entry.length > index_offset
                || entry.offset > index_offset.saturating_sub(entry.length)
                || entry.mime.len() > 256
            {
                return Err(AppError::InvalidPackage(
                    "invalid NSP resource entry".into(),
                ));
            }
        }
        Ok(Self {
            path: path.to_owned(),
            entries,
        })
    }

    pub fn get(&self, url: &str) -> Option<NspEntry> {
        self.entries.get(url).map(|entry| NspEntry {
            path: self.path.clone(),
            offset: entry.offset,
            length: entry.length,
            mime: entry.mime.clone(),
        })
    }

    pub fn find_equivalent(&self, url: &str) -> Option<NspEntry> {
        let expected = normalize_url(url)?;
        self.entries.iter().find_map(|(candidate, entry)| {
            (normalize_url(candidate).as_deref() == Some(expected.as_str())).then(|| NspEntry {
                path: self.path.clone(),
                offset: entry.offset,
                length: entry.length,
                mime: entry.mime.clone(),
            })
        })
    }
}

fn normalize_url(value: &str) -> Option<String> {
    let base = url::Url::parse("http://offline.invalid/").ok()?;
    let mut url = base.join(value).ok()?;
    let mut query = url
        .query_pairs()
        .filter(|(key, _)| key != "package_id" && key != "sort_by")
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

    #[tokio::test]
    async fn reads_valid_nsp_index() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"hello").unwrap();
        let index = br#"{"/hello":{"offset":0,"length":5,"mime":"text/plain"}}"#;
        file.write_all(index).unwrap();
        file.write_all(&5_u64.to_be_bytes()).unwrap();
        file.write_all(b"NSPK").unwrap();
        file.flush().unwrap();

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
    }
}
