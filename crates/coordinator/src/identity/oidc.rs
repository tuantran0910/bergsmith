//! OpenID Connect authorization-code flow with PKCE.
//!
//! The coordinator is a confidential client. It discovers the provider from
//! `<issuer>/.well-known/openid-configuration`, verifies the ID token's RS256
//! signature against the provider's JWKS with `ring`, and validates the
//! issuer, audience, authorized party, lifetime, and nonce. No JWT library is
//! used, so the accepted algorithms are explicit and small.
//!
//! Only `RS256`, `RS384`, and `RS512` are accepted. `none`, HMAC, and elliptic
//! curve algorithms are rejected rather than negotiated.

use std::fmt;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bergsmith_common::secret::Secret;
use ring::signature::{
    RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384, RSA_PKCS1_2048_8192_SHA512,
    RsaPublicKeyComponents,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use super::random;

/// How the client authenticates to the token endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenEndpointAuth {
    /// HTTP Basic with the client secret (`client_secret_basic`).
    ClientSecretBasic,
    /// Client ID and secret in the form body (`client_secret_post`).
    ClientSecretPost,
    /// A public client with no secret.
    None,
}

/// Provider configuration supplied by the operator.
#[derive(Clone)]
pub struct OidcConfig {
    /// Issuer, compared exactly with the `iss` claim and used to locate
    /// `/.well-known/openid-configuration`. The exact string is preserved, so
    /// a provider that publishes no trailing slash still validates.
    pub issuer: String,
    /// Client identifier registered with the provider.
    pub client_id: String,
    /// Client secret for a confidential client.
    pub client_secret: Option<Secret>,
    /// Redirect URI registered with the provider.
    pub redirect_uri: Url,
    /// Scopes requested; `openid` is always added.
    pub scopes: Vec<String>,
    /// How to authenticate at the token endpoint.
    pub token_endpoint_auth: TokenEndpointAuth,
    /// Allow a plain-HTTP issuer, for the development fixture only.
    pub allow_insecure_http: bool,
    /// Clock skew allowed for `exp` and `iat`.
    pub leeway: Duration,
    /// Timeout for each provider HTTP call: discovery, token exchange, JWKS.
    pub request_timeout: Duration,
    /// HTTP client used for discovery, token exchange, and JWKS fetches.
    pub http: reqwest::Client,
}

impl fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("token_endpoint_auth", &self.token_endpoint_auth)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("leeway", &self.leeway)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

/// The provider's discovery document, restricted to the fields used here.
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderMetadata {
    /// Issuer identifier.
    pub issuer: String,
    /// Authorization endpoint.
    pub authorization_endpoint: String,
    /// Token endpoint.
    pub token_endpoint: String,
    /// JWKS endpoint.
    pub jwks_uri: String,
    /// Algorithms the provider will sign ID tokens with.
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
}

/// A verified person identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcIdentity {
    /// Issuer from the verified token.
    pub issuer: String,
    /// Subject from the verified token.
    pub subject: String,
    /// Human-readable name chosen from `name`, `preferred_username`,
    /// `email`, or the subject.
    pub display_name: String,
    /// Email, when the provider released it.
    pub email: Option<String>,
}

/// A PKCE verifier and its S256 challenge.
#[derive(Clone, Debug)]
pub struct Pkce {
    verifier: Secret,
    challenge: String,
}

impl Pkce {
    /// Generates a 32-byte verifier and computes its S256 challenge.
    ///
    /// # Errors
    ///
    /// [`OidcError::Random`] when the random source fails.
    pub fn generate() -> Result<Self, OidcError> {
        let verifier = random::token().ok_or(OidcError::Random)?;
        let digest = Sha256::digest(verifier.as_bytes());
        Ok(Self {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier: Secret::new(verifier),
        })
    }

    /// The `code_challenge` parameter.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The verifier, sent only to the token endpoint.
    #[must_use]
    pub fn verifier(&self) -> &Secret {
        &self.verifier
    }
}

/// An error from discovery, the token exchange, or ID-token validation.
#[derive(Debug, Error)]
pub enum OidcError {
    /// Configuration is invalid.
    #[error("invalid OIDC configuration: {0}")]
    Configuration(&'static str),
    /// Secure randomness failed.
    #[error("secure random generation failed")]
    Random,
    /// Discovery could not be fetched.
    #[error("OIDC discovery request failed")]
    DiscoveryUnavailable,
    /// The provider answered discovery with a non-success status.
    #[error("OIDC discovery returned status {0}")]
    DiscoveryStatus(u16),
    /// Discovery is missing a required endpoint.
    #[error("OIDC discovery is missing {0}")]
    DiscoveryIncomplete(&'static str),
    /// The discovery document names a different issuer.
    #[error("OIDC discovery issuer does not match the configured issuer")]
    IssuerDiscoveryMismatch,
    /// The authorization endpoint is not a usable URL.
    #[error("OIDC authorization endpoint is invalid")]
    AuthorizeEndpointInvalid,
    /// The token request failed at the transport layer.
    #[error("OIDC token request failed")]
    TokenUnavailable,
    /// The token endpoint returned a non-success status.
    #[error("OIDC token endpoint returned status {0}")]
    TokenStatus(u16),
    /// The token response is missing or malformed.
    #[error("OIDC token response is invalid")]
    TokenResponseInvalid,
    /// A provider HTTP call exceeded its deadline.
    #[error("OIDC provider request timed out")]
    Timeout,
    /// The ID token is not a compact JWS with three parts.
    #[error("ID token is malformed")]
    IdTokenMalformed,
    /// The ID token names an algorithm this coordinator does not accept.
    #[error("ID token uses an unsupported algorithm")]
    UnsupportedAlgorithm,
    /// No JWKS key matches the token's `kid`.
    #[error("ID token signing key is unknown")]
    UnknownKey,
    /// The JWKS could not be fetched or parsed.
    #[error("OIDC JWKS unavailable")]
    JwksUnavailable,
    /// The JWKS key is not a usable RSA verification key.
    #[error("OIDC JWKS contains an unusable key")]
    InvalidKey,
    /// The signature does not verify.
    #[error("ID token signature is invalid")]
    SignatureInvalid,
    /// The `iss` claim differs from the configured issuer.
    #[error("ID token issuer does not match")]
    IssuerMismatch,
    /// The `aud` claim does not include this client.
    #[error("ID token audience does not match")]
    AudienceMismatch,
    /// `azp` is missing or wrong for a multi-audience token.
    #[error("ID token authorized party does not match")]
    AuthorizedPartyMismatch,
    /// The token has expired.
    #[error("ID token has expired")]
    Expired,
    /// The token was issued in the future beyond the allowed skew.
    #[error("ID token was issued in the future")]
    IssuedInFuture,
    /// The token's not-before time is still in the future.
    #[error("ID token is not yet valid")]
    NotYetValid,
    /// The `nonce` claim does not match the login attempt.
    #[error("ID token nonce does not match")]
    NonceMismatch,
    /// The token has no subject.
    #[error("ID token has no subject")]
    MissingSubject,
    /// A claim has an invalid type or value.
    #[error("ID token claim {0} is invalid")]
    InvalidClaim(&'static str),
}

impl OidcError {
    /// A stable reason code for operator logs; never includes token content.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "OidcConfigurationInvalid",
            Self::Random => "OidcRandomUnavailable",
            Self::DiscoveryUnavailable => "OidcDiscoveryUnavailable",
            Self::DiscoveryStatus(_) => "OidcDiscoveryRejected",
            Self::DiscoveryIncomplete(_) => "OidcDiscoveryIncomplete",
            Self::IssuerDiscoveryMismatch => "OidcIssuerDiscoveryMismatch",
            Self::AuthorizeEndpointInvalid => "OidcAuthorizeEndpointInvalid",
            Self::TokenUnavailable => "OidcTokenUnavailable",
            Self::TokenStatus(_) => "OidcTokenRejected",
            Self::TokenResponseInvalid => "OidcTokenResponseInvalid",
            Self::Timeout => "OidcTimeout",
            Self::IdTokenMalformed => "OidcIdTokenMalformed",
            Self::UnsupportedAlgorithm => "OidcAlgorithmUnsupported",
            Self::UnknownKey => "OidcKeyUnknown",
            Self::JwksUnavailable => "OidcJwksUnavailable",
            Self::InvalidKey => "OidcKeyInvalid",
            Self::SignatureInvalid => "OidcSignatureInvalid",
            Self::IssuerMismatch => "OidcIssuerMismatch",
            Self::AudienceMismatch => "OidcAudienceMismatch",
            Self::AuthorizedPartyMismatch => "OidcAuthorizedPartyMismatch",
            Self::Expired => "OidcExpired",
            Self::IssuedInFuture => "OidcIssuedInFuture",
            Self::NotYetValid => "OidcNotYetValid",
            Self::NonceMismatch => "OidcNonceMismatch",
            Self::MissingSubject => "OidcSubjectMissing",
            Self::InvalidClaim(_) => "OidcClaimInvalid",
        }
    }
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, client_id: &str) -> bool {
        match self {
            Self::One(value) => value == client_id,
            Self::Many(values) => values.iter().any(|value| value == client_id),
        }
    }

    fn is_multiple(&self) -> bool {
        matches!(self, Self::Many(values) if values.len() > 1)
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: i64,
    /// Issued-at time. Required by OpenID Connect Core; a missing or null
    /// value is a malformed token rather than an unchecked claim.
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Clone, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default, rename = "use")]
    usage: Option<String>,
    /// RSA modulus. Optional so a provider document that also publishes
    /// non-RSA keys still deserializes; selection rejects keys without it.
    #[serde(default)]
    n: Option<String>,
    /// RSA exponent, optional for the same reason.
    #[serde(default)]
    e: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

struct CachedJwks {
    keys: Jwks,
    fetched_at: Instant,
}

/// A discovered provider plus its cached JWKS.
pub struct OidcProvider {
    config: OidcConfig,
    metadata: ProviderMetadata,
    jwks: RwLock<Option<CachedJwks>>,
}

const JWKS_CACHE: Duration = Duration::from_secs(300);

impl fmt::Debug for OidcProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcProvider")
            .field("issuer", &self.config.issuer)
            .finish_non_exhaustive()
    }
}

impl OidcProvider {
    /// Discovers the provider and validates the returned issuer.
    ///
    /// # Errors
    ///
    /// [`OidcError`] for configuration, transport, status, and metadata
    /// problems.
    pub async fn discover(config: OidcConfig) -> Result<Self, OidcError> {
        validate_config(&config)?;
        let mut url = Url::parse(&config.issuer)
            .map_err(|_| OidcError::Configuration("issuer is not a URL"))?;
        url.path_segments_mut()
            .map_err(|()| OidcError::Configuration("issuer cannot be a base URL"))?
            .pop_if_empty()
            .push(".well-known")
            .push("openid-configuration");
        let timeout = config.request_timeout;
        let (metadata, config) = tokio::time::timeout(timeout, async move {
            let response = config
                .http
                .get(url)
                .send()
                .await
                .map_err(|_| OidcError::DiscoveryUnavailable)?;
            let status = response.status();
            if !status.is_success() {
                return Err(OidcError::DiscoveryStatus(status.as_u16()));
            }
            let metadata: ProviderMetadata = response
                .json()
                .await
                .map_err(|_| OidcError::DiscoveryIncomplete("document"))?;
            Ok::<_, OidcError>((metadata, config))
        })
        .await
        .map_err(|_| OidcError::Timeout)??;
        if metadata.issuer != config.issuer {
            return Err(OidcError::IssuerDiscoveryMismatch);
        }
        for (value, name) in [
            (&metadata.authorization_endpoint, "authorization_endpoint"),
            (&metadata.token_endpoint, "token_endpoint"),
            (&metadata.jwks_uri, "jwks_uri"),
        ] {
            let parsed = Url::parse(value).map_err(|_| OidcError::DiscoveryIncomplete(name))?;
            if !acceptable_scheme(&parsed, &config) {
                return Err(OidcError::Configuration("OIDC endpoints must use HTTPS"));
            }
        }
        Ok(Self {
            config,
            metadata,
            jwks: RwLock::new(None),
        })
    }

    /// The configured issuer, exactly as configured.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.config.issuer
    }

    /// Builds the authorization redirect URL.
    ///
    /// # Errors
    ///
    /// [`OidcError::AuthorizeEndpointInvalid`] when the endpoint is malformed.
    pub fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        pkce: &Pkce,
    ) -> Result<Url, OidcError> {
        let mut url = Url::parse(&self.metadata.authorization_endpoint)
            .map_err(|_| OidcError::AuthorizeEndpointInvalid)?;
        let scope = self.scopes();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", self.config.redirect_uri.as_str())
            .append_pair("scope", &scope)
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", pkce.challenge())
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    fn scopes(&self) -> String {
        let mut scopes = vec!["openid".to_owned()];
        for scope in &self.config.scopes {
            if scope != "openid" && !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        scopes.join(" ")
    }

    /// Exchanges an authorization code and verifies the returned ID token.
    ///
    /// # Errors
    ///
    /// [`OidcError`] for transport, status, response, and token-validation
    /// failures. Error text never contains the code, token, or secret.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &Secret,
        nonce: &str,
    ) -> Result<OidcIdentity, OidcError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.config.redirect_uri.as_str()),
            ("code_verifier", verifier.expose()),
        ];
        let request = self
            .config
            .http
            .post(&self.metadata.token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json");
        let request = match (&self.config.token_endpoint_auth, &self.config.client_secret) {
            (TokenEndpointAuth::ClientSecretBasic, Some(secret)) => request
                .basic_auth(
                    form_encode(&self.config.client_id),
                    Some(form_encode(secret.expose())),
                )
                .form(&form),
            (TokenEndpointAuth::ClientSecretPost, Some(secret)) => {
                form.push(("client_id", self.config.client_id.as_str()));
                form.push(("client_secret", secret.expose()));
                request.form(&form)
            }
            (TokenEndpointAuth::None, _) => {
                form.push(("client_id", self.config.client_id.as_str()));
                request.form(&form)
            }
            _ => return Err(OidcError::Configuration("client secret is missing")),
        };
        let token: TokenResponse = tokio::time::timeout(self.config.request_timeout, async {
            let response = request
                .send()
                .await
                .map_err(|_| OidcError::TokenUnavailable)?;
            let status = response.status();
            if !status.is_success() {
                return Err(OidcError::TokenStatus(status.as_u16()));
            }
            response
                .json()
                .await
                .map_err(|_| OidcError::TokenResponseInvalid)
        })
        .await
        .map_err(|_| OidcError::Timeout)??;
        self.verify_id_token_at(&token.id_token, nonce, chrono::Utc::now().timestamp())
            .await
    }

    /// Verifies an ID token at `now` (Unix seconds).
    ///
    /// # Errors
    ///
    /// [`OidcError`] for a malformed token, unknown key, bad signature, or an
    /// invalid claim.
    pub async fn verify_id_token_at(
        &self,
        id_token: &str,
        nonce: &str,
        now: i64,
    ) -> Result<OidcIdentity, OidcError> {
        let (header, claims, signing_input, signature) = decode_jwt(id_token)?;
        let algorithm = algorithm(&header.alg)?;
        let key = self.signing_key(header.kid.as_deref(), &header.alg).await?;
        let components = RsaPublicKeyComponents {
            n: &key.n,
            e: &key.e,
        };
        components
            .verify(algorithm, signing_input, &signature)
            .map_err(|_| OidcError::SignatureInvalid)?;
        validate_claims(&claims, &self.config, nonce, now)
    }

    async fn signing_key(
        &self,
        kid: Option<&str>,
        algorithm: &str,
    ) -> Result<VerificationKey, OidcError> {
        {
            let cached = self.jwks.read().await;
            if let Some(cached) = cached.as_ref()
                && cached.fetched_at.elapsed() < JWKS_CACHE
                && let Some(key) = select_key(&cached.keys, kid, algorithm)
            {
                return Ok(key);
            }
        }
        // Unknown key or stale cache: refresh once.
        let keys = self.fetch_jwks().await?;
        let selected = select_key(&keys, kid, algorithm);
        *self.jwks.write().await = Some(CachedJwks {
            keys,
            fetched_at: Instant::now(),
        });
        selected.ok_or(OidcError::UnknownKey)
    }

    async fn fetch_jwks(&self) -> Result<Jwks, OidcError> {
        tokio::time::timeout(self.config.request_timeout, async {
            let response = self
                .config
                .http
                .get(&self.metadata.jwks_uri)
                .send()
                .await
                .map_err(|_| OidcError::JwksUnavailable)?;
            if !response.status().is_success() {
                return Err(OidcError::JwksUnavailable);
            }
            response
                .json()
                .await
                .map_err(|_| OidcError::JwksUnavailable)
        })
        .await
        .map_err(|_| OidcError::Timeout)?
    }
}

/// RFC 6749 section 2.3.1: `client_id` and `client_secret` are form-encoded
/// before they are joined with `:` and Base64-encoded for HTTP Basic.
fn form_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

fn validate_config(config: &OidcConfig) -> Result<(), OidcError> {
    let issuer =
        Url::parse(&config.issuer).map_err(|_| OidcError::Configuration("issuer is not a URL"))?;
    if !acceptable_scheme(&issuer, config)
        || !acceptable_scheme(&config.redirect_uri, config)
        || config.client_id.is_empty()
        || config.leeway > Duration::from_secs(300)
        || config.request_timeout.is_zero()
        || config.request_timeout > Duration::from_secs(120)
    {
        return Err(OidcError::Configuration(
            "issuer, redirect URI, client ID, or leeway is invalid",
        ));
    }
    match (config.token_endpoint_auth, &config.client_secret) {
        (TokenEndpointAuth::None, Some(_)) => Err(OidcError::Configuration(
            "a public client must not have a secret",
        )),
        (TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost, None) => {
            Err(OidcError::Configuration("client secret is missing"))
        }
        _ => Ok(()),
    }
}

fn acceptable_scheme(url: &Url, config: &OidcConfig) -> bool {
    url.scheme() == "https" || (config.allow_insecure_http && url.scheme() == "http")
}

fn algorithm(name: &str) -> Result<&'static ring::signature::RsaParameters, OidcError> {
    match name {
        "RS256" => Ok(&RSA_PKCS1_2048_8192_SHA256),
        "RS384" => Ok(&RSA_PKCS1_2048_8192_SHA384),
        "RS512" => Ok(&RSA_PKCS1_2048_8192_SHA512),
        _ => Err(OidcError::UnsupportedAlgorithm),
    }
}

type DecodedJwt<'a> = (JwtHeader, Claims, &'a [u8], Vec<u8>);

fn decode_jwt(id_token: &str) -> Result<DecodedJwt<'_>, OidcError> {
    let (signing_input, signature) = id_token
        .rsplit_once('.')
        .ok_or(OidcError::IdTokenMalformed)?;
    let mut parts = signing_input.split('.');
    let (Some(header), Some(claims), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(OidcError::IdTokenMalformed);
    };
    let header: JwtHeader = decode_part(header)?;
    let claims: Claims = decode_part(claims)?;
    let signature = decode_b64(signature)?;
    Ok((header, claims, signing_input.as_bytes(), signature))
}

fn decode_part<T: DeserializeOwned>(part: &str) -> Result<T, OidcError> {
    let bytes = decode_b64(part)?;
    serde_json::from_slice(&bytes).map_err(|_| OidcError::IdTokenMalformed)
}

fn decode_b64(value: &str) -> Result<Vec<u8>, OidcError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
        .map_err(|_| OidcError::IdTokenMalformed)
}

/// A usable RSA verification key.
struct VerificationKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

fn select_key(jwks: &Jwks, kid: Option<&str>, algorithm: &str) -> Option<VerificationKey> {
    let key = jwks.keys.iter().find(|key| {
        key.kty == "RSA"
            && key.usage.as_deref().is_none_or(|usage| usage == "sig")
            && key.alg.as_deref().is_none_or(|value| value == algorithm)
            && kid.is_none_or(|kid| key.kid.as_deref() == Some(kid))
    })?;
    let n = decode_b64(key.n.as_deref()?).ok()?;
    let e = decode_b64(key.e.as_deref()?).ok()?;
    if n.is_empty() || e.is_empty() {
        return None;
    }
    Some(VerificationKey { n, e })
}

/// Validates the claims of a signature-verified token.
///
/// Split from signature checking so the rules are unit-testable without a
/// provider key.
fn validate_claims(
    claims: &Claims,
    config: &OidcConfig,
    nonce: &str,
    now: i64,
) -> Result<OidcIdentity, OidcError> {
    if claims.iss != config.issuer {
        return Err(OidcError::IssuerMismatch);
    }
    if !claims.aud.contains(&config.client_id) {
        return Err(OidcError::AudienceMismatch);
    }
    if claims.aud.is_multiple() && claims.azp.is_none() {
        return Err(OidcError::AuthorizedPartyMismatch);
    }
    if let Some(azp) = claims.azp.as_deref()
        && azp != config.client_id
    {
        return Err(OidcError::AuthorizedPartyMismatch);
    }
    let leeway = i64::try_from(config.leeway.as_secs()).unwrap_or(i64::MAX);
    if now >= claims.exp.saturating_add(leeway) {
        return Err(OidcError::Expired);
    }
    if claims.iat > now.saturating_add(leeway) {
        return Err(OidcError::IssuedInFuture);
    }
    if let Some(nbf) = claims.nbf
        && nbf > now.saturating_add(leeway)
    {
        return Err(OidcError::NotYetValid);
    }
    let nonce_matches = claims
        .nonce
        .as_deref()
        .is_some_and(|value| random::constant_time_eq(value.as_bytes(), nonce.as_bytes()));
    if !nonce_matches {
        return Err(OidcError::NonceMismatch);
    }
    if claims.sub.is_empty() {
        return Err(OidcError::MissingSubject);
    }
    let display_name = claims
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            claims
                .preferred_username
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            claims
                .email
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or(&claims.sub)
        .to_owned();
    Ok(OidcIdentity {
        issuer: claims.iss.clone(),
        subject: claims.sub.clone(),
        display_name,
        email: claims.email.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.example".to_owned(),
            client_id: "bergsmith".to_owned(),
            client_secret: Some(Secret::new("secret")),
            redirect_uri: Url::parse("https://bergsmith.example/auth/callback").unwrap(),
            scopes: Vec::new(),
            token_endpoint_auth: TokenEndpointAuth::ClientSecretBasic,
            allow_insecure_http: false,
            leeway: Duration::from_secs(60),
            request_timeout: Duration::from_secs(10),
            http: reqwest::Client::new(),
        }
    }

    fn claims() -> Claims {
        Claims {
            iss: "https://idp.example".to_owned(),
            sub: "alice".to_owned(),
            aud: Audience::One("bergsmith".to_owned()),
            exp: 1_000,
            iat: 900,
            nbf: None,
            nonce: Some("nonce-value".to_owned()),
            azp: None,
            name: Some("Alice".to_owned()),
            preferred_username: None,
            email: Some("alice@example.com".to_owned()),
        }
    }

    #[test]
    fn valid_claims_produce_the_bound_identity() {
        let identity = validate_claims(&claims(), &config(), "nonce-value", 950).unwrap();
        assert_eq!(identity.issuer, "https://idp.example");
        assert_eq!(identity.subject, "alice");
        assert_eq!(identity.display_name, "Alice");
        assert_eq!(identity.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn issuer_audience_and_nonce_must_match_exactly() {
        let config = config();
        let mut wrong = claims();
        wrong.iss = "https://other.example".to_owned();
        assert!(matches!(
            validate_claims(&wrong, &config, "nonce-value", 950),
            Err(OidcError::IssuerMismatch)
        ));

        let mut wrong = claims();
        wrong.aud = Audience::Many(vec!["other".to_owned()]);
        assert!(matches!(
            validate_claims(&wrong, &config, "nonce-value", 950),
            Err(OidcError::AudienceMismatch)
        ));

        assert!(matches!(
            validate_claims(&claims(), &config, "other-nonce", 950),
            Err(OidcError::NonceMismatch)
        ));
    }

    #[test]
    fn multiple_audiences_require_the_authorized_party() {
        let config = config();
        let mut multiple = claims();
        multiple.aud = Audience::Many(vec!["bergsmith".to_owned(), "other".to_owned()]);
        assert!(matches!(
            validate_claims(&multiple, &config, "nonce-value", 950),
            Err(OidcError::AuthorizedPartyMismatch)
        ));
        multiple.azp = Some("bergsmith".to_owned());
        assert!(validate_claims(&multiple, &config, "nonce-value", 950).is_ok());
        multiple.azp = Some("other".to_owned());
        assert!(matches!(
            validate_claims(&multiple, &config, "nonce-value", 950),
            Err(OidcError::AuthorizedPartyMismatch)
        ));
    }

    #[test]
    fn an_id_token_must_carry_a_numeric_iat() {
        let base = serde_json::json!({
            "iss": "https://idp.example",
            "sub": "alice",
            "aud": "bergsmith",
            "exp": 1_000,
            "nonce": "nonce-value",
        });
        assert!(
            serde_json::from_value::<Claims>(base.clone()).is_err(),
            "missing iat is rejected"
        );
        let mut null = base;
        null["iat"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<Claims>(null).is_err(),
            "null iat is rejected"
        );
    }

    #[test]
    fn expiry_honours_leeway_and_rejects_future_issue_times() {
        let config = config();
        // exp 1000, leeway 60: still valid at 1059, rejected at 1060.
        assert!(validate_claims(&claims(), &config, "nonce-value", 1059).is_ok());
        assert!(matches!(
            validate_claims(&claims(), &config, "nonce-value", 1060),
            Err(OidcError::Expired)
        ));
        let mut future = claims();
        future.iat = 1_011;
        assert!(matches!(
            validate_claims(&future, &config, "nonce-value", 950),
            Err(OidcError::IssuedInFuture)
        ));
    }

    #[test]
    fn a_subject_is_required_and_display_name_falls_back() {
        let config = config();
        let mut missing = claims();
        missing.name = None;
        missing.email = None;
        missing.sub.clear();
        assert!(matches!(
            validate_claims(&missing, &config, "nonce-value", 950),
            Err(OidcError::MissingSubject)
        ));

        let mut fallback = claims();
        fallback.name = None;
        fallback.email = None;
        assert_eq!(
            validate_claims(&fallback, &config, "nonce-value", 950)
                .unwrap()
                .display_name,
            "alice"
        );
    }

    #[test]
    fn basic_client_credentials_are_form_encoded() {
        assert_eq!(form_encode("id +%"), "id+%2B%25");
        assert_eq!(form_encode("s:e c"), "s%3Ae+c");
        assert_eq!(form_encode("plain-1_2.3*"), "plain-1_2.3*");
    }

    #[test]
    fn heterogeneous_jwks_selects_the_rsa_key() {
        let document = serde_json::json!({
            "keys": [
                { "kty": "EC", "kid": "ec", "crv": "P-256", "x": "a", "y": "b" },
                { "kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": "c" },
                { "kty": "RSA", "kid": "rsa", "use": "sig", "alg": "RS256",
                  "n": "AQAB", "e": "AQAB" }
            ]
        });
        let jwks: Jwks = serde_json::from_value(document).expect("mixed keys deserialize");
        assert!(select_key(&jwks, Some("rsa"), "RS256").is_some());
        assert!(select_key(&jwks, Some("ec"), "RS256").is_none());
        assert!(select_key(&jwks, Some("ed"), "RS256").is_none());
    }

    #[test]
    fn only_rsa_pkcs1_algorithms_are_accepted() {
        for accepted in ["RS256", "RS384", "RS512"] {
            assert!(algorithm(accepted).is_ok(), "{accepted}");
        }
        for rejected in ["none", "HS256", "ES256", "PS256", "rs256"] {
            assert!(
                matches!(algorithm(rejected), Err(OidcError::UnsupportedAlgorithm)),
                "{rejected}"
            );
        }
    }

    #[test]
    fn a_malformed_token_is_rejected_before_key_lookup() {
        assert!(matches!(
            decode_jwt("not-a-jwt"),
            Err(OidcError::IdTokenMalformed)
        ));
        assert!(matches!(
            decode_jwt("a.b.c.d"),
            Err(OidcError::IdTokenMalformed)
        ));
        assert!(matches!(
            decode_jwt("!!!.???.###"),
            Err(OidcError::IdTokenMalformed)
        ));
    }

    #[test]
    fn pkce_challenges_are_s256_of_the_verifier() {
        let pkce = Pkce::generate().unwrap();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier().expose().as_bytes()));
        assert_eq!(pkce.challenge(), expected);
        assert_eq!(pkce.verifier().expose().len(), 43);
    }

    #[test]
    fn public_clients_must_not_configure_a_secret() {
        let config = OidcConfig {
            token_endpoint_auth: TokenEndpointAuth::None,
            ..config()
        };
        assert!(matches!(
            validate_config(&config),
            Err(OidcError::Configuration(_))
        ));
    }
}
