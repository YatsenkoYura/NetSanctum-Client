use std::time::Duration;

use cookie::Cookie;
use reqwest::{
    Client, StatusCode,
    header::{COOKIE, HeaderMap, SET_COOKIE},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::error::{AppError, AppResult};

const MAX_AUTH_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize)]
struct SessionRequest<'a> {
    master_token: &'a str,
    client: ClientIdentity<'a>,
}

#[derive(Debug, Serialize)]
struct ClientIdentity<'a> {
    name: &'a str,
    version: &'a str,
    protocol_version: u8,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    token_type: String,
    node_name: String,
}

pub struct DesktopSession {
    pub credential: SessionCredential,
    pub web_cookie: Zeroizing<String>,
    pub expires_in: u64,
    pub node_name: String,
}

#[derive(Clone)]
pub enum SessionCredential {
    Bearer(Zeroizing<String>),
    Cookie(Zeroizing<String>),
}

impl SessionCredential {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bearer(secret) | Self::Cookie(secret) => secret.is_empty(),
        }
    }
}

pub struct NodeClient {
    client: Client,
    download_client: Client,
}

impl NodeClient {
    pub fn new() -> AppResult<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .redirect(Policy::none())
            .https_only(false)
            .user_agent(concat!("NetSanctumDesktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let download_client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .redirect(Policy::none())
            .https_only(false)
            .user_agent(concat!("NetSanctumDesktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        Ok(Self {
            client,
            download_client,
        })
    }

    pub async fn get_authenticated(
        &self,
        node_url: &Url,
        relative_url: &str,
        credential: &SessionCredential,
    ) -> AppResult<reqwest::Response> {
        if !relative_url.starts_with('/') || relative_url.starts_with("//") {
            return Err(AppError::InvalidPackage(
                "resource URL must be an absolute path".into(),
            ));
        }
        let url = node_url
            .join(relative_url)
            .map_err(|error| AppError::InvalidPackage(error.to_string()))?;
        if url.origin() != node_url.origin() || url.fragment().is_some() {
            return Err(AppError::InvalidPackage(
                "resource URL must remain on the node origin".into(),
            ));
        }
        let request = self.download_client.get(url).header("Accept", "*/*");
        let request = match credential {
            SessionCredential::Bearer(token) => request.bearer_auth(token.as_str()),
            SessionCredential::Cookie(session_id) => {
                request.header(COOKIE, format!("access_token={}", session_id.as_str()))
            }
        };
        request
            .send()
            .await
            .map_err(|error| AppError::Download(safe_network_error(&error)))
    }

    pub async fn create_session(
        &self,
        node_url: &Url,
        master_token: &str,
    ) -> AppResult<DesktopSession> {
        let endpoint = node_url
            .join("auth/desktop/session")
            .map_err(|error| AppError::InvalidNodeUrl(error.to_string()))?;
        let response = self
            .client
            .post(endpoint)
            .header("Accept", "application/json")
            .json(&SessionRequest {
                master_token,
                client: ClientIdentity {
                    name: "NetSanctum Desktop",
                    version: env!("CARGO_PKG_VERSION"),
                    protocol_version: 1,
                },
            })
            .send()
            .await
            .map_err(|error| AppError::NodeUnavailable(safe_network_error(&error)))?;

        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(AppError::InvalidCredentials);
            }
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => {
                return self.create_cookie_session(node_url, master_token).await;
            }
            status if !status.is_success() => {
                return Err(AppError::InvalidNodeResponse(format!("HTTP {status}")));
            }
            _ => {}
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_AUTH_RESPONSE_BYTES)
        {
            return Err(AppError::InvalidNodeResponse(
                "слишком большой ответ авторизации".into(),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::InvalidNodeResponse(error.to_string()))?;
        if bytes.len() as u64 > MAX_AUTH_RESPONSE_BYTES {
            return Err(AppError::InvalidNodeResponse(
                "слишком большой ответ авторизации".into(),
            ));
        }
        let mut response: SessionResponse = serde_json::from_slice(&bytes)
            .map_err(|error| AppError::InvalidNodeResponse(error.to_string()))?;
        if response.access_token.is_empty()
            || response.expires_in == 0
            || response.expires_in > 86_400
        {
            response.access_token.zeroize();
            return Err(AppError::InvalidNodeResponse(
                "некорректный срок или токен сессии".into(),
            ));
        }
        if !response.token_type.is_empty() && !response.token_type.eq_ignore_ascii_case("bearer") {
            response.access_token.zeroize();
            return Err(AppError::InvalidNodeResponse(
                "поддерживается только Bearer session".into(),
            ));
        }

        let web_session = self.create_cookie_session(node_url, master_token).await?;
        Ok(DesktopSession {
            credential: SessionCredential::Bearer(Zeroizing::new(response.access_token)),
            web_cookie: web_session.web_cookie,
            expires_in: response.expires_in,
            node_name: response.node_name,
        })
    }

    async fn create_cookie_session(
        &self,
        node_url: &Url,
        master_token: &str,
    ) -> AppResult<DesktopSession> {
        let login_endpoint = node_url
            .join("auth/ui/login")
            .map_err(|error| AppError::InvalidNodeUrl(error.to_string()))?;
        let login_response = self
            .client
            .post(login_endpoint)
            .header("Accept", "text/html")
            .form(&[("token", master_token)])
            .send()
            .await
            .map_err(|error| AppError::NodeUnavailable(safe_network_error(&error)))?;

        if login_response.status() == StatusCode::NOT_FOUND
            || login_response.status() == StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(AppError::UnsupportedNode);
        }
        if !login_response.status().is_success() {
            return Err(AppError::InvalidNodeResponse(format!(
                "login HTTP {}",
                login_response.status()
            )));
        }
        let session_id = extract_session_cookie(login_response.headers())?
            .ok_or(AppError::InvalidCredentials)?;

        let cookie_header = Zeroizing::new(format!("access_token={}", session_id.as_str()));
        let me_endpoint = node_url
            .join("auth/me")
            .map_err(|error| AppError::InvalidNodeUrl(error.to_string()))?;
        let verification = self
            .client
            .get(me_endpoint)
            .header("Accept", "application/json")
            .header(COOKIE, cookie_header.as_str())
            .send()
            .await
            .map_err(|error| AppError::NodeUnavailable(safe_network_error(&error)))?;
        if verification.status() == StatusCode::UNAUTHORIZED
            || verification.status() == StatusCode::FORBIDDEN
        {
            return Err(AppError::InvalidCredentials);
        }
        if !verification.status().is_success() {
            return Err(AppError::InvalidNodeResponse(format!(
                "verification HTTP {}",
                verification.status()
            )));
        }

        let node_name = node_url
            .host_str()
            .ok_or_else(|| AppError::InvalidNodeUrl("адрес не содержит host".into()))?
            .to_owned();
        Ok(DesktopSession {
            credential: SessionCredential::Cookie(session_id.clone()),
            web_cookie: session_id,
            expires_in: 86_400,
            node_name,
        })
    }
}

fn extract_session_cookie(headers: &HeaderMap) -> AppResult<Option<Zeroizing<String>>> {
    for value in headers.get_all(SET_COOKIE) {
        let raw = value
            .to_str()
            .map_err(|_| AppError::InvalidNodeResponse("некорректный Set-Cookie".into()))?;
        let cookie = Cookie::parse(raw.to_owned())
            .map_err(|_| AppError::InvalidNodeResponse("некорректный Set-Cookie".into()))?;
        if cookie.name() == "access_token" {
            let value = cookie.value();
            if value.is_empty() || value.len() > 256 || !value.is_ascii() {
                return Err(AppError::InvalidNodeResponse(
                    "некорректная временная сессия".into(),
                ));
            }
            return Ok(Some(Zeroizing::new(value.to_owned())));
        }
    }
    Ok(None)
}

fn safe_network_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "превышено время ожидания".into()
    } else if error.is_connect() {
        "не удалось установить соединение".into()
    } else {
        "ошибка защищённого HTTP-клиента".into()
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

    use super::extract_session_cookie;

    #[test]
    fn extracts_only_access_token_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("lang=ru; Path=/"));
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("access_token=2d47f940-test; HttpOnly; SameSite=Lax"),
        );
        let session = extract_session_cookie(&headers).unwrap().unwrap();
        assert_eq!(session.as_str(), "2d47f940-test");
    }

    #[test]
    fn missing_session_cookie_is_not_success() {
        assert!(extract_session_cookie(&HeaderMap::new()).unwrap().is_none());
    }
}
