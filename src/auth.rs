//! Microsoft Entra authentication, credential storage, and token refresh.

use async_trait::async_trait;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, CsrfToken, DeviceAuthorizationUrl,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope,
    StandardDeviceAuthorizationResponse, TokenResponse, TokenUrl, basic::BasicClient,
};
use secrecy::{ExposeSecret, SecretString};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

const AUTHORITY_ROOT: &str = "https://login.microsoftonline.com";
const CREDENTIAL_SERVICE: &str = "nochange";
const REQUIRED_SCOPES: [&str; 4] = [
    "offline_access",
    "https://graph.microsoft.com/User.Read",
    "https://graph.microsoft.com/Mail.ReadWrite",
    "https://graph.microsoft.com/Mail.Send",
];

/// Return the delegated OAuth scopes required by the initial release.
pub const fn get_required_scopes() -> &'static [&'static str] {
    &REQUIRED_SCOPES
}

/// Validated Microsoft Entra OAuth endpoints for one tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntraEndpoints {
    /// Authorization-code endpoint.
    pub authorization: Url,
    /// Token and device-code polling endpoint.
    pub token: Url,
    /// Device authorization endpoint.
    pub device_authorization: Url,
}

impl EntraEndpoints {
    /// Build commercial-cloud v2 endpoints without permitting URL path injection.
    pub fn build(tenant: &str) -> Result<Self, AuthError> {
        validate_tenant(tenant)?;
        let root = format!("{AUTHORITY_ROOT}/{tenant}/oauth2/v2.0");
        Ok(Self {
            authorization: parse_endpoint(&format!("{root}/authorize"))?,
            token: parse_endpoint(&format!("{root}/token"))?,
            device_authorization: parse_endpoint(&format!("{root}/devicecode"))?,
        })
    }
}

/// Secrets and browser URL associated with one PKCE authorization attempt.
pub struct AuthorizationSession {
    authorization_url: Url,
    redirect_url: Url,
    csrf_state: SecretString,
    pkce_verifier: SecretString,
}

impl AuthorizationSession {
    /// Generate a fresh SHA-256 PKCE challenge and CSRF state.
    pub fn build(
        client_id: &str,
        endpoints: &EntraEndpoints,
        redirect_url: Url,
    ) -> Result<Self, AuthError> {
        if client_id.trim().is_empty() {
            return Err(AuthError::InvalidClientId);
        }
        let auth_url = AuthUrl::new(endpoints.authorization.to_string())
            .map_err(|_| AuthError::InvalidEndpoint)?;
        let redirect_url =
            RedirectUrl::new(redirect_url.to_string()).map_err(|_| AuthError::InvalidEndpoint)?;
        let client = BasicClient::new(ClientId::new(client_id.to_owned()))
            .set_auth_uri(auth_url)
            .set_redirect_uri(redirect_url.clone());
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (authorization_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scopes(
                get_required_scopes()
                    .iter()
                    .map(|scope| Scope::new((*scope).to_owned())),
            )
            .set_pkce_challenge(challenge)
            .url();
        Ok(Self {
            authorization_url,
            redirect_url: redirect_url.url().clone(),
            csrf_state: csrf_state.secret().as_str().into(),
            pkce_verifier: verifier.secret().as_str().into(),
        })
    }

    /// Return the URL that should be opened in the user's browser.
    pub const fn get_authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    /// Open the authorization URL through an injectable browser boundary.
    pub fn open_with(&self, browser: &impl BrowserLauncher) -> Result<(), AuthError> {
        browser.open_url(&self.authorization_url)
    }

    /// Verify that a callback contains the state generated for this attempt.
    pub fn is_valid_state(&self, returned_state: &str) -> bool {
        self.csrf_state.expose_secret() == returned_state
    }

    fn get_pkce_verifier(&self) -> PkceCodeVerifier {
        PkceCodeVerifier::new(self.pkce_verifier.expose_secret().to_owned())
    }
}

/// Boundary for opening an authorization URL in the user's browser.
pub trait BrowserLauncher: Send + Sync {
    /// Open a URL with the platform's configured browser.
    fn open_url(&self, url: &Url) -> Result<(), AuthError>;
}

/// System-browser implementation used by interactive authorization.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBrowserLauncher;

impl BrowserLauncher for SystemBrowserLauncher {
    fn open_url(&self, url: &Url) -> Result<(), AuthError> {
        open::that(url.as_str()).map_err(|_| AuthError::BrowserLauncher)
    }
}

impl fmt::Debug for AuthorizationSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationSession")
            .field("authorization_url", &"[REDACTED]")
            .field("redirect_url", &self.redirect_url)
            .field("csrf_state", &"[REDACTED]")
            .field("pkce_verifier", &"[REDACTED]")
            .finish()
    }
}

/// One-shot localhost listener for an authorization-code browser callback.
pub struct LocalCallbackListener {
    server: tiny_http::Server,
    redirect_url: Url,
}

impl LocalCallbackListener {
    /// Bind an IPv4 localhost callback on a random operating-system port.
    pub fn bind() -> Result<Self, AuthError> {
        let server =
            tiny_http::Server::http("127.0.0.1:0").map_err(|_| AuthError::CallbackServer)?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or(AuthError::CallbackServer)?;
        let redirect_url = Url::parse(&format!("http://localhost:{}/", address.port()))
            .map_err(|_| AuthError::InvalidEndpoint)?;
        Ok(Self {
            server,
            redirect_url,
        })
    }

    /// Return the exact localhost redirect URI for the authorization request.
    pub const fn get_redirect_url(&self) -> &Url {
        &self.redirect_url
    }

    /// Wait for one callback and return its code only after validating CSRF state.
    pub async fn wait_for_code(
        self,
        session: &AuthorizationSession,
        timeout: Duration,
    ) -> Result<SecretString, AuthError> {
        let expected_state = session.csrf_state.clone();
        tokio::task::spawn_blocking(move || self.get_callback_code(&expected_state, timeout))
            .await
            .map_err(|_| AuthError::CallbackServer)?
    }

    fn get_callback_code(
        self,
        expected_state: &SecretString,
        timeout: Duration,
    ) -> Result<SecretString, AuthError> {
        let request = self
            .server
            .recv_timeout(timeout)
            .map_err(|_| AuthError::CallbackServer)?
            .ok_or(AuthError::CallbackTimeout)?;
        let result = parse_callback_target(request.url(), expected_state);
        let (status, message) = if result.is_ok() {
            (200, "Authorization received. You can close this window.")
        } else {
            (
                400,
                "Authorization callback was rejected. Return to Nochange.",
            )
        };
        request
            .respond(tiny_http::Response::from_string(message).with_status_code(status))
            .map_err(|_| AuthError::CallbackServer)?;
        result
    }
}

/// OAuth tokens returned from a successful authorization or refresh.
#[derive(Clone, Debug)]
pub struct TokenGrant {
    /// Short-lived bearer token, retained only in memory.
    pub access_token: SecretString,
    /// Rotated refresh token, when the authorization server supplied one.
    pub refresh_token: Option<SecretString>,
    /// Lifetime reported for the access token.
    pub expires_in: Duration,
}

/// Injectable wall clock for access-token expiry decisions.
pub trait Clock: Send + Sync {
    /// Return the current wall-clock time.
    fn get_now(&self) -> std::time::SystemTime;
}

/// System wall clock used by production authentication.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn get_now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}

/// In-progress device authorization with code-bearing fields redacted from diagnostics.
pub struct DeviceAuthorizationSession {
    response: StandardDeviceAuthorizationResponse,
    verification_uri: Url,
    user_code: SecretString,
}

impl DeviceAuthorizationSession {
    /// Return the Microsoft sign-in page the user should visit.
    pub const fn get_verification_uri(&self) -> &Url {
        &self.verification_uri
    }

    /// Return the short code the user must enter on the verification page.
    pub const fn get_user_code(&self) -> &SecretString {
        &self.user_code
    }
}

impl fmt::Debug for DeviceAuthorizationSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorizationSession")
            .field("verification_uri", &self.verification_uri)
            .field("user_code", &"[REDACTED]")
            .field("device_code", &"[REDACTED]")
            .finish()
    }
}

/// Persistent refresh-token operations.
pub trait CredentialStore: Send + Sync {
    /// Load the refresh token for an account.
    fn get_refresh_token(&self, account: &str) -> Result<Option<SecretString>, AuthError>;

    /// Atomically replace the refresh token for an account.
    fn replace_refresh_token(&self, account: &str, token: &SecretString) -> Result<(), AuthError>;

    /// Delete the refresh token for an account.
    fn delete_refresh_token(&self, account: &str) -> Result<(), AuthError>;
}

/// Platform credential-store implementation backed by the `keyring` crate.
#[derive(Clone, Debug, Default)]
pub struct KeyringCredentialStore;

impl KeyringCredentialStore {
    fn get_entry(account: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(CREDENTIAL_SERVICE, account).map_err(|_| AuthError::CredentialStore)
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get_refresh_token(&self, account: &str) -> Result<Option<SecretString>, AuthError> {
        let entry = Self::get_entry(account)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token.into())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(AuthError::CredentialStore),
        }
    }

    fn replace_refresh_token(&self, account: &str, token: &SecretString) -> Result<(), AuthError> {
        Self::get_entry(account)?
            .set_password(token.expose_secret())
            .map_err(|_| AuthError::CredentialStore)
    }

    fn delete_refresh_token(&self, account: &str) -> Result<(), AuthError> {
        match Self::get_entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AuthError::CredentialStore),
        }
    }
}

/// Boundary for OAuth token exchanges.
#[async_trait]
pub trait OAuthTokenExchange: Send + Sync {
    /// Exchange a stored refresh token for a fresh grant.
    async fn exchange_refresh_token(
        &self,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError>;
}

/// OAuth2-based Microsoft Entra refresh-token exchanger.
#[derive(Clone, Debug)]
pub struct EntraTokenExchange {
    client_id: String,
    token_url: Url,
    device_authorization_url: Url,
    http_client: reqwest::Client,
}

impl EntraTokenExchange {
    /// Create a public-client token exchanger with redirects disabled.
    pub fn build(client_id: &str, endpoints: &EntraEndpoints) -> Result<Self, AuthError> {
        if client_id.trim().is_empty() {
            return Err(AuthError::InvalidClientId);
        }
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AuthError::HttpClient)?;
        Ok(Self {
            client_id: client_id.to_owned(),
            token_url: endpoints.token.clone(),
            device_authorization_url: endpoints.device_authorization.clone(),
            http_client,
        })
    }

    /// Request the user and device codes for an interactive device flow.
    pub async fn start_device_authorization(
        &self,
    ) -> Result<DeviceAuthorizationSession, AuthError> {
        let device_url = DeviceAuthorizationUrl::new(self.device_authorization_url.to_string())
            .map_err(|_| AuthError::InvalidEndpoint)?;
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_type(AuthType::RequestBody)
            .set_device_authorization_url(device_url);
        let response: StandardDeviceAuthorizationResponse = client
            .exchange_device_code()
            .add_scopes(
                get_required_scopes()
                    .iter()
                    .map(|scope| Scope::new((*scope).to_owned())),
            )
            .request_async(&self.http_client)
            .await
            .map_err(|_| AuthError::TokenExchange)?;
        let verification_uri = Url::parse(response.verification_uri().url().as_str())
            .map_err(|_| AuthError::InvalidEndpoint)?;
        let user_code = SecretString::from(response.user_code().secret().as_str());
        Ok(DeviceAuthorizationSession {
            response,
            verification_uri,
            user_code,
        })
    }

    /// Poll Entra until an in-progress device flow succeeds or expires.
    pub async fn poll_device_authorization(
        &self,
        session: DeviceAuthorizationSession,
    ) -> Result<TokenGrant, AuthError> {
        let token_url =
            TokenUrl::new(self.token_url.to_string()).map_err(|_| AuthError::InvalidEndpoint)?;
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_type(AuthType::RequestBody)
            .set_token_uri(token_url);
        let response = client
            .exchange_device_access_token(&session.response)
            .request_async(&self.http_client, tokio::time::sleep, None)
            .await
            .map_err(|_| AuthError::TokenExchange)?;
        Ok(TokenGrant {
            access_token: response.access_token().secret().as_str().into(),
            refresh_token: response
                .refresh_token()
                .map(|token| SecretString::from(token.secret().as_str())),
            expires_in: response.expires_in().unwrap_or(Duration::ZERO),
        })
    }

    /// Exchange a verified browser callback code with its original PKCE verifier.
    pub async fn exchange_authorization_code(
        &self,
        authorization_code: &SecretString,
        session: AuthorizationSession,
    ) -> Result<TokenGrant, AuthError> {
        let token_url =
            TokenUrl::new(self.token_url.to_string()).map_err(|_| AuthError::InvalidEndpoint)?;
        let redirect_url = RedirectUrl::new(session.redirect_url.to_string())
            .map_err(|_| AuthError::InvalidEndpoint)?;
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_type(AuthType::RequestBody)
            .set_token_uri(token_url)
            .set_redirect_uri(redirect_url);
        let response = client
            .exchange_code(AuthorizationCode::new(
                authorization_code.expose_secret().to_owned(),
            ))
            .set_pkce_verifier(session.get_pkce_verifier())
            .request_async(&self.http_client)
            .await
            .map_err(|_| AuthError::TokenExchange)?;
        Ok(TokenGrant {
            access_token: response.access_token().secret().as_str().into(),
            refresh_token: response
                .refresh_token()
                .map(|token| SecretString::from(token.secret().as_str())),
            expires_in: response.expires_in().unwrap_or(Duration::ZERO),
        })
    }
}

#[async_trait]
impl OAuthTokenExchange for EntraTokenExchange {
    async fn exchange_refresh_token(
        &self,
        refresh_token: &SecretString,
    ) -> Result<TokenGrant, AuthError> {
        let token_url =
            TokenUrl::new(self.token_url.to_string()).map_err(|_| AuthError::InvalidEndpoint)?;
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_type(AuthType::RequestBody)
            .set_token_uri(token_url);
        let refresh_token = RefreshToken::new(refresh_token.expose_secret().to_owned());
        let response = client
            .exchange_refresh_token(&refresh_token)
            .add_scopes(
                get_required_scopes()
                    .iter()
                    .map(|scope| Scope::new((*scope).to_owned())),
            )
            .request_async(&self.http_client)
            .await
            .map_err(|_| AuthError::TokenExchange)?;
        Ok(TokenGrant {
            access_token: response.access_token().secret().as_str().into(),
            refresh_token: response
                .refresh_token()
                .map(|token| SecretString::from(token.secret().as_str())),
            expires_in: response.expires_in().unwrap_or(Duration::ZERO),
        })
    }
}

/// Access-token boundary used by authenticated Graph requests.
#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    /// Return a cached token or force one refresh for a replay after `401`.
    async fn get_access_token(&self, force_refresh: bool) -> Result<SecretString, AuthError>;
}

/// Serializes token refresh, caches access tokens, and persists rotations.
pub struct TokenManager<C, E> {
    account: String,
    credentials: Arc<C>,
    exchange: Arc<E>,
    clock: Arc<dyn Clock>,
    cached_access_token: Mutex<Option<CachedAccessToken>>,
}

struct CachedAccessToken {
    token: SecretString,
    expires_at: std::time::SystemTime,
}

impl<C, E> TokenManager<C, E> {
    /// Construct a token manager for one configured account.
    pub fn new(account: impl Into<String>, credentials: Arc<C>, exchange: Arc<E>) -> Self {
        Self::new_with_clock(account, credentials, exchange, Arc::new(SystemClock))
    }

    /// Construct a token manager with a deterministic clock.
    pub fn new_with_clock<K>(
        account: impl Into<String>,
        credentials: Arc<C>,
        exchange: Arc<E>,
        clock: Arc<K>,
    ) -> Self
    where
        K: Clock + 'static,
    {
        Self {
            account: account.into(),
            credentials,
            exchange,
            clock,
            cached_access_token: Mutex::new(None),
        }
    }
}

impl<C, E> TokenManager<C, E>
where
    C: CredentialStore,
    E: OAuthTokenExchange,
{
    /// Return a cached access token, refreshing and rotating credentials as needed.
    pub async fn get_access_token(&self, force_refresh: bool) -> Result<SecretString, AuthError> {
        let mut cached = self.cached_access_token.lock().await;
        let now = self.clock.get_now();
        if !force_refresh
            && let Some(access_token) = cached.as_ref()
            && access_token.expires_at > now
        {
            return Ok(access_token.token.clone());
        }
        let refresh_token = self
            .credentials
            .get_refresh_token(&self.account)?
            .ok_or_else(|| AuthError::MissingCredentials {
                account: self.account.clone(),
            })?;
        let grant = self.exchange.exchange_refresh_token(&refresh_token).await?;
        if let Some(rotated_refresh_token) = grant.refresh_token.as_ref() {
            self.credentials
                .replace_refresh_token(&self.account, rotated_refresh_token)?;
        }
        let issued_at = self.clock.get_now();
        let expires_at = issued_at.checked_add(grant.expires_in).unwrap_or(issued_at);
        *cached = Some(CachedAccessToken {
            token: grant.access_token.clone(),
            expires_at,
        });
        Ok(grant.access_token)
    }
}

#[async_trait]
impl<C, E> AccessTokenProvider for TokenManager<C, E>
where
    C: CredentialStore,
    E: OAuthTokenExchange,
{
    async fn get_access_token(&self, force_refresh: bool) -> Result<SecretString, AuthError> {
        Self::get_access_token(self, force_refresh).await
    }
}

/// Authentication and credential failures with no secret-bearing payloads.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    /// Tenant values may not alter the fixed commercial authority URL.
    #[error("invalid Microsoft Entra tenant '{0}'")]
    InvalidTenant(String),
    /// An internally constructed OAuth endpoint was invalid.
    #[error("could not construct a Microsoft Entra OAuth endpoint")]
    InvalidEndpoint,
    /// The public-client identifier was empty.
    #[error("Microsoft Entra client ID cannot be empty")]
    InvalidClientId,
    /// The platform credential store is unavailable or rejected an operation.
    #[error("the operating system credential store is unavailable")]
    CredentialStore,
    /// No refresh token has been stored for the requested account.
    #[error("account '{account}' has no stored credentials; run 'nochange init'")]
    MissingCredentials {
        /// Account requiring initialization.
        account: String,
    },
    /// Microsoft Entra rejected or could not complete a token exchange.
    #[error("Microsoft Entra token exchange failed")]
    TokenExchange,
    /// The hardened OAuth HTTP client could not be created.
    #[error("could not create the OAuth HTTP client")]
    HttpClient,
    /// The localhost OAuth callback listener failed.
    #[error("localhost OAuth callback listener failed")]
    CallbackServer,
    /// The platform could not open the authorization URL.
    #[error("could not open the Microsoft Entra authorization page in a browser")]
    BrowserLauncher,
    /// No OAuth callback arrived before the interactive login timeout.
    #[error("timed out waiting for the localhost OAuth callback")]
    CallbackTimeout,
    /// The OAuth callback was malformed or failed CSRF validation.
    #[error("the OAuth callback was invalid")]
    InvalidCallback,
    /// The user or Microsoft Entra denied the authorization request.
    #[error("Microsoft Entra authorization was denied")]
    AuthorizationDenied,
}

fn validate_tenant(tenant: &str) -> Result<(), AuthError> {
    let valid = !tenant.is_empty()
        && !matches!(tenant, "." | "..")
        && tenant
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'));
    if valid {
        Ok(())
    } else {
        Err(AuthError::InvalidTenant(tenant.to_owned()))
    }
}

fn parse_endpoint(endpoint: &str) -> Result<Url, AuthError> {
    Url::parse(endpoint).map_err(|_| AuthError::InvalidEndpoint)
}

fn parse_callback_target(
    request_target: &str,
    expected_state: &SecretString,
) -> Result<SecretString, AuthError> {
    let callback = Url::parse(&format!("http://localhost{request_target}"))
        .map_err(|_| AuthError::InvalidCallback)?;
    if callback.path() != "/" || callback.fragment().is_some() {
        return Err(AuthError::InvalidCallback);
    }
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut denied = false;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" => denied = true,
            "code" | "state" => return Err(AuthError::InvalidCallback),
            _ => {}
        }
    }
    if denied {
        return Err(AuthError::AuthorizationDenied);
    }
    let state = state.ok_or(AuthError::InvalidCallback)?;
    if state != expected_state.expose_secret() {
        return Err(AuthError::InvalidCallback);
    }
    code.filter(|value| !value.is_empty())
        .map(SecretString::from)
        .ok_or(AuthError::InvalidCallback)
}

#[cfg(test)]
mod tests {
    use super::{AuthError, parse_callback_target};
    use secrecy::{ExposeSecret, SecretString};

    #[test]
    fn accepts_ignored_callback_parameters_without_weakening_required_values() {
        let result = parse_callback_target(
            "/?ignored=value&code=accepted-code&state=expected-state",
            &SecretString::from("expected-state"),
        )
        .expect("complete callback should be accepted");

        assert_eq!(result.expose_secret(), "accepted-code");
    }

    #[test]
    fn rejects_denials_duplicate_values_and_non_root_callbacks() {
        let expected_state = SecretString::from("expected-state");
        for target in [
            "/?error=access_denied&state=expected-state",
            "/?code=one&code=two&state=expected-state",
            "/?code=one&state=expected-state&state=duplicate",
            "/other?code=one&state=expected-state",
            "/?code=&state=expected-state",
            "/?code=one",
        ] {
            assert!(parse_callback_target(target, &expected_state).is_err());
        }

        assert!(matches!(
            parse_callback_target(
                "/?error=access_denied&state=expected-state",
                &expected_state,
            ),
            Err(AuthError::AuthorizationDenied)
        ));
    }
}
