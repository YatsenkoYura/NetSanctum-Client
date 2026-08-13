use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{AppError, AppResult};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredConfig {
    pub node_url: Url,
    pub allow_insecure_http: bool,
}

pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("connection.json"),
        }
    }

    pub fn load(&self) -> AppResult<Option<StoredConfig>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path).map_err(|error| AppError::Storage(error.to_string()))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| AppError::Storage(format!("повреждён connection.json: {error}")))
    }

    pub fn save(&self, config: &StoredConfig) -> AppResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| AppError::Storage("нет каталога конфигурации".into()))?;
        fs::create_dir_all(parent).map_err(|error| AppError::Storage(error.to_string()))?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| AppError::Storage(error.to_string()))?;

        let payload = serde_json::to_vec_pretty(config)
            .map_err(|error| AppError::Storage(error.to_string()))?;
        crate::atomic_file::write(&self.path, &payload)
    }

    pub fn clear(&self) -> AppResult<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).map_err(|error| AppError::Storage(error.to_string()))?;
        }
        Ok(())
    }
}

pub fn normalize_node_url(raw: &str, allow_insecure_http: bool) -> AppResult<Url> {
    let mut url =
        Url::parse(raw.trim()).map_err(|error| AppError::InvalidNodeUrl(error.to_string()))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(AppError::InvalidNodeUrl(
            "разрешены только https:// и http://".into(),
        ));
    }
    if url.scheme() == "http" && !allow_insecure_http {
        return Err(AppError::InsecureTransport);
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::InvalidNodeUrl(
            "адрес должен содержать host и не должен содержать логин или пароль".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(AppError::InvalidNodeUrl(
            "query-параметры и fragment в адресе узла запрещены".into(),
        ));
    }
    let normalized_path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&normalized_path);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::normalize_node_url;

    #[test]
    fn rejects_http_without_explicit_consent() {
        assert!(normalize_node_url("http://192.168.1.10:8000", false).is_err());
        assert!(normalize_node_url("http://192.168.1.10:8000", true).is_ok());
    }

    #[test]
    fn strips_trailing_slash() {
        let url = normalize_node_url("https://sanctum.example/", false).unwrap();
        assert_eq!(url.as_str(), "https://sanctum.example/");
    }

    #[test]
    fn preserves_reverse_proxy_path_for_relative_endpoints() {
        let url = normalize_node_url("https://example.net/sanctum", false).unwrap();
        assert_eq!(
            url.join("auth/desktop/session").unwrap().path(),
            "/sanctum/auth/desktop/session"
        );
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert!(normalize_node_url("https://owner:secret@sanctum.example", false).is_err());
    }
}
