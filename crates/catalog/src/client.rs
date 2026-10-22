//! The HTTP client for one catalog connection: authentication, timeouts,
//! and JSON requests. It never retries and never follows redirects, so a
//! request is sent at most once; callers decide per operation.

use std::time::{Duration, Instant};

use bergsmith_common::secret::Secret;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use url::Url;

use crate::error::{CatalogError, ErrorEnvelope};

/// How the connection authenticates to the catalog.
#[derive(Clone, Debug)]
pub enum Auth {
    /// No authentication, for development catalogs.
    None,
    /// A static bearer token.
    BearerToken(Secret),
    /// The OAuth2 client-credentials grant.
    OAuth2 {
        /// The token endpoint.
        token_url: Url,
        /// The OAuth2 client ID.
        client_id: String,
        /// The OAuth2 client secret.
        client_secret: Secret,
        /// The scope to request, such as `catalog`.
        scope: Option<String>,
    },
}

/// How to reach a catalog.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    /// The catalog base URI, such as `https://lakekeeper.example/catalog`;
    /// requests go to `<uri>/v1/...`.
    pub uri: Url,
    /// The warehouse to request in `GET /v1/config`, if the catalog needs one.
    pub warehouse: Option<String>,
    /// Authentication.
    pub auth: Auth,
    /// Bound on connecting to the catalog.
    pub connect_timeout: Duration,
    /// Bound on each whole request, including the response body.
    pub request_timeout: Duration,
}

impl ConnectionConfig {
    /// Defaults for `uri`: no authentication, a 5 second connect timeout, and
    /// a 30 second request timeout.
    #[must_use]
    pub fn new(uri: Url) -> Self {
        Self {
            uri,
            warehouse: None,
            auth: Auth::None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// A cached OAuth2 access token.
struct Token {
    value: Secret,
    refresh_after: Instant,
}

/// Refresh tokens this long before they expire.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The most bytes one catalog response may carry, unless a caller asks for
/// less.
///
/// A response is buffered before it is decoded, so this is the bound on how
/// much memory one catalog call can take.
pub(crate) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The HTTP client for one connection.
pub(crate) struct HttpClient {
    http: reqwest::Client,
    auth: Auth,
    token: Mutex<Option<Token>>,
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

impl HttpClient {
    pub(crate) fn new(config: &ConnectionConfig) -> Result<Self, CatalogError> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .default_headers(headers)
            // A redirected commit would be sent twice, and reqwest's default
            // retry of refused HTTP/2 streams is still a second send.
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|source| CatalogError::Transport {
                operation: "build HTTP client",
                source,
            })?;
        Ok(Self {
            http,
            auth: config.auth.clone(),
            token: Mutex::new(None),
        })
    }

    /// A request with authentication applied, for callers that send it and
    /// classify the response themselves.
    ///
    /// # Errors
    ///
    /// Errors from fetching an OAuth2 token; nothing has been sent.
    pub(crate) async fn authorized(
        &self,
        method: Method,
        url: Url,
    ) -> Result<RequestBuilder, CatalogError> {
        self.authorize(self.http.request(method, url)).await
    }

    /// Sends a request and decodes a JSON response.
    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        method: Method,
        url: Url,
        configure: impl FnOnce(RequestBuilder) -> RequestBuilder,
    ) -> Result<T, CatalogError> {
        self.json_within(operation, method, url, configure, MAX_RESPONSE_BYTES)
            .await
    }

    /// Sends a request, reads at most `max_bytes`, and decodes a JSON
    /// response.
    ///
    /// A response is buffered before it is decoded, so the byte limit is what
    /// keeps a faulty or hostile catalog from exhausting memory. The limit
    /// covers error bodies too: an error envelope is read from the same
    /// stream.
    pub(crate) async fn json_within<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        method: Method,
        url: Url,
        configure: impl FnOnce(RequestBuilder) -> RequestBuilder,
        max_bytes: usize,
    ) -> Result<T, CatalogError> {
        let request = self
            .authorize(configure(self.http.request(method, url)))
            .await?;
        let mut response = request
            .send()
            .await
            .map_err(|source| CatalogError::Transport { operation, source })?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 419) {
            // A token can be revoked or expire before its advertised lifetime.
            // Fetch a fresh one next time instead of repeating a rejected token.
            self.invalidate_token().await;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| CatalogError::Transport { operation, source })?
        {
            if body.len() + chunk.len() > max_bytes {
                return Err(CatalogError::ResponseTooLarge {
                    operation,
                    max_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(CatalogError::Status {
                operation,
                status: status.as_u16(),
                model: ErrorEnvelope::decode(&body),
            });
        }
        serde_json::from_slice(&body).map_err(|source| CatalogError::Decode { operation, source })
    }

    /// Discards any cached OAuth2 token after the catalog rejected it, so
    /// the next request fetches a new one. Static and absent credentials
    /// are unaffected.
    pub(crate) async fn invalidate_token(&self) {
        if matches!(self.auth, Auth::OAuth2 { .. }) {
            self.token.lock().await.take();
        }
    }

    async fn authorize(&self, request: RequestBuilder) -> Result<RequestBuilder, CatalogError> {
        let token = match &self.auth {
            Auth::None => return Ok(request),
            Auth::BearerToken(token) => token.clone(),
            Auth::OAuth2 { .. } => self.oauth2_token().await?,
        };
        Ok(request.header(AUTHORIZATION, format!("Bearer {}", token.expose())))
    }

    /// Returns a cached OAuth2 token, fetching a new one when it is close
    /// to expiry. One fetch at a time.
    async fn oauth2_token(&self) -> Result<Secret, CatalogError> {
        let Auth::OAuth2 {
            token_url,
            client_id,
            client_secret,
            scope,
        } = &self.auth
        else {
            return Err(CatalogError::InvalidUrl(
                "no OAuth2 configuration".to_owned(),
            ));
        };
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref()
            && Instant::now() < token.refresh_after
        {
            return Ok(token.value.clone());
        }
        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.expose()),
        ];
        if let Some(scope) = scope {
            form.push(("scope", scope.as_str()));
        }
        let operation = "POST oauth2 token";
        let response = self
            .http
            .post(token_url.clone())
            .form(&form)
            .send()
            .await
            .map_err(|source| CatalogError::Transport { operation, source })?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(CatalogError::Authentication {
                status: status.as_u16(),
            });
        }
        let body = response
            .bytes()
            .await
            .map_err(|source| CatalogError::Transport { operation, source })?;
        let token: TokenResponse = serde_json::from_slice(&body)
            .map_err(|source| CatalogError::Decode { operation, source })?;
        let lifetime = Duration::from_secs(token.expires_in.unwrap_or(3600));
        let value = Secret::new(token.access_token);
        *cached = Some(Token {
            value: value.clone(),
            refresh_after: Instant::now() + lifetime.saturating_sub(TOKEN_REFRESH_MARGIN),
        });
        Ok(value)
    }
}
